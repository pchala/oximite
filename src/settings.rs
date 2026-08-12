use crate::board::{FLASH_SIZE, FS_RANGE, FS_SCRATCH};
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
    /// °C offset between the boiler sensor and the group head / puck. Added to
    /// the session brew temperature to form the boiler setpoint
    /// (`control::set_target_temp`) and subtracted again for display
    /// (`Telemetry::display_temp`). `operations::execute_cooldown_flush` also
    /// uses it to stop early, once the boiler reaches
    /// `brew_temp + 2 * temp_offset`.
    pub temp_offset: f32,
    pub flow_pulses_per_liter: f32,
    /// Unused since flow limiting became a dedicated PID on pump duty
    /// (`Settings::flow_pid`). Kept only so existing `sys_machine` blobs in
    /// flash still deserialize — dropping the field would make the whole
    /// section fail to parse and silently reset to defaults.
    pub flow_limit_kp: f32,
    /// Scales the flow-proportional temperature feed-forward applied during a
    /// brew, as a percentage of the built-in nominal gain: 100 = nominal,
    /// 0 = disabled.
    ///
    /// Only active while `control::BrewActiveGuard` is armed. The setpoint
    /// boost is `CONST_FF * (this / 100) * (target_t - 20) * flow_ml_s`,
    /// clamped to +20 °C — it scales with both the flow rate and the
    /// boiler-to-ambient delta. Raise it if the first seconds of a shot sag,
    /// lower it if the boiler overshoots once flow starts.
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
    /// Drives pump duty directly from the flow error whenever a profile step
    /// asks for a flow rate. Output units are 0-100 % triac duty, matching
    /// `PidController`'s built-in clamp.
    pub flow_pid: PidSettings,
    pub wifi: WifiSettings,
}

/// Single source of truth for defaults — used by both `Default` and the static cache.
pub const DEFAULT_SETTINGS: Settings = Settings {
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

    flow_pid: PidSettings {
        kp: 4.0,
        ki: 30.0,
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
    pub flow_pid: PidSettings,
}

impl From<&Settings> for ControlSettings {
    /// The single place the control-relevant subset is spelled out — used by
    /// both `Default` and `Settings::update_ram`, so a new field cannot reach
    /// one and miss the other.
    fn from(s: &Settings) -> Self {
        Self {
            machine: s.machine,
            temp_pid: s.temp_pid,
            press_pid: s.press_pid,
            flow_pid: s.flow_pid,
        }
    }
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self::from(&DEFAULT_SETTINGS)
    }
}

static CONTROL_SETTINGS: Watch<CriticalSectionRawMutex, ControlSettings, 2> = Watch::new();

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
        CONTROL_SETTINGS
            .sender()
            .send(ControlSettings::from(&new_settings));
        *CURRENT_SETTINGS.lock().await = new_settings;
    }
}

// ==========================================
// FLASH PERSISTENCE - Load/save settings
// ==========================================

/// Loads one settings section from flash into `$field`, reporting whether it
/// was there. Absent or corrupt sections leave the default in place.
macro_rules! load_section {
    ($flash:expr, $scratch:expr, $key:expr, $field:expr) => {{
        match fetch_item($flash, FS_RANGE, &mut NoCache::new(), $scratch, $key).await {
            Ok(Some(bytes)) => match serde_json_core::from_slice(bytes) {
                Ok((v, _)) => {
                    $field = v;
                    true
                }
                Err(_) => false,
            },
            _ => false,
        }
    }};
}

pub struct SettingsStore;

impl SettingsStore {
    /// Reads all settings sections from flash and populates the RAM cache.
    pub async fn load<T: Instance>(flash: &mut Flash<'_, T, Async, FLASH_SIZE>) {
        let mut scratch = [0u8; FS_SCRATCH];
        let mut s = Settings::default();

        let mut loaded = load_section!(flash, &mut scratch, b"sys_machine", s.machine);
        loaded |= load_section!(flash, &mut scratch, b"sys_temp_pid", s.temp_pid);
        loaded |= load_section!(flash, &mut scratch, b"sys_press_pid", s.press_pid);
        loaded |= load_section!(flash, &mut scratch, b"sys_flow_pid", s.flow_pid);
        loaded |= load_section!(flash, &mut scratch, b"sys_wifi", s.wifi);

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
        let mut scratch = [0u8; FS_SCRATCH];
        let mut buf = [0u8; FS_SCRATCH];
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
    /// (temp, pressure, flow). Flow is optional so an older UI that only knows
    /// about two PIDs leaves the flow gains untouched instead of zeroing them.
    SavePids(PidSettings, PidSettings, Option<PidSettings>),
    SaveWifi(WifiSettings),
    SaveProfile(u8),
    DeleteProfile(u8),
}

pub static SIG_FLASH_UPDATE: Signal<CriticalSectionRawMutex, FlashUpdate> = Signal::new();

#[embassy_executor::task]
pub async fn flash_update_task(
    mut flash: Flash<'static, embassy_rp::peripherals::FLASH, Async, FLASH_SIZE>,
) {
    let mut state_rx = crate::state::MACHINE_STATE.receiver().unwrap();
    loop {
        let event = SIG_FLASH_UPDATE.wait().await;

        while crate::state::get_state().is_busy() {
            state_rx.changed().await;
        }

        match event {
            FlashUpdate::SaveMachine(m) => {
                let _ = SettingsStore::save_section(&mut flash, b"sys_machine", &m).await;
            }
            FlashUpdate::SavePids(t, p, f) => {
                let _ = SettingsStore::save_section(&mut flash, b"sys_temp_pid", &t).await;
                let _ = SettingsStore::save_section(&mut flash, b"sys_press_pid", &p).await;
                if let Some(f) = f {
                    let _ = SettingsStore::save_section(&mut flash, b"sys_flow_pid", &f).await;
                }
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
