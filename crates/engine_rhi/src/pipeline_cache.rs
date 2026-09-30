//! The persistent pipeline cache's file (ADR 0055).
//!
//! The driver's pipeline cache data goes to disk inside the engine's own envelope. The
//! envelope names the GPU and driver it came from and checksums the data: a file from
//! another GPU, another driver, a damaged or foreign file is never handed to the driver,
//! whose own checks vary and have crashed on bad data in the past.
//!
//! Layout (little endian):
//!
//! | offset | bytes | field |
//! |---:|---:|---|
//! | 0 | 4 | magic `OFPC` |
//! | 4 | 4 | envelope version (1) |
//! | 8 | 4 | vendor id |
//! | 12 | 4 | device id |
//! | 16 | 4 | driver version |
//! | 20 | 16 | pipeline cache UUID |
//! | 36 | 8 | data length |
//! | 44 | 8 | data hash (`engine_core::hash::state_hash64`) |
//! | 52 | n | the driver's data (`vkGetPipelineCacheData`) |
//!
//! The driver's data starts with its own header (`VkPipelineCacheHeaderVersionOne`), which
//! is checked against the device too.

use crate::adapter::Adapter;
use engine_core::hash::state_hash64;

/// First bytes of a cache file.
const MAGIC: [u8; 4] = *b"OFPC";
/// Version of the envelope.
const VERSION: u32 = 1;
/// Bytes before the driver's data.
pub(crate) const HEADER_BYTES: usize = 52;
/// `VK_PIPELINE_CACHE_HEADER_VERSION_ONE`.
const DRIVER_HEADER_VERSION_ONE: u32 = 1;
/// Size of `VkPipelineCacheHeaderVersionOne`.
const DRIVER_HEADER_BYTES: usize = 32;

/// What loading the cache file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineCacheLoad {
    /// The cache holds the file's data: `bytes` of the driver's data.
    Loaded {
        /// Size of the driver's data.
        bytes: usize,
    },
    /// There was no file: the cache starts empty (the first run).
    Missing,
    /// The file was not used and the cache starts empty; the reason, for the log: another
    /// GPU or driver (after a driver update), a damaged or foreign file.
    Rejected(String),
}

/// What saving the cache did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineCacheSave {
    /// The file was written: `bytes` in all.
    Written {
        /// Size of the file.
        bytes: usize,
    },
    /// Nothing new since the file was loaded or last written; it was left alone.
    Unchanged,
}

/// The device a cache belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheIdentity {
    pub vendor_id: u32,
    pub device_id: u32,
    pub driver_version: u32,
    pub uuid: [u8; 16],
}

impl CacheIdentity {
    pub(crate) fn of(adapter: &Adapter) -> Self {
        Self {
            vendor_id: adapter.vendor_id,
            device_id: adapter.device_id,
            driver_version: adapter.driver_version,
            uuid: adapter.pipeline_cache_uuid,
        }
    }
}

/// The file for `data`, the driver's cache data of the device `identity`.
pub(crate) fn wrap(identity: &CacheIdentity, data: &[u8]) -> Vec<u8> {
    let mut file = Vec::with_capacity(HEADER_BYTES + data.len());
    file.extend_from_slice(&MAGIC);
    file.extend_from_slice(&VERSION.to_le_bytes());
    file.extend_from_slice(&identity.vendor_id.to_le_bytes());
    file.extend_from_slice(&identity.device_id.to_le_bytes());
    file.extend_from_slice(&identity.driver_version.to_le_bytes());
    file.extend_from_slice(&identity.uuid);
    file.extend_from_slice(&(data.len() as u64).to_le_bytes());
    file.extend_from_slice(&state_hash64(data).to_le_bytes());
    file.extend_from_slice(data);
    file
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap_or_default())
}

