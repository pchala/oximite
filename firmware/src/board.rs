//! Board-level facts for the RP2350B espresso controller (pure, host-testable).
//!
//! Hardware clocks, flash layout, storage keys, and CYW43 blob XIP addresses
//! live here so drivers never duplicate magic constants.

use oximite_proto::model::MAX_PROFILES;

/// System clock, as configured by `embassy_rp::init(Default::default())`.
pub const SYS_CLK_HZ: f32 = 150_000_000.0;

/// Total QSPI flash size (16 MiB).
pub const FLASH_SIZE: usize = 16 * 1024 * 1024;

/// Last 64 KiB of flash, reserved for the `sequential-storage` key/value store
/// that backs settings and brew profiles.
pub const FS_RANGE: core::ops::Range<u32> = (FLASH_SIZE as u32 - 64 * 1024)..(FLASH_SIZE as u32);

/// Scratch/serialization buffer size for `sequential-storage` operations on
/// [`FS_RANGE`].
pub const FS_SCRATCH: usize = 1024;

/// Reserved XIP flash locations for the CYW43439 Wi-Fi firmware and CLM blobs,
/// written by `flash-wifi.bat`.
pub const CYW43_FW_ADDR: usize = 0x10FB_0000;
pub const CYW43_FW_LEN: usize = 231_077;
pub const CYW43_CLM_ADDR: usize = 0x10FE_F000;
pub const CYW43_CLM_LEN: usize = 984;

pub const FW_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `sequential-storage` `u8` map keys inside [`FS_RANGE`].
pub const FLASH_KEY_MACHINE: u8 = 1;
pub const FLASH_KEY_TEMP_PID: u8 = 2;
pub const FLASH_KEY_PUMP_PID: u8 = 3;
pub const FLASH_KEY_WIFI: u8 = 4;
pub const FLASH_KEY_PROFILE_BASE: u8 = 10;

pub const fn profile_flash_key(slot: u8) -> Option<u8> {
    if slot < MAX_PROFILES {
        Some(FLASH_KEY_PROFILE_BASE + slot)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/board.rs"]
mod tests;
