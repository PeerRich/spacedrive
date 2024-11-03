use super::error::VolumeError;
use sd_prisma::prisma::{
	device,
	volume::{self, read_only},
	PrismaClient,
};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use specta::Type;
use std::path::Path;
use std::path::PathBuf;
use strum_macros::Display;
use uuid::Uuid;

/// Events emitted by the Volume Manager when volume state changes
#[derive(Debug, Clone, Type, Deserialize, Serialize)]
pub enum VolumeEvent {
	/// Emitted when a new volume is discovered and added
	VolumeAdded(Volume),
	/// Emitted when a volume is removed from the system
	VolumeRemoved(Volume),
	/// Emitted when a volume's properties are updated
	VolumeUpdated { old: Volume, new: Volume },
	/// Emitted when a volume's speed test completes
	VolumeSpeedTested {
		id: Vec<u8>,
		read_speed: u64,
		write_speed: u64,
	},
	/// Emitted when a volume's mount status changes
	VolumeMountChanged { id: Vec<u8>, is_mounted: bool },
	/// Emitted when a volume encounters an error
	VolumeError { id: Vec<u8>, error: String },
}

/// Represents a physical or virtual storage volume in the system
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct Volume {
	/// Database ID (None if not yet committed to database)
	pub id: Option<i32>,
	/// Unique public identifier (None if not yet committed)
	pub pub_id: Option<Vec<u8>>,
	/// Database ID of the device this volume is attached to, if any
	pub device_id: Option<i32>,
	/// Human-readable volume name
	pub name: String,
	/// Type of mount (system, external, etc)
	pub mount_type: MountType,
	/// Path where the volume is mounted
	#[specta(type = Vec<String>)]
	pub mount_point: PathBuf,
	/// for APFS volumes like Macintosh HD, additional mount points are returned
	#[specta(type = Vec<String>)]
	pub mount_points: Vec<PathBuf>,
	/// Whether the volume is currently mounted
	pub is_mounted: bool,
	/// Type of storage device (SSD, HDD, etc)
	pub disk_type: DiskType,
	/// Filesystem type (NTFS, EXT4, etc)
	pub file_system: FileSystem,
	/// Whether the volume is mounted read-only
	pub read_only: bool,
	/// Current error status if any
	pub error_status: Option<String>,

	// Performance metrics
	/// Read speed in megabytes per second
	pub read_speed_mbps: Option<u64>,
	/// Write speed in megabytes per second
	pub write_speed_mbps: Option<u64>,
	/// Total storage capacity in bytes
	#[specta(type = String)]
	#[serde_as(as = "DisplayFromStr")]
	pub total_bytes_capacity: u64,
	/// Available storage space in bytes
	#[specta(type = String)]
	#[serde_as(as = "DisplayFromStr")]
	pub total_bytes_available: u64,
}

impl PartialEq for Volume {
	fn eq(&self, other: &Self) -> bool {
		self.name == other.name
            && self.disk_type == other.disk_type
            && self.file_system == other.file_system
            // Check if any mount points overlap
            && (self.mount_points.iter().any(|mp| other.mount_points.contains(mp))
                || other.mount_points.iter().any(|mp| self.mount_points.contains(mp)))
	}
}

impl Eq for Volume {}

impl From<volume::Data> for Volume {
	fn from(vol: volume::Data) -> Self {
		Volume {
			id: Some(vol.id),
			pub_id: Some(vol.pub_id),
			device_id: vol.device_id,
			name: vol.name.unwrap_or_else(|| "Unknown".to_string()),
			mount_type: vol
				.mount_type
				.as_deref()
				.map(MountType::from_string)
				.unwrap_or(MountType::System),
			mount_point: PathBuf::from(vol.mount_point.unwrap_or_else(|| "/".to_string())),
			mount_points: Vec::new(),
			is_mounted: vol.is_mounted.unwrap_or(false),
			disk_type: vol
				.disk_type
				.as_deref()
				.map(DiskType::from_string)
				.unwrap_or(DiskType::Unknown),
			file_system: vol
				.file_system
				.as_deref()
				.map(FileSystem::from_string)
				.unwrap_or_else(|| FileSystem::Other("Unknown".to_string())),
			read_only: vol.read_only.unwrap_or(false),
			error_status: vol.error_status,
			total_bytes_capacity: vol
				.total_bytes_capacity
				.and_then(|t| t.parse().ok())
				.unwrap_or(0),
			total_bytes_available: vol
				.total_bytes_available
				.and_then(|a| a.parse().ok())
				.unwrap_or(0),
			read_speed_mbps: vol.read_speed_mbps.map(|s| s as u64),
			write_speed_mbps: vol.write_speed_mbps.map(|s| s as u64),
		}
	}
}

