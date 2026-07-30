use crate::board::{FLASH_SIZE, FS_RANGE};
use crate::profiles::{delete_profile_from_flash, get_profile_from_ram, save_profile_to_flash};
use embassy_rp::flash::{Async, Flash, Instance};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use sequential_storage::cache::NoCache;
use sequential_storage::map::{fetch_item, store_item};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct MachineSettings {
    pub brew_temp: f32,
    pub steam_temp: f32,
    pub steam_time_limit_s: f32,
    /// Minutes of inactivity while Idle before the machine auto-sleeps.
    pub sleep_timeout_min: f32,
    /// °C offset between boiler sensor and group head / puck at shot start.
    /// Also used as the maximum boiler target drop by end of shot.
    pub temp_offset: f32,
    pub flow_pulses_per_liter: f32,
    /// Flow-limit backoff gain: bar per (ml/s of flow error), added to an
    /// accumulated pressure setpoint each control tick (integral action, not
    /// a one-shot shift). Keep small — applied every tick (~50Hz).
    ///
    /// Default tuned for max flow ~3.5ml/s and max pressure 9 bar (usable
    /// range 8.8 bar), targeting a full-range correction in ~2s at max
    /// error: kp = 8.8 / (50Hz * 3.5ml/s * 2s) ~= 0.025
    pub flow_limit_kp: f32,
    /// Tau (ml) for the volume-based boiler target decay during a shot.
    /// Lower = faster drop. At vol = tau, ~63% of temp_offset is applied.
    /// At vol = 3*tau, ~95% is applied. Tune to match group head warm-up.
    pub feed_forward_percents: f32,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
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

// ==========================================
// DATA - Pure settings values
// ==========================================

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub machine: MachineSettings,
    pub temp_pid: PidSettings,
    pub press_pid: PidSettings,
    pub wifi: WifiSettings,
}

/// Single source of truth for defaults — used by both `Default` and the static cache.
const DEFAULT_SETTINGS: Settings = Settings {
    machine: MachineSettings {
        brew_temp: 92.0,
        steam_temp: 135.0,
        steam_time_limit_s: 120.0,
        sleep_timeout_min: 20.0,
        temp_offset: 10.0,
        flow_pulses_per_liter: 98324.0, // 49162 physical pulses/L × 2 edges per pulse
        flow_limit_kp: 0.025,
        feed_forward_percents: 100.0,
    },
    temp_pid: PidSettings {
        kp: 8.0,
        ki: 0.8,
        kd: 20.0,
    },
    press_pid: PidSettings {
        kp: 10.0,
        ki: 20.0,
        kd: 0.0,
    },
    wifi: WifiSettings {
        ssid: heapless::String::new(),
        password: heapless::String::new(),
    },
};

impl Default for Settings {
    fn default() -> Self {
        DEFAULT_SETTINGS
    }
}

// ==========================================
// CONTROL SETTINGS - Cheap hot-path snapshot
// ==========================================

/// Subset of `Settings` actually needed by the hard-real-time AC-sync control
/// loop (`control::ac_sync_control_task`, ~100Hz). Kept `Copy` and published
/// via `Watch` so the hot loop can grab the latest values with a plain copy —
/// no mutex lock/await and no cloning of unrelated fields (profile name,
/// Wi-Fi credentials) on every tick.
#[derive(Clone, Copy, PartialEq)]
pub struct ControlSettings {
    pub machine: MachineSettings,
    pub temp_pid: PidSettings,
    pub press_pid: PidSettings,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            machine: DEFAULT_SETTINGS.machine,
            temp_pid: DEFAULT_SETTINGS.temp_pid,
            press_pid: DEFAULT_SETTINGS.press_pid,
        }
    }
}

pub static CONTROL_SETTINGS: Watch<CriticalSectionRawMutex, ControlSettings, 2> = Watch::new();

impl ControlSettings {
    /// Latest control-relevant settings. Cheap enough to call every tick of a
    /// hot loop — republished by `Settings::update_ram` whenever settings change.
    pub fn current() -> Self {
        CONTROL_SETTINGS.try_get().unwrap_or_default()
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
        CONTROL_SETTINGS.sender().send(ControlSettings {
            machine: new_settings.machine,
            temp_pid: new_settings.temp_pid,
            press_pid: new_settings.press_pid,
        });
        *CURRENT_SETTINGS.lock().await = new_settings;
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

pub struct SettingsStore;

impl SettingsStore {
    /// Reads all settings sections from flash and populates the RAM cache.
    pub async fn load<T: Instance>(flash: &mut Flash<'_, T, Async, FLASH_SIZE>) {
        let mut scratch = [0u8; 1024];
        let mut s = Settings::default();
        let mut loaded = false;

        if let Some(v) = load_section!(flash, &mut scratch, b"sys_machine", MachineSettings) {
            s.machine = v;
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

        if loaded {
            defmt::info!("Settings loaded from flash.");
        } else {
            defmt::info!("No settings in flash. Using defaults.");
        }
        Settings::update_ram(s).await;
    }

    pub async fn save_section<T: Instance, S: Serialize, K: sequential_storage::map::Key>(
        flash: &mut Flash<'_, T, Async, FLASH_SIZE>,
        key: &K,
        data: &S,
    ) -> Result<(), ()> {
        let mut scratch = [0u8; 1024];
        let mut buf = [0u8; 1024];
        if let Ok(len) = serde_json_core::to_slice(data, &mut buf) {
            store_item(
                flash,
                FS_RANGE,
                &mut NoCache::new(),
                &mut scratch,
                key,
                &&buf[..len],
            )
            .await
            .map_err(|_| ())
        } else {
            Err(())
        }
    }
}

// ==========================================
// BACKGROUND FLASH EVENT HANDLER
// ==========================================
pub enum FlashUpdate {
    SaveMachine(MachineSettings),
    SavePids(PidSettings, PidSettings),
    SaveWifi(WifiSettings),
    SaveProfile(u8),
    DeleteProfile(u8),
}

pub static SIG_FLASH_UPDATE: Signal<CriticalSectionRawMutex, FlashUpdate> = Signal::new();

#[embassy_executor::task]
pub async fn flash_update_task(
    mut flash: Flash<'static, embassy_rp::peripherals::FLASH, Async, FLASH_SIZE>,
) {
    loop {
        let event = SIG_FLASH_UPDATE.wait().await;
        match event {
            FlashUpdate::SaveMachine(m) => {
                let _ = SettingsStore::save_section(&mut flash, b"sys_machine", &m).await;
            }
            FlashUpdate::SavePids(t, p) => {
                let _ = SettingsStore::save_section(&mut flash, b"sys_temp_pid", &t).await;
                let _ = SettingsStore::save_section(&mut flash, b"sys_press_pid", &p).await;
            }
            FlashUpdate::SaveWifi(w) => {
                let _ = SettingsStore::save_section(&mut flash, b"sys_wifi", &w).await;
            }
            FlashUpdate::SaveProfile(slot) => {
                if let Some(p) = get_profile_from_ram(slot).await {
                    let _ = save_profile_to_flash(&mut flash, slot, &p).await;
                }
            }
            FlashUpdate::DeleteProfile(slot) => {
                let _ = delete_profile_from_flash(&mut flash, slot).await;
            }
        }
    }
}