/// The driver's data in `file`, if the file is intact and belongs to `identity`; otherwise
/// why not.
pub(crate) fn unwrap<'a>(identity: &CacheIdentity, file: &'a [u8]) -> Result<&'a [u8], String> {
    if file.len() < HEADER_BYTES || file[..4] != MAGIC {
        return Err(String::from("not a pipeline cache file"));
    }
    let version = u32_at(file, 4);
    if version != VERSION {
        return Err(format!("envelope version {version}, expected {VERSION}"));
    }
    let (vendor_id, device_id) = (u32_at(file, 8), u32_at(file, 12));
    if (vendor_id, device_id) != (identity.vendor_id, identity.device_id) {
        return Err(format!(
            "made on another GPU ({vendor_id:04x}:{device_id:04x}, this one is {:04x}:{:04x})",
            identity.vendor_id, identity.device_id
        ));
    }
    let driver_version = u32_at(file, 16);
    if driver_version != identity.driver_version || file[20..36] != identity.uuid {
        return Err(String::from("made by another driver version"));
    }
    let data = &file[HEADER_BYTES..];
    if u64_at(file, 36) != data.len() as u64 {
        return Err(String::from("truncated"));
    }
    if u64_at(file, 44) != state_hash64(data) {
        return Err(String::from("damaged (checksum mismatch)"));
    }
    // The driver's own header, as vkGetPipelineCacheData wrote it (host byte order).
    if data.len() < DRIVER_HEADER_BYTES {
        return Err(String::from("no driver header"));
    }
    let word =
        |offset: usize| u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap_or_default());
    let header_bytes = word(0) as usize;
    if header_bytes < DRIVER_HEADER_BYTES
        || header_bytes > data.len()
        || word(4) != DRIVER_HEADER_VERSION_ONE
        || word(8) != identity.vendor_id
        || word(12) != identity.device_id
        || data[16..32] != identity.uuid
    {
        return Err(String::from(
            "the driver's header does not match this device",
        ));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: CacheIdentity = CacheIdentity {
        vendor_id: 0x10de,
        device_id: 0x2803,
        driver_version: 7,
        uuid: [9; 16],
    };

    /// Driver data: a valid header for `IDENTITY` and some payload.
    fn driver_data() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&32u32.to_ne_bytes());
        data.extend_from_slice(&DRIVER_HEADER_VERSION_ONE.to_ne_bytes());
        data.extend_from_slice(&IDENTITY.vendor_id.to_ne_bytes());
        data.extend_from_slice(&IDENTITY.device_id.to_ne_bytes());
        data.extend_from_slice(&IDENTITY.uuid);
        data.extend((0..1000u32).map(|i| (i * 7) as u8));
        data
    }

    #[test]
    fn round_trip() {
        let data = driver_data();
        let file = wrap(&IDENTITY, &data);
        assert_eq!(file.len(), HEADER_BYTES + data.len());
        assert_eq!(unwrap(&IDENTITY, &file), Ok(&data[..]));
    }

    #[test]
    fn rejects_other_devices_and_drivers() {
        let file = wrap(&IDENTITY, &driver_data());
        let other_gpu = CacheIdentity {
            device_id: 0x1234,
            ..IDENTITY
        };
        assert!(
            unwrap(&other_gpu, &file)
                .unwrap_err()
                .contains("another GPU")
        );
        let new_driver = CacheIdentity {
            driver_version: 8,
            ..IDENTITY
        };
        assert!(unwrap(&new_driver, &file).unwrap_err().contains("driver"));
        let new_uuid = CacheIdentity {
            uuid: [1; 16],
            ..IDENTITY
        };
        assert!(unwrap(&new_uuid, &file).unwrap_err().contains("driver"));
    }

    #[test]
    fn rejects_damage() {
        let file = wrap(&IDENTITY, &driver_data());
        let mut flipped = file.clone();
        flipped[HEADER_BYTES + 500] ^= 1;
        assert!(
            unwrap(&IDENTITY, &flipped)
                .unwrap_err()
                .contains("checksum")
        );
        assert!(
            unwrap(&IDENTITY, &file[..file.len() - 1])
                .unwrap_err()
                .contains("truncated")
        );
        assert!(unwrap(&IDENTITY, &file[..10]).is_err());
        assert!(unwrap(&IDENTITY, b"").is_err());
        let mut foreign = file.clone();
        foreign[0] = b'X';
        assert!(
            unwrap(&IDENTITY, &foreign)
                .unwrap_err()
                .contains("not a pipeline cache")
        );
    }

    #[test]
    fn rejects_a_driver_header_for_another_device() {
        // An intact envelope around driver data that names another device.
        let mut data = driver_data();
        data[8] ^= 0xff;
        let file = wrap(&IDENTITY, &data);
        assert!(
            unwrap(&IDENTITY, &file)
                .unwrap_err()
                .contains("driver's header")
        );
        let file = wrap(&IDENTITY, &data[..20]);
        assert!(
            unwrap(&IDENTITY, &file)
                .unwrap_err()
                .contains("no driver header")
        );
    }
}