impl Volume {
	/// Creates a new Volume instance from detected system volume information
	pub fn new(
		name: String,
		mount_type: MountType,
		mount_point: PathBuf,
		mount_points: Vec<PathBuf>,
		disk_type: DiskType,
		file_system: FileSystem,
		total_bytes_capacity: u64,
		total_bytes_available: u64,
		read_only: bool,
	) -> Self {
		Self {
			id: None,
			pub_id: None,
			device_id: None,
			name,
			mount_type,
			mount_point,
			mount_points,
			is_mounted: true,
			disk_type,
			file_system,
			read_only,
			error_status: None,
			read_speed_mbps: None,
			write_speed_mbps: None,
			total_bytes_capacity,
			total_bytes_available,
		}
	}
	/// Generate a unique fingerprint for a volume that will be consistent across detections
	pub fn generate_fingerprint(&self, current_device_pub_id: Vec<u8>) -> Vec<u8> {
		let mut hasher = blake3::Hasher::new();

		// Add hardware-specific identifiers that won't change between reboots
		for id in current_device_pub_id {
			hasher.update(&[id]);
		}

		// Add all mount points to make fingerprint unique
		for mount_point in &self.mount_points {
			hasher.update(mount_point.to_string_lossy().as_bytes());
		}

		// hasher.update(self.name.as_bytes());
		// hasher.update(&self.total_bytes_capacity.to_be_bytes());
		hasher.update(self.file_system.to_string().as_bytes());

		hasher.finalize().as_bytes().to_vec()
	}

	/// Creates a hex string representation of the fingerprint
	pub fn fingerprint_hex(&self, current_device_pub_id: Vec<u8>) -> String {
		hex::encode(self.generate_fingerprint(current_device_pub_id))
	}

	/// Check if a path is under any of this volume's mount points
	pub fn contains_path(&self, path: &Path) -> bool {
		self.mount_points.iter().any(|mp| path.starts_with(mp))
	}

	/// Get the preferred mount point for writing (non-read-only if available)
	// pub fn writable_mount_point(&self) -> Option<&PathBuf> {
	// 	if !self.read_only {
	// 		Some(&self.mount_point)
	// 	} else {
	// 		// Try to find a non-read-only mount point
	// 		self.mount_points
	// 			.iter()
	// 			.find(|mp| !is_volume_readonly(mp).unwrap_or(true))
	// 	}
	// }

	/// Merge system detected volume with database volume, preferring system values for hardware info
	pub fn merge_with_db_volume(system_volume: &Volume, db_volume: &Volume) -> Volume {
		Volume {
			// Keep system-detected hardware properties
			mount_point: system_volume.mount_point.clone(),
			mount_points: system_volume.mount_points.clone(),
			total_bytes_capacity: system_volume.total_bytes_capacity,
			total_bytes_available: system_volume.total_bytes_available,
			disk_type: system_volume.disk_type.clone(),
			file_system: system_volume.file_system.clone(),
			mount_type: system_volume.mount_type.clone(),
			is_mounted: system_volume.is_mounted,

			// Keep database-tracked properties and metadata
			id: db_volume.id,
			device_id: db_volume.device_id,
			pub_id: db_volume.pub_id.clone(),
			name: db_volume.name.clone(),
			read_only: db_volume.read_only,
			error_status: db_volume.error_status.clone(),
			read_speed_mbps: db_volume.read_speed_mbps,
			write_speed_mbps: db_volume.write_speed_mbps,
		}
	}

	/// Check if two volumes represent the same physical device
	// pub fn is_same_device(&self, other: &Volume) -> bool {
	// 	self.generate_fingerprint() == other.generate_fingerprint()
	// }

	pub fn is_volume_tracked(&self) -> bool {
		self.pub_id.is_some()
	}

	/// Creates a new volume record in the database
	pub async fn create(
		&self,
		db: &PrismaClient,
		device_pub_id: Vec<u8>,
	) -> Result<(), VolumeError> {
		let pub_id = Uuid::now_v7().as_bytes().to_vec();

		let device_id = db
			.device()
			.find_unique(device::pub_id::equals(device_pub_id.clone()))
			.select(device::select!({ id }))
			.exec()
			.await?
			.ok_or(VolumeError::DeviceNotFound(device_pub_id))?
			.id;

		db.volume()
			.create(
				pub_id,
				vec![
					volume::name::set(Some(self.name.clone())),
					volume::mount_type::set(Some(self.mount_type.to_string())),
					volume::mount_point::set(Some(self.mount_point.to_str().unwrap().to_string())),
					volume::is_mounted::set(Some(self.is_mounted)),
					volume::disk_type::set(Some(self.disk_type.to_string())),
					volume::file_system::set(Some(self.file_system.to_string())),
					volume::read_only::set(Some(self.read_only)),
					volume::error_status::set(self.error_status.clone()),
					volume::total_bytes_capacity::set(Some(self.total_bytes_capacity.to_string())),
					volume::total_bytes_available::set(Some(
						self.total_bytes_available.to_string(),
					)),
					volume::read_speed_mbps::set(
						self.read_speed_mbps.filter(|&v| v != 0).map(|v| v as i64),
					),
					volume::write_speed_mbps::set(
						self.write_speed_mbps.filter(|&v| v != 0).map(|v| v as i64),
					),
					volume::device_id::set(Some(device_id)),
				],
			)
			.exec()
			.await?;
		Ok(())
	}

