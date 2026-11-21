//! RP2350 internal flash persistence using `sequential-storage 8.0.2`.
//!
//! Core 0 alone reads/writes flash via `DMA_CH1`. Writes are coalesced via
//! `bus::FLASH_DIRTY` and delayed until `!state.is_busy()` so flash erases
//! never stall an active brew shot.

use embassy_rp::flash::{Async, Flash};
use embassy_rp::peripherals::FLASH;
use oximite_proto::model::{BrewProfile, Settings, MAX_PROFILES};
use oximite_proto::protocol::DeviceEvent;
use sequential_storage::cache::Cache;
use sequential_storage::map::{MapConfig, MapStorage};
use serde::{de::DeserializeOwned, Serialize};

use crate::board::{
    profile_flash_key, FLASH_KEY_MACHINE, FLASH_KEY_PUMP_PID, FLASH_KEY_TEMP_PID, FLASH_KEY_WIFI,
    FLASH_SIZE, FS_RANGE, FS_SCRATCH,
};
use crate::bus;
use crate::machine::{is_machine_dirty, is_pids_dirty, is_profile_dirty, is_wifi_dirty};

pub type AppFlash = Flash<'static, FLASH, Async, FLASH_SIZE>;

macro_rules! map {
    ($flash:expr) => {
        MapStorage::<u8, _, _>::new(
            &mut *$flash,
            const { MapConfig::new(FS_RANGE) },
            Cache::new_uncached(),
        )
    };
}

async fn erase_corrupted(flash: &mut AppFlash) -> bool {
    defmt::warn!("Flash settings/profiles area corrupted — erasing");
    let mut store = map!(flash);
    store.erase_all().await.is_ok()
}

async fn load_json_section<T: DeserializeOwned>(flash: &mut AppFlash, key: u8) -> Option<T> {
    let mut scratch = [0u8; FS_SCRATCH];
    let res = {
        let mut store = map!(flash);
        store.fetch_item::<&[u8]>(&mut scratch, &key).await
    };
    match res {
        Ok(Some(bytes)) => match serde_json_core::from_slice::<T>(bytes) {
            Ok((val, _)) => Some(val),
            Err(_) => {
                defmt::warn!("Flash key {} failed to parse — ignoring", key);
                None
            }
        },
        Ok(None) => None,
        Err(sequential_storage::Error::Corrupted { .. }) => {
            let _ = erase_corrupted(flash).await;
            None
        }
        Err(_) => None,
    }
}

async fn save_json_section<T: Serialize>(flash: &mut AppFlash, key: u8, data: &T) -> bool {
    let mut json_buf = [0u8; FS_SCRATCH];
    let Ok(len) = serde_json_core::to_slice(data, &mut json_buf) else {
        defmt::warn!("Flash key {} exceeded serialization buffer", key);
        return false;
    };
    let slice: &[u8] = &json_buf[..len];
    let mut scratch = [0u8; FS_SCRATCH];
    let res = {
        let mut store = map!(flash);
        store.store_item(&mut scratch, &key, &slice).await
    };
    match res {
        Ok(()) => true,
        Err(sequential_storage::Error::Corrupted { .. }) => {
            if !erase_corrupted(flash).await {
                return false;
            }
            let mut store = map!(flash);
            store.store_item(&mut scratch, &key, &slice).await.is_ok()
        }
        Err(_) => false,
    }
}

async fn remove_flash_key(flash: &mut AppFlash, key: u8) -> bool {
    let mut scratch = [0u8; FS_SCRATCH];
    let res = {
        let mut store = map!(flash);
        store.remove_item(&mut scratch, &key).await
    };
    match res {
        Ok(()) => true,
        Err(sequential_storage::Error::Corrupted { .. }) => erase_corrupted(flash).await,
        Err(_) => false,
    }
}

/// Loads all settings sections and saved brew profiles from flash at boot.
pub async fn load_all(
    flash: &mut AppFlash,
) -> (Settings, [Option<BrewProfile>; MAX_PROFILES as usize]) {
    let mut settings = Settings::default();
    let mut loaded_any = false;

    if let Some(m) = load_json_section(flash, FLASH_KEY_MACHINE).await {
        settings.machine = m;
        loaded_any = true;
    }
    if let Some(t) = load_json_section(flash, FLASH_KEY_TEMP_PID).await {
        settings.temp_pid = t;
        loaded_any = true;
    }
    if let Some(p) = load_json_section(flash, FLASH_KEY_PUMP_PID).await {
        settings.pump_pid = p;
        loaded_any = true;
    }
    if let Some(w) = load_json_section(flash, FLASH_KEY_WIFI).await {
        settings.wifi = w;
        loaded_any = true;
    }

    if loaded_any {
        defmt::info!("Settings loaded from flash.");
    } else {
        defmt::info!("No settings in flash. Using defaults.");
    }

    let mut profiles: [Option<BrewProfile>; MAX_PROFILES as usize] =
        [const { None }; MAX_PROFILES as usize];
    for slot in 0..MAX_PROFILES {
        if let Some(key) = profile_flash_key(slot) {
            profiles[slot as usize] = load_json_section(flash, key).await;
        }
    }
    defmt::info!("All saved profiles loaded into RAM.");

    (settings, profiles)
}

#[embassy_executor::task]
pub async fn storage_task(mut flash: AppFlash) {
    let mut state_rx = bus::MACHINE_STATE.receiver().unwrap();
    loop {
        bus::wait_flash_dirty().await;

        while bus::get_state().is_busy() {
            state_rx.changed().await;
        }

        let mask = bus::take_flash_dirty();
        if mask == 0 {
            continue;
        }

        let snap = bus::get_settings_snapshot();
        let (wifi, _) = bus::get_wifi_settings();
        let mut ok = true;

        if is_machine_dirty(mask) {
            ok &= save_json_section(&mut flash, FLASH_KEY_MACHINE, &snap.control.machine).await;
        }
        if is_pids_dirty(mask) {
            ok &= save_json_section(&mut flash, FLASH_KEY_TEMP_PID, &snap.control.temp_pid).await;
            ok &= save_json_section(&mut flash, FLASH_KEY_PUMP_PID, &snap.control.pump_pid).await;
        }
        if is_wifi_dirty(mask) {
            ok &= save_json_section(&mut flash, FLASH_KEY_WIFI, &wifi).await;
        }
        for slot in 0..MAX_PROFILES {
            if is_profile_dirty(mask, slot) {
                if let Some(key) = profile_flash_key(slot) {
                    match bus::get_profile(slot) {
                        Some(p) => ok &= save_json_section(&mut flash, key, &p).await,
                        None => ok &= remove_flash_key(&mut flash, key).await,
                    }
                }
            }
        }

        if ok {
            bus::push_event(DeviceEvent::FlashSaved);
        } else {
            defmt::warn!("One or more flash writes failed (mask={:#x})", mask);
        }
    }
}
