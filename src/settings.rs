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
    pub total_ml_all_time: f32,
    pub ml_at_last_descale: f32,
}

// ==========================================
// DATA - Pure settings values
// ==========================================

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub machine: MachineSettings,
    pub hardware: HardwareSettings,
    pub temp_pid: PidSettings,
    pub press_pid: PidSettings,
    pub wifi: WifiSettings,
    pub usage: UsageSettings,
}

/// Single source of truth for defaults — used by both `Default` and the static cache.
const DEFAULT_SETTINGS: Settings = Settings {
    machine: MachineSettings {
        brew_temp: 92.0,
        steam_temp: 135.0,
        steam_time_limit_s: 120.0,
        steam_pressure: 1.5,
    },
    hardware: HardwareSettings {
        temp_offset: 8.0,
        flow_edges_per_liter: 5200.0,
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
        total_ml_all_time: 0.0,
        ml_at_last_descale: 0.0,
    },
};

impl Default for Settings {
    fn default() -> Self {
        DEFAULT_SETTINGS
    }
}

// ==========================================
// RAM CACHE - Live settings state
// ==========================================

static CURRENT_SETTINGS: Mutex<CriticalSectionRawMutex, Settings> = Mutex::new(DEFAULT_SETTINGS);

impl Settings {
    pub async fn get() -> Self {
        CURRENT_SETTINGS.lock().await.clone()
    }

    pub async fn update_ram(new_settings: Self) {
        *CURRENT_SETTINGS.lock().await = new_settings;
    }

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
}

// ==========================================
// FLASH PERSISTENCE - Load/save settings
// ==========================================

/// Fetches one settings section from flash, returning `None` if absent or corrupt.
macro_rules! load_section {
    ($flash:expr, $scratch:expr, $key:expr, $type:ty) => {{
        match fetch_item($flash, FS_RANGE, &mut NoCache::new(), $scratch, $key).await {
            Ok(Some(bytes)) => serde_json_core::from_slice::<$type>(bytes)
                .ok()
                .map(|(v, _)| v),
            _ => None,
        }
    }};
}

/// Serialises and stores a section only when the value has changed.
macro_rules! save_if_changed {
    ($flash:expr, $scratch:expr, $buf:expr, $old:expr, $new:expr, $key:expr, $changed:ident) => {
        if $old != $new {
            if let Ok(len) = serde_json_core::to_slice($new, &mut $buf) {
                let _ = store_item(
                    $flash,
                    FS_RANGE,
                    &mut NoCache::new(),
                    $scratch,
                    $key,
                    &&$buf[..len],
                )
                .await;
                $changed = true;
            }
        }
    };
}

pub struct SettingsStore;

impl SettingsStore {
    /// Reads all settings sections from flash and populates the RAM cache.
    pub async fn load<T: Instance>(flash: &mut Flash<'_, T, Async, 2097152>) {
        let mut scratch = [0u8; 1024];
        let mut s = Settings::default();
        let mut loaded = false;

        if let Some(v) = load_section!(flash, &mut scratch, b"sys_machine", MachineSettings) {
            s.machine = v;
            loaded = true;
        }
        if let Some(v) = load_section!(flash, &mut scratch, b"sys_hardware", HardwareSettings) {
            s.hardware = v;
            loaded = true;
        }
        if let Some(v) = load_section!(flash, &mut scratch, b"sys_temp_pid", PidSettings) {
            s.temp_pid = v;
            loaded = true;
        }
        if let Some(v) = load_section!(flash, &mut scratch, b"sys_press_pid", PidSettings) {
            s.press_pid = v;
            loaded = true;
        }
        if let Some(v) = load_section!(flash, &mut scratch, b"sys_wifi", WifiSettings) {
            s.wifi = v;
            loaded = true;
        }
        if let Some(v) = load_section!(flash, &mut scratch, b"sys_usage", UsageSettings) {
            s.usage = v;
            loaded = true;
        }

        if loaded {
            defmt::info!("Settings loaded from flash.");
        } else {
            defmt::info!("No settings in flash. Using defaults.");
        }
        Settings::update_ram(s).await;
    }

    /// Writes only sections that differ between `old` and `new` to flash.
    pub async fn save_changed<T: Instance>(
        flash: &mut Flash<'_, T, Async, 2097152>,
        old: &Settings,
        new: &Settings,
    ) {
        let mut scratch = [0u8; 1024];
        let mut buf = [0u8; 1024];
        let mut saved = false;

        save_if_changed!(
            flash,
            &mut scratch,
            buf,
            &old.machine,
            &new.machine,
            b"sys_machine",
            saved
        );
        save_if_changed!(
            flash,
            &mut scratch,
            buf,
            &old.hardware,
            &new.hardware,
            b"sys_hardware",
            saved
        );
        save_if_changed!(
            flash,
            &mut scratch,
            buf,
            &old.temp_pid,
            &new.temp_pid,
            b"sys_temp_pid",
            saved
        );
        save_if_changed!(
            flash,
            &mut scratch,
            buf,
            &old.press_pid,
            &new.press_pid,
            b"sys_press_pid",
            saved
        );
        save_if_changed!(
            flash,
            &mut scratch,
            buf,
            &old.wifi,
            &new.wifi,
            b"sys_wifi",
            saved
        );
        save_if_changed!(
            flash,
            &mut scratch,
            buf,
            &old.usage,
            &new.usage,
            b"sys_usage",
            saved
        );

        if saved {
            defmt::info!("Settings changes saved to flash.");
        }
    }
}

// ==========================================
// PROFILE RAM CACHE & FLASH MANAGEMENT
// ==========================================

static PROFILES_CACHE: Mutex<CriticalSectionRawMutex, [Option<BrewProfile>; 10]> =
    Mutex::new([None, None, None, None, None, None, None, None, None, None]);

fn profile_key(slot: u8) -> [u8; 6] {
    [b'p', b'r', b'o', b'f', b'_', b'0' + slot]
}

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
        let key = profile_key(slot);
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
    let key = profile_key(slot);
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
    let key = profile_key(slot);
    let mut scratch = [0u8; 512];
    remove_item(flash, FS_RANGE, &mut NoCache::new(), &mut scratch, &key)
        .await
        .map_err(|_| ())
}