	/// Updates an existing volume record in the database
	pub async fn update(&self, db: &PrismaClient) -> Result<(), VolumeError> {
		let id = self.id.ok_or(VolumeError::NotInDatabase)?;

		db.volume()
			.update(
				volume::id::equals(id),
				vec![
					volume::name::set(Some(self.name.clone())),
					volume::mount_type::set(Some(self.mount_type.to_string())),
					volume::mount_point::set(Some(self.mount_point.to_str().unwrap().to_string())),
					volume::is_mounted::set(Some(self.is_mounted)),
					volume::disk_type::set(Some(self.disk_type.to_string())),
					volume::file_system::set(Some(self.file_system.to_string())),
					volume::read_only::set(Some(self.read_only)),
					volume::error_status::set(self.error_status.clone()),
					volume::total_bytes_capacity::set(Some(self.total_bytes_capacity.to_string())),
					volume::total_bytes_available::set(Some(
						self.total_bytes_available.to_string(),
					)),
					volume::read_speed_mbps::set(
						self.read_speed_mbps.filter(|&v| v != 0).map(|v| v as i64),
					),
					volume::write_speed_mbps::set(
						self.write_speed_mbps.filter(|&v| v != 0).map(|v| v as i64),
					),
				],
			)
			.exec()
			.await?;
		Ok(())
	}
}

/// Represents the type of physical storage device
#[derive(Serialize, Deserialize, Debug, Clone, Type, Hash, PartialEq, Eq, Display)]
#[allow(clippy::upper_case_acronyms)]
pub enum DiskType {
	/// Solid State Drive
	SSD,
	/// Hard Disk Drive
	HDD,
	/// Unknown or virtual disk type
	Unknown,
}

impl DiskType {
	pub fn from_string(disk_type: &str) -> Self {
		match disk_type.to_uppercase().as_str() {
			"SSD" => Self::SSD,
			"HDD" => Self::HDD,
			_ => Self::Unknown,
		}
	}
}

/// Represents the filesystem type of the volume
#[derive(Serialize, Deserialize, Debug, Clone, Type, Hash, PartialEq, Eq, Display)]
pub enum FileSystem {
	/// Windows NTFS filesystem
	NTFS,
	/// FAT32 filesystem
	FAT32,
	/// Linux EXT4 filesystem
	EXT4,
	/// Apple APFS filesystem
	APFS,
	/// ExFAT filesystem
	ExFAT,
	/// Other/unknown filesystem type
	Other(String),
}

impl FileSystem {
	pub fn from_string(fs: &str) -> Self {
		match fs.to_uppercase().as_str() {
			"NTFS" => FileSystem::NTFS,
			"FAT32" => FileSystem::FAT32,
			"EXT4" => FileSystem::EXT4,
			"APFS" => FileSystem::APFS,
			"EXFAT" => FileSystem::ExFAT,
			other => FileSystem::Other(other.to_string()),
		}
	}
}

/// Represents how the volume is mounted in the system
#[derive(Serialize, Deserialize, Debug, Clone, Type, Hash, PartialEq, Eq, Display)]
pub enum MountType {
	/// System/boot volume
	System,
	/// External/removable volume
	External,
	/// Network-attached volume
	Network,
	/// Virtual/container volume
	Virtual,
}

impl MountType {
	pub fn from_string(mount_type: &str) -> Self {
		match mount_type.to_uppercase().as_str() {
			"SYSTEM" => Self::System,
			"EXTERNAL" => Self::External,
			"NETWORK" => Self::Network,
			"VIRTUAL" => Self::Virtual,
			_ => Self::System,
		}
	}
}

/// Configuration options for volume operations
#[derive(Debug, Clone)]
pub struct VolumeOptions {
	/// Whether to include system volumes
	pub include_system: bool,
	/// Whether to include virtual volumes
	pub include_virtual: bool,
	/// Whether to run speed tests on discovery
	pub run_speed_test: bool,
	/// Maximum concurrent speed tests
	pub max_concurrent_speed_tests: usize,
}

impl Default for VolumeOptions {
	fn default() -> Self {
		Self {
			include_system: true,
			include_virtual: false,
			run_speed_test: true,
			max_concurrent_speed_tests: 2,
		}
	}
}