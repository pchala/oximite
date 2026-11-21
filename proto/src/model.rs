//! Core domain models shared across firmware, host tests, and the desktop UI.

use serde::{Deserialize, Serialize};

pub const MAX_PROFILES: u8 = 10;
pub const MAX_STEPS: usize = 5;
pub const STEP_NONE: u8 = u8::MAX;

/// Operational state of the espresso machine.
///
/// Discriminants are a wire contract with the web UI (`GET /api/telemetry` `st`)
/// and the Port 8080 binary protocol (`TelemetryFrame.st`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MachineState {
    #[default]
    Idle = 0,
    Brewing = 1,
    Steaming = 2,
    Sleeping = 3,
    Pumping = 4,
    Cooling = 5,
    HotWater = 6,
}

impl MachineState {
    /// States representing an active hardware operation (profile/steam/pump
    /// running). Used to gate flash writes and busy-abort transitions.
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            MachineState::Brewing
                | MachineState::Pumping
                | MachineState::Cooling
                | MachineState::HotWater
                | MachineState::Steaming
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MachineState::Idle => "IDLE",
            MachineState::Brewing => "BREWING",
            MachineState::Steaming => "STEAMING",
            MachineState::Sleeping => "SLEEPING",
            MachineState::Pumping => "PUMPING",
            MachineState::Cooling => "COOLING",
            MachineState::HotWater => "HOT WATER",
        }
    }
}

impl From<MachineState> for u8 {
    fn from(state: MachineState) -> Self {
        state as u8
    }
}

