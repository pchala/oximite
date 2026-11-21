//! Shared data models and wire protocols for the Oximite espresso controller.
//!
//! Used identically by the `no_std` RP2350B firmware, the PC host test suite,
//! and the Windows Rust control UI so wire formats and domain structs never drift.

#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]

pub use heapless;

pub mod model;
pub mod protocol;
