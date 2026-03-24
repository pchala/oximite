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

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineSettings {
    pub brew_temp: f32,
    pub steam_temp: f32,
    pub steam_time_limit_s: f32,
    pub steam_pressure: f32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct HardwareSettings {
    pub temp_offset: f32,
    pub flow_edges_per_liter: f32,
    pub temp_feed_forward: f32,
    pub flow_multiplier: f32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct PidSettings {
    pub kp: f32,
    pub ki: f32,
    pub kd: f32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct WifiSettings {
    pub ssid: heapless::String<32>,
    pub password: heapless::String<64>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageSettings {
    pub total_ml_since_descale: f32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsManager {
    pub machine: MachineSettings,
    pub hardware: HardwareSettings,
    pub temp_pid: PidSettings,
    pub press_pid: PidSettings,
    pub wifi: WifiSettings,
    pub usage: UsageSettings,
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self {
            machine: MachineSettings {
                brew_temp: 92.0,
                steam_temp: 135.0,
                steam_time_limit_s: 120.0,
                steam_pressure: 1.5,
            },
            hardware: HardwareSettings {
                temp_offset: -2.5,
                flow_edges_per_liter: 3850.0,
                temp_feed_forward: 35.0,
                flow_multiplier: 20.0,
            },
            temp_pid: PidSettings {
                kp: 2.0,
                ki: 0.01,
                kd: 5.0,
            },
            press_pid: PidSettings {
                kp: 2.0,
                ki: 0.1,
                kd: 0.5,
            },
            wifi: WifiSettings {
                ssid: heapless::String::try_from("").unwrap(),
                password: heapless::String::try_from("").unwrap(),
            },
            usage: UsageSettings {
                total_ml_since_descale: 0.0,
            },
        }
    }
}

static CURRENT_SETTINGS: Mutex<CriticalSectionRawMutex, SettingsManager> =
    Mutex::new(SettingsManager {
        machine: MachineSettings {
            brew_temp: 92.0,
            steam_temp: 135.0,
            steam_time_limit_s: 120.0,
            steam_pressure: 1.5,
        },
        hardware: HardwareSettings {
            temp_offset: -2.5,
            flow_edges_per_liter: 3850.0,
            temp_feed_forward: 35.0,
            flow_multiplier: 20.0,
        },
        temp_pid: PidSettings {
            kp: 2.0,
            ki: 0.01,
            kd: 5.0,
        },
        press_pid: PidSettings {
            kp: 2.0,
            ki: 0.1,
            kd: 0.5,
        },
        wifi: WifiSettings {
            ssid: heapless::String::new(),
            password: heapless::String::new(),
        },
        usage: UsageSettings {
            total_ml_since_descale: 0.0,
        },
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
        let mut loaded_settings = Self::default();
        let mut anything_loaded = false;

        if let Ok(Some(item_bytes)) = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_machine",
        )
        .await
        {
            if let Ok((machine, _)) = serde_json_core::from_slice::<MachineSettings>(item_bytes) {
                loaded_settings.machine = machine;
                anything_loaded = true;
            }
        }
        if let Ok(Some(item_bytes)) = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_hardware",
        )
        .await
        {
            if let Ok((hardware, _)) = serde_json_core::from_slice::<HardwareSettings>(item_bytes) {
                loaded_settings.hardware = hardware;
                anything_loaded = true;
            }
        }
        if let Ok(Some(item_bytes)) = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_temp_pid",
        )
        .await
        {
            if let Ok((temp_pid, _)) = serde_json_core::from_slice::<PidSettings>(item_bytes) {
                loaded_settings.temp_pid = temp_pid;
                anything_loaded = true;
            }
        }
        if let Ok(Some(item_bytes)) = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_press_pid",
        )
        .await
        {
            if let Ok((press_pid, _)) = serde_json_core::from_slice::<PidSettings>(item_bytes) {
                loaded_settings.press_pid = press_pid;
                anything_loaded = true;
            }
        }
        if let Ok(Some(item_bytes)) = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_wifi",
        )
        .await
        {
            if let Ok((wifi, _)) = serde_json_core::from_slice::<WifiSettings>(item_bytes) {
                loaded_settings.wifi = wifi;
                anything_loaded = true;
            }
        }
        if let Ok(Some(item_bytes)) = fetch_item(
            flash,
            FS_RANGE,
            &mut NoCache::new(),
            &mut scratch,
            b"sys_usage",
        )
        .await
        {
            if let Ok((usage, _)) = serde_json_core::from_slice::<UsageSettings>(item_bytes) {
                loaded_settings.usage = usage;
                anything_loaded = true;
            }
        }

        if anything_loaded {
            defmt::info!("Settings loaded from FS.");
        } else {
            defmt::info!("No settings found in flash. Using defaults.");
        }
        Self::update_ram(loaded_settings).await;
    }

    pub async fn save_changes_to_flash<T: Instance>(
        flash: &mut Flash<'_, T, Async, 2097152>,
        old_settings: &Self,
        new_settings: &Self,
    ) {
        let mut scratch = [0u8; 1024];
        let mut data = [0u8; 1024];
        let mut saved_anything = false;

        if old_settings.machine != new_settings.machine {
            if let Ok(len) = serde_json_core::to_slice(&new_settings.machine, &mut data) {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_machine",
                    &&data[..len],
                )
                .await;
                saved_anything = true;
            }
        }
        if old_settings.hardware != new_settings.hardware {
            if let Ok(len) = serde_json_core::to_slice(&new_settings.hardware, &mut data) {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_hardware",
                    &&data[..len],
                )
                .await;
                saved_anything = true;
            }
        }
        if old_settings.temp_pid != new_settings.temp_pid {
            if let Ok(len) = serde_json_core::to_slice(&new_settings.temp_pid, &mut data) {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_temp_pid",
                    &&data[..len],
                )
                .await;
                saved_anything = true;
            }
        }
        if old_settings.press_pid != new_settings.press_pid {
            if let Ok(len) = serde_json_core::to_slice(&new_settings.press_pid, &mut data) {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_press_pid",
                    &&data[..len],
                )
                .await;
                saved_anything = true;
            }
        }
        if old_settings.wifi != new_settings.wifi {
            if let Ok(len) = serde_json_core::to_slice(&new_settings.wifi, &mut data) {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_wifi",
                    &&data[..len],
                )
                .await;
                saved_anything = true;
            }
        }
        if old_settings.usage != new_settings.usage {
            if let Ok(len) = serde_json_core::to_slice(&new_settings.usage, &mut data) {
                let _ = store_item(
                    flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    &mut scratch,
                    b"sys_usage",
                    &&data[..len],
                )
                .await;
                saved_anything = true;
            }
        }
        if saved_anything {
            defmt::info!("Settings changes saved to FS.");
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