impl TryFrom<u8> for MachineState {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(MachineState::Idle),
            1 => Ok(MachineState::Brewing),
            2 => Ok(MachineState::Steaming),
            3 => Ok(MachineState::Sleeping),
            4 => Ok(MachineState::Pumping),
            5 => Ok(MachineState::Cooling),
            6 => Ok(MachineState::HotWater),
            other => Err(other),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineSettings {
    pub brew_temp: f32,
    pub steam_temp: f32,
    pub steam_time_limit_s: f32,
    /// Minutes of inactivity while Idle before the machine auto-sleeps.
    pub sleep_timeout_min: f32,
    /// Offset in deg C between the boiler sensor and the group head / puck.
    pub temp_offset: f32,
    pub flow_pulses_per_liter: f32,
    /// Flow-proportional temperature feed-forward scaling in percent (0 disables).
    pub feed_forward_percents: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PidSettings {
    pub kp: f32,
    pub ki: f32,
    pub kd: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WifiSettings {
    pub ssid: heapless::String<32>,
    pub password: heapless::String<64>,
}

/// Redacted Wi-Fi status exposed over HTTP and Port 8080 so stored passwords
/// are never transmitted over the network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicWifiInfo {
    pub ssid: heapless::String<32>,
    pub has_password: bool,
}

/// Network-safe settings view (identical to `Settings` except Wi-Fi password
/// is replaced by `has_password`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicSettings {
    pub machine: MachineSettings,
    pub temp_pid: PidSettings,
    pub pump_pid: PidSettings,
    pub wifi: PublicWifiInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub machine: MachineSettings,
    pub temp_pid: PidSettings,
    pub pump_pid: PidSettings,
    pub wifi: WifiSettings,
}

impl Settings {
    pub fn to_public(&self) -> PublicSettings {
        PublicSettings {
            machine: self.machine,
            temp_pid: self.temp_pid,
            pump_pid: self.pump_pid,
            wifi: PublicWifiInfo {
                ssid: self.wifi.ssid.clone(),
                has_password: !self.wifi.password.is_empty(),
            },
        }
    }
}

/// Single source of truth for factory default settings.
pub const DEFAULT_SETTINGS: Settings = Settings {
    machine: MachineSettings {
        brew_temp: 92.0,
        steam_temp: 135.0,
        steam_time_limit_s: 120.0,
        sleep_timeout_min: 20.0,
        temp_offset: 10.0,
        flow_pulses_per_liter: 98324.0,
        feed_forward_percents: 100.0,
    },
    temp_pid: PidSettings {
        kp: 8.0,
        ki: 0.8,
        kd: 20.0,
    },
    pump_pid: PidSettings {
        kp: 12.0,
        ki: 36.0,
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

/// Copy-only settings snapshot needed by the 50 Hz AC control loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlSettings {
    pub machine: MachineSettings,
    pub temp_pid: PidSettings,
    pub pump_pid: PidSettings,
}

impl From<&Settings> for ControlSettings {
    fn from(s: &Settings) -> Self {
        Self {
            machine: s.machine,
            temp_pid: s.temp_pid,
            pump_pid: s.pump_pid,
        }
    }
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self::from(&DEFAULT_SETTINGS)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrewProfileStep {
    pub time_s: Option<f32>,
    pub volume: Option<f32>,
    pub pressure: Option<f32>,
    pub flow: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrewProfile {
    pub name: heapless::String<32>,
    pub steps: heapless::Vec<BrewProfileStep, MAX_STEPS>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileHeaderOwned {
    pub slot: u8,
    pub name: heapless::String<32>,
}

/// Built-in 9 bar / 36 ml shot used when slot 0 has no saved profile.
pub fn default_profile() -> BrewProfile {
    let mut name = heapless::String::new();
    let _ = name.push_str("Standard");
    let mut steps = heapless::Vec::new();
    let _ = steps.push(BrewProfileStep {
        time_s: Some(30.0),
        volume: Some(36.0),
        pressure: Some(9.0),
        flow: Some(0.0),
    });
    BrewProfile { name, steps }
}

/// How the pump should be driven for the current tick.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum PumpMode {
    #[default]
    Idle,
    DirectPump(f32),
    Unified {
        target_bar: f32,
        target_ml_s: f32,
    },
}

impl PumpMode {
    pub fn targets(&self) -> (f32, f32) {
        match *self {
            PumpMode::Unified {
                target_bar,
                target_ml_s,
            } => (target_bar, target_ml_s),
            _ => (0.0, 0.0),
        }
    }

    pub fn has_target(&self) -> bool {
        let (bar, ml_s) = self.targets();
        bar > 0.0 || ml_s > 0.0
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum MachineCommand {
    RunProfile(BrewProfile),
    Brew,
    Stop,
    Steam,
    Flush,
    DirectPump(f32),
    SaveMachine(MachineSettings),
    SavePids(PidSettings, PidSettings),
    SaveWifi(WifiSettings),
    SaveProfile(u8, BrewProfile),
    DeleteProfile(u8),
    TogglePower,
    SetSessionTemp(f32),
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Ambient {
    No,
    RamOnly,
    RamAndFlash,
}

impl MachineCommand {
    pub fn ambient(&self) -> Ambient {
        match self {
            MachineCommand::SaveMachine(_)
            | MachineCommand::SavePids(_, _)
            | MachineCommand::SaveWifi(_)
            | MachineCommand::SaveProfile(_, _)
            | MachineCommand::DeleteProfile(_) => Ambient::RamAndFlash,
            MachineCommand::SetSessionTemp(_) => Ambient::RamOnly,
            MachineCommand::RunProfile(_)
            | MachineCommand::Brew
            | MachineCommand::Stop
            | MachineCommand::Steam
            | MachineCommand::Flush
            | MachineCommand::DirectPump(_)
            | MachineCommand::TogglePower => Ambient::No,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            MachineCommand::RunProfile(_) => "RunProfile",
            MachineCommand::Brew => "Brew",
            MachineCommand::Stop => "Stop",
            MachineCommand::Steam => "Steam",
            MachineCommand::Flush => "Flush",
            MachineCommand::DirectPump(_) => "DirectPump",
            MachineCommand::SaveMachine(_) => "SaveMachine",
            MachineCommand::SavePids(_, _) => "SavePids",
            MachineCommand::SaveWifi(_) => "SaveWifi",
            MachineCommand::SaveProfile(_, _) => "SaveProfile",
            MachineCommand::DeleteProfile(_) => "DeleteProfile",
            MachineCommand::TogglePower => "TogglePower",
            MachineCommand::SetSessionTemp(_) => "SetSessionTemp",
        }
    }
}

/// Complete per-tick controller snapshot published by Core 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Telemetry {
    pub tick: u32,
    pub pressure_bar: f32,
    pub temp_c: f32,
    pub target_bar: f32,
    pub target_temp: f32,
    pub effective_target_temp: f32,
    pub target_ml_s: f32,
    pub flow_rate_ml_s: f32,
    pub volume_ml: f32,
    pub flow_controlled: bool,
    pub heater_duty: f32,
    pub pump_duty: f32,
    pub ac_period_us: f32,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            tick: 0,
            pressure_bar: 0.0,
            temp_c: 20.0,
            target_bar: 0.0,
            target_temp: 20.0,
            effective_target_temp: 20.0,
            target_ml_s: 0.0,
            flow_rate_ml_s: 0.0,
            volume_ml: 0.0,
            flow_controlled: false,
            heater_duty: 0.0,
            pump_duty: 0.0,
            ac_period_us: 10_000.0,
        }
    }
}

impl Telemetry {
    /// Group-head temperature for display: boiler temp minus offset, except in
    /// steam mode where the raw boiler temp is shown.
    pub fn display_temp(&self, offset: f32, is_steaming: bool) -> f32 {
        if is_steaming {
            self.temp_c
        } else {
            self.temp_c - offset
        }
    }
}
