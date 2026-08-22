//! Board-level facts for the RP2350B espresso controller.
//!
//! These are properties of the hardware and the flash layout rather than of any
//! one driver, so they live here instead of being duplicated as literals across
//! the PIO setup functions and the settings store.

/// System clock, as configured by `embassy_rp::init(Default::default())`.
///
/// Every PIO clock divider is derived from this.
pub const SYS_CLK_HZ: f32 = 150_000_000.0;

/// Total QSPI flash size (16 MiB).
pub const FLASH_SIZE: usize = 16 * 1024 * 1024;

/// Last 64 KiB of flash, reserved for the `sequential-storage` key/value store
/// that backs settings and brew profiles.
pub const FS_RANGE: core::ops::Range<u32> = (FLASH_SIZE as u32 - 64 * 1024)..(FLASH_SIZE as u32);

/// Scratch/serialization buffer size for `sequential-storage` operations on
/// [`FS_RANGE`].
///
/// Settings and brew profiles share one key/value area, so any write can be
/// asked to relocate any *other* item while compacting a page. Every caller
/// must therefore size its buffer for the largest item in the whole store, not
/// just for the one it is reading or writing.
pub const FS_SCRATCH: usize = 1024;
