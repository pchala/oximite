use embassy_rp::flash::{Async, Flash, Instance};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use sequential_storage::cache::NoCache;
use sequential_storage::map::{fetch_item, remove_item, store_item};
use serde::{Deserialize, Serialize};

const FS_RANGE: core::ops::Range<u32> = (2097152 - 65536)..2097152;
const MAX_PROFILES: u8 = 10;

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
    pub steps: heapless::Vec<BrewProfileStep, 10>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SettingsManager {
    pub brew_temp: f32,
    pub steam_temp: f32,
    pub temp_offset: f32,
    pub steam_time_limit_s: f32,
    pub steam_pressure: f32,
    pub temp_kp: f32,
    pub temp_ki: f32,
    pub temp_kd: f32,
    pub press_kp: f32,
    pub press_ki: f32,
    pub press_kd: f32,
    pub flow_edges_per_liter: f32,
    pub wifi_ssid: heapless::String<32>,
    pub wifi_password: heapless::String<64>,
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self {
            brew_temp: 92.0,
            steam_temp: 135.0,
            temp_offset: -2.5,
            steam_time_limit_s: 120.0,
            steam_pressure: 1.5,
            temp_kp: 2.0,
            temp_ki: 0.01,
            temp_kd: 5.0,
            press_kp: 2.0,
            press_ki: 0.1,
            press_kd: 0.5,
            flow_edges_per_liter: 3850.0,
            wifi_ssid: heapless::String::try_from("Oximite-Setup").unwrap(),
            wifi_password: heapless::String::try_from("password").unwrap(),
        }
    }
}

static CURRENT_SETTINGS: Mutex<CriticalSectionRawMutex, SettingsManager> =
    Mutex::new(SettingsManager {
        brew_temp: 92.0,
        steam_temp: 135.0,
        temp_offset: -2.5,
        steam_time_limit_s: 120.0,
        steam_pressure: 1.5,
        temp_kp: 2.0,
        temp_ki: 0.01,
        temp_kd: 5.0,
        press_kp: 2.0,
        press_ki: 0.1,
        press_kd: 0.5,
        flow_edges_per_liter: 3850.0,
        wifi_ssid: heapless::String::new(),
        wifi_password: heapless::String::new(),
    });

impl SettingsManager {
    pub async fn get() -> Self {
        CURRENT_SETTINGS.lock().await.clone()
    }

    pub async fn update_ram(new_settings: Self) {
        *CURRENT_SETTINGS.lock().await = new_settings;
    }

    pub async fn load_from_flash<T: Instance>(flash: &mut Flash<'_, T, Async, 2097152>) {
        let mut scratch = [0u8; 1024];

        let fetch_result: Result<Option<&[u8]>, _> = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_settings",
        )
        .await;

        match fetch_result {
            Ok(Some(item_bytes)) => {
                if let Ok((settings, _)) = serde_json_core::from_slice::<Self>(item_bytes) {
                    defmt::info!("Settings loaded from FS.");
                    Self::update_ram(settings).await;
                }
            }
            _ => {
                defmt::info!("No settings found in flash. Using defaults.");
                Self::update_ram(Self::default()).await;
            }
        }
    }

    pub async fn save_to_flash<T: Instance>(
        flash: &mut Flash<'_, T, Async, 2097152>,
        settings: &Self,
    ) {
        let mut scratch = [0u8; 2048];
        let mut data = [0u8; 2048];
        match serde_json_core::to_slice(settings, &mut data) {
            Ok(len) => {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_settings",
                    &&data[..len],
                )
                .await;
                defmt::info!("Settings saved to FS.");
            }
            Err(_e) => {
                defmt::error!("Failed to serialize settings. They were not saved.");
            }
        }
    }

    pub async fn get_default_profile() -> BrewProfile {
        // Try to load Slot 0 first. If it exists, this is our default!
        if let Some(profile) = get_profile_from_ram(0).await {
            return profile;
        }

        // Fallback: If Slot 0 is empty, return a safe standard profile
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
}

// ==========================================
// PROFILE RAM CACHE & FLASH MANAGEMENT
// ==========================================

static PROFILES_CACHE: Mutex<CriticalSectionRawMutex, [Option<BrewProfile>; 10]> =
    Mutex::new([None, None, None, None, None, None, None, None, None, None]);

pub async fn get_profile_from_ram(slot: u8) -> Option<BrewProfile> {
    if slot >= MAX_PROFILES {
        return None;
    }
    PROFILES_CACHE.lock().await[slot as usize].clone()
}

pub async fn get_all_profiles_from_ram() -> heapless::Vec<(u8, BrewProfile), 10> {
    let mut list = heapless::Vec::new();
    let cache = PROFILES_CACHE.lock().await;
    for i in 0..MAX_PROFILES {
        if let Some(p) = &cache[i as usize] {
            let _ = list.push((i, p.clone()));
        }
    }
    list
}

pub async fn load_all_profiles_from_flash<T: Instance>(flash: &mut Flash<'_, T, Async, 2097152>) {
    let mut scratch = [0u8; 512];
    let mut cache = PROFILES_CACHE.lock().await;

    for slot in 0..MAX_PROFILES {
        let key = [b'p', b'r', b'o', b'f', b'_', b'0' + slot];
        let fetch_result: Result<Option<&[u8]>, _> =
            fetch_item(flash, FS_RANGE, &mut NoCache::new(), &mut scratch, &key).await;

        if let Ok(Some(item_bytes)) = fetch_result {
            if let Ok((profile, _)) = serde_json_core::from_slice::<BrewProfile>(item_bytes) {
                cache[slot as usize] = Some(profile);
            }
        }
    }
    defmt::info!("All saved profiles loaded into RAM.");
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

pub async fn save_profile_to_flash<T: Instance>(
    flash: &mut Flash<'_, T, Async, 2097152>,
    slot: u8,
    profile: &BrewProfile,
) -> Result<(), ()> {
    if slot >= MAX_PROFILES {
        return Err(());
    }
    let key = [b'p', b'r', b'o', b'f', b'_', b'0' + slot];
    let mut scratch = [0u8; 512];
    let mut data = [0u8; 1024];

    if let Ok(len) = serde_json_core::to_slice(profile, &mut data) {
        store_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            &key,
            &&data[..len],
        )
        .await
        .map_err(|_| ())
    } else {
        Err(())
    }
}

pub async fn delete_profile_from_flash<T: Instance>(
    flash: &mut Flash<'_, T, Async, 2097152>,
    slot: u8,
) -> Result<(), ()> {
    if slot >= MAX_PROFILES {
        return Err(());
    }
    let key = [b'p', b'r', b'o', b'f', b'_', b'0' + slot];
    let mut scratch = [0u8; 512];
    remove_item(flash, FS_RANGE, &mut NoCache::new(), &mut scratch, &key)
        .await
        .map_err(|_| ())
}
