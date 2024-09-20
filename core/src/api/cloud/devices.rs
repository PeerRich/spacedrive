use crate::api::{Ctx, R};

use sd_cloud_schema::{
	auth::AccessToken,
	devices::{
		self,
		register::{Request, RequestUpdate, Response, State},
		DeviceOS, HardwareModel, PubId,
	},
	opaque_ke::{
		ClientLogin, ClientLoginFinishParameters, ClientLoginFinishResult, ClientLoginStartResult,
		ClientRegistration, ClientRegistrationFinishParameters, ClientRegistrationFinishResult,
		ClientRegistrationStartResult,
	},
	Client, Service, SpacedriveCipherSuite,
};
use sd_core_cloud_services::{NodeId, QuinnConnection};
use sd_crypto::{cloud::secret_key::SecretKey, CryptoRng};

use blake3::Hash;
use chrono::DateTime;
use futures::{SinkExt, StreamExt};
use rspc::alpha::AlphaRouter;
use tracing::{debug, error};

use super::{handle_comm_error, try_get_cloud_services_client};

#[derive(Debug, serde::Serialize, serde::Deserialize, specta::Type)]
struct MockDevice {
	pub_id: PubId,
	name: String,
	os: DeviceOS,
	used_storage: u64,
	storage_size: u64,
	created_at: DateTime<chrono::Utc>,
	updated_at: DateTime<chrono::Utc>,
	device_model: HardwareModel,
}

pub fn mount() -> AlphaRouter<Ctx> {
	R.router()
		.procedure("get", {
			R.query(|node, req: devices::get::Request| async move {
				let devices::get::Response(device) = super::handle_comm_error(
					try_get_cloud_services_client(&node)
						.await?
						.devices()
						.get(req)
						.await,
					"Failed to get device;",
				)??;

				debug!(?device, "Got device");

				Ok(device)
			})
		})
		.procedure("list", {
			R.query(|node, req: devices::list::Request| async move {
				let devices::list::Response(mut devices) = super::handle_comm_error(
					try_get_cloud_services_client(&node)
						.await?
						.devices()
						.list(req)
						.await,
					"Failed to list devices;",
				)??;

				debug!(?devices, "Listed devices");

				let id = node.config.get().await.id.into();
				// Filter out the local device by matching pub_id
				devices.retain(|device| device.pub_id != id);

				Ok(devices)
			})
		})
		.procedure("delete", {
			R.mutation(|node, req: devices::delete::Request| async move {
				super::handle_comm_error(
					try_get_cloud_services_client(&node)
						.await?
						.devices()
						.delete(req)
						.await,
					"Failed to delete device;",
				)??;

				debug!("Deleted device");

				Ok(())
			})
		})
		.procedure("update", {
			R.mutation(|node, req: devices::update::Request| async move {
				super::handle_comm_error(
					try_get_cloud_services_client(&node)
						.await?
						.devices()
						.update(req)
						.await,
					"Failed to update device;",
				)??;

				debug!("Updated device");

				Ok(())
			})
		})
}

pub async fn hello(
	client: &Client<QuinnConnection<Service>, Service>,
	access_token: AccessToken,
	device_pub_id: PubId,
	hashed_pub_id: Hash,
	rng: &mut CryptoRng,
) -> Result<SecretKey, rspc::Error> {
	use devices::hello::{Request, RequestUpdate, Response, State};

	let ClientLoginStartResult { message, state } =
		ClientLogin::<SpacedriveCipherSuite>::start(rng, hashed_pub_id.as_bytes().as_slice())
			.map_err(|e| {
				error!(?e, "OPAQUE error initializing device hello request;");
				rspc::Error::new(
					rspc::ErrorCode::InternalServerError,
					"Failed to initialize device login".into(),
				)
			})?;

	let (mut hello_continuation, mut res_stream) = handle_comm_error(
		client
			.devices()
			.hello(Request {
				access_token,
				pub_id: device_pub_id,
				opaque_login_message: Box::new(message),
			})
			.await,
		"Failed to send device hello request;",
	)?;

	let Some(res) = res_stream.next().await else {
		let message = "Server did not send a device hello response;";
		error!("{message}");
		return Err(rspc::Error::new(
			rspc::ErrorCode::InternalServerError,
			message.to_string(),
		));
	};

	let credential_response =
		match handle_comm_error(res, "Communication error on device hello response;")? {
			Ok(Response(State::LoginResponse(credential_response))) => credential_response,
			Ok(Response(State::End)) => {
				unreachable!("Device hello response MUST not be End here, this is a serious bug and should crash;");
			}
			Err(e) => {
				error!(?e, "Device hello response error;");
				return Err(e.into());
			}
		};

	let ClientLoginFinishResult {
		message,
		export_key,
		..
	} = state
		.finish(
			hashed_pub_id.as_bytes().as_slice(),
			*credential_response,
			ClientLoginFinishParameters::default(),
		)
		.map_err(|e| {
			error!(?e, "Device hello finish error;");
			rspc::Error::new(
				rspc::ErrorCode::InternalServerError,
				"Failed to finish device login".into(),
			)
		})?;

	hello_continuation
		.send(RequestUpdate {
			opaque_login_finish: Box::new(message),
		})
		.await
		.map_err(|e| {
			error!(?e, "Failed to send device hello request continuation;");
			rspc::Error::new(
				rspc::ErrorCode::InternalServerError,
				"Failed to finish device login procedure;".into(),
			)
		})?;

	let Some(res) = res_stream.next().await else {
		let message = "Server did not send a device hello END response;";
		error!("{message}");
		return Err(rspc::Error::new(
			rspc::ErrorCode::InternalServerError,
			message.to_string(),
		));
	};

	match handle_comm_error(res, "Communication error on device hello response;")? {
		Ok(Response(State::LoginResponse(_))) => {
			unreachable!("Device hello final response MUST be End here, this is a serious bug and should crash;");
		}
		Ok(Response(State::End)) => {
			// Protocol completed successfully
			Ok(SecretKey::new(export_key.as_slice().try_into().expect(
				"Key mismatch between OPAQUE and crypto crate; this is a serious bug and should crash;",
			)))
		}
		Err(e) => {
			error!(?e, "Device hello final response error;");
			Err(e.into())
		}
	}
}

