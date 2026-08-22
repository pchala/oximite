//! Brew profile model, RAM cache and flash persistence.
//!
//! Profiles share the `sequential-storage` key/value area with settings (see
//! [`crate::board::FS_RANGE`]) but live under their own `prof_N` keys. They are
//! mirrored in a RAM cache so the brew path never has to touch flash.

use embassy_rp::flash::{Async, Flash, Instance};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use sequential_storage::cache::NoCache;
use sequential_storage::map::{fetch_item, remove_item};
use serde::{Deserialize, Serialize};

use crate::board::{FLASH_SIZE, FS_RANGE, FS_SCRATCH};

pub const MAX_PROFILES: u8 = 10;

/// Maximum steps in a brew profile.
///
/// This bounds `MachineCommand::SaveProfile`, which travels through the
/// command queue, so every step costs its 32 bytes in each of the queue's four
/// slots. Five covers pre-infuse, ramp, hold, decline and a tail.
pub const MAX_STEPS: usize = 5;

#[derive(Clone, Serialize, Deserialize)]
pub struct BrewProfileStep {
    pub time_s: Option<f32>,
    pub volume: Option<f32>,
    pub pressure: Option<f32>,
    pub flow: Option<f32>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BrewProfile {
    pub name: heapless::String<32>,
    pub steps: heapless::Vec<BrewProfileStep, MAX_STEPS>,
}

/// Profile used by a bare `Brew` command: slot 0 if one is saved, otherwise a
/// built-in 9 bar / 36 ml shot so the machine is usable out of the box.
pub async fn get_default_profile() -> BrewProfile {
    if let Some(profile) = get_profile_from_ram(0).await {
        return profile;
    }
    let mut p = BrewProfile {
        name: heapless::String::try_from("Standard").unwrap(),
        steps: heapless::Vec::new(),
    };
    let _ = p.steps.push(BrewProfileStep {
        time_s: Some(30.0),
        volume: Some(36.0),
        pressure: Some(9.0),
        flow: Some(0.0),
    });
    p
}

// ==========================================
// RAM CACHE
// ==========================================

static PROFILES_CACHE: Mutex<
    CriticalSectionRawMutex,
    [Option<BrewProfile>; MAX_PROFILES as usize],
> = Mutex::new([const { None }; MAX_PROFILES as usize]);

fn profile_key(slot: u8) -> [u8; 6] {
    [b'p', b'r', b'o', b'f', b'_', b'0' + slot]
}

pub async fn get_profile_from_ram(slot: u8) -> Option<BrewProfile> {
    if slot >= MAX_PROFILES {
        return None;
    }
    PROFILES_CACHE.lock().await[slot as usize].clone()
}

pub async fn get_all_profiles_from_ram(
) -> heapless::Vec<(u8, BrewProfile), { MAX_PROFILES as usize }> {
    let mut list = heapless::Vec::new();
    let cache = PROFILES_CACHE.lock().await;
    for i in 0..MAX_PROFILES {
        if let Some(p) = &cache[i as usize] {
            let _ = list.push((i, p.clone()));
        }
    }
    list
}

pub async fn save_profile_to_ram(slot: u8, profile: BrewProfile) {
    if slot < MAX_PROFILES {
        PROFILES_CACHE.lock().await[slot as usize] = Some(profile);
    }
}

pub async fn delete_profile_from_ram(slot: u8) {
    if slot < MAX_PROFILES {
        PROFILES_CACHE.lock().await[slot as usize] = None;
    }
}

// ==========================================
// FLASH PERSISTENCE
// ==========================================

pub async fn load_all_profiles_from_flash<T: Instance>(
    flash: &mut Flash<'_, T, Async, FLASH_SIZE>,
) {
    let mut scratch = [0u8; FS_SCRATCH];
    let mut cache = PROFILES_CACHE.lock().await;

    for slot in 0..MAX_PROFILES {
        let key = profile_key(slot);
        let fetch_result: Result<Option<&[u8]>, _> =
            fetch_item(flash, FS_RANGE, &mut NoCache::new(), &mut scratch, &key).await;

        if let Ok(Some(item_bytes)) = fetch_result {
            match serde_json_core::from_slice::<BrewProfile>(item_bytes) {
                Ok((profile, _)) => cache[slot as usize] = Some(profile),
                Err(_) => defmt::warn!("Profile slot {} failed to parse — ignoring", slot),
            }
        }
    }
    defmt::info!("All saved profiles loaded into RAM.");
}

pub async fn save_profile_to_flash<T: Instance>(
    flash: &mut Flash<'_, T, Async, FLASH_SIZE>,
    slot: u8,
    profile: &BrewProfile,
) -> Result<(), ()> {
    if slot >= MAX_PROFILES {
        return Err(());
    }
    // Shares `SettingsStore::save_section` rather than repeating the
    // serialize/store dance: profiles and settings live in the same
    // `FS_RANGE`, so a write here can be asked to relocate a settings blob
    // during page GC and must use the same scratch size.
    crate::settings::SettingsStore::save_section(flash, &profile_key(slot), profile).await
}

pub async fn delete_profile_from_flash<T: Instance>(
    flash: &mut Flash<'_, T, Async, FLASH_SIZE>,
    slot: u8,
) -> Result<(), ()> {
    if slot >= MAX_PROFILES {
        return Err(());
    }
    let key = profile_key(slot);
    let mut scratch = [0u8; FS_SCRATCH];
    remove_item(flash, FS_RANGE, &mut NoCache::new(), &mut scratch, &key)
        .await
        .map_err(|_| ())
}