pub struct DeviceRegisterData {
	pub pub_id: PubId,
	pub name: String,
	pub os: DeviceOS,
	pub hardware_model: HardwareModel,
	pub storage_size: u64,
	pub used_storage: u64,
	pub connection_id: NodeId,
}

pub async fn register(
	client: &Client<QuinnConnection<Service>, Service>,
	access_token: AccessToken,
	DeviceRegisterData {
		pub_id,
		name,
		os,
		hardware_model,
		storage_size,
		used_storage,
		connection_id,
	}: DeviceRegisterData,
	hashed_pub_id: Hash,
	rng: &mut CryptoRng,
) -> Result<SecretKey, rspc::Error> {
	let ClientRegistrationStartResult { message, state } =
		ClientRegistration::<SpacedriveCipherSuite>::start(
			rng,
			hashed_pub_id.as_bytes().as_slice(),
		)
		.map_err(|e| {
			error!(?e, "OPAQUE error initializing device register request;");
			rspc::Error::new(
				rspc::ErrorCode::InternalServerError,
				"Failed to initialize device register".into(),
			)
		})?;

	let (mut register_continuation, mut res_stream) = handle_comm_error(
		client
			.devices()
			.register(Request {
				access_token,
				pub_id,
				name,
				os,
				storage_size,
				connection_id,
				opaque_register_message: Box::new(message),
				hardware_model,
				used_storage,
			})
			.await,
		"Failed to send device register request;",
	)?;

	let Some(res) = res_stream.next().await else {
		let message = "Server did not send a device register response;";
		error!("{message}");
		return Err(rspc::Error::new(
			rspc::ErrorCode::InternalServerError,
			message.to_string(),
		));
	};

	let registration_response =
		match handle_comm_error(res, "Communication error on device register response;")? {
			Ok(Response(State::RegistrationResponse(res))) => res,
			Ok(Response(State::End)) => {
				unreachable!("Device hello response MUST not be End here, this is a serious bug and should crash;");
			}
			Err(e) => {
				error!(?e, "Device hello response error;");
				return Err(e.into());
			}
		};

	let ClientRegistrationFinishResult {
		message,
		export_key,
		..
	} = state
		.finish(
			rng,
			hashed_pub_id.as_bytes().as_slice(),
			*registration_response,
			ClientRegistrationFinishParameters::default(),
		)
		.map_err(|e| {
			error!(?e, "Device register finish error;");
			rspc::Error::new(
				rspc::ErrorCode::InternalServerError,
				"Failed to finish device register".into(),
			)
		})?;

	register_continuation
		.send(RequestUpdate {
			opaque_registration_finish: Box::new(message),
		})
		.await
		.map_err(|e| {
			error!(?e, "Failed to send device register request continuation;");
			rspc::Error::new(
				rspc::ErrorCode::InternalServerError,
				"Failed to finish device register procedure;".into(),
			)
		})?;

	let Some(res) = res_stream.next().await else {
		let message = "Server did not send a device register END response;";
		error!("{message}");
		return Err(rspc::Error::new(
			rspc::ErrorCode::InternalServerError,
			message.to_string(),
		));
	};

	match handle_comm_error(res, "Communication error on device register response;")? {
		Ok(Response(State::RegistrationResponse(_))) => {
			unreachable!("Device register final response MUST be End here, this is a serious bug and should crash;");
		}
		Ok(Response(State::End)) => {
			// Protocol completed successfully
			Ok(SecretKey::from(export_key))
		}
		Err(e) => {
			error!(?e, "Device register final response error;");
			Err(e.into())
		}
	}
}