use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use portable_atomic::{AtomicU32, AtomicU8};

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, defmt::Format, IntoPrimitive, TryFromPrimitive)]
pub enum MachineState {
    Idle = 0,
    Brewing = 1,
    Steaming = 2,
    Descaling = 3,
    Sleeping = 4,
    Pumping = 5,
    Cooling = 6,
    HotWater = 7,
}

impl MachineState {
    /// States representing an active hardware operation (profile/steam/pump
    /// running). Used to decide whether a Stop-like command should abort the
    /// current operation. Update this in one place when adding new states.
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            MachineState::Brewing
                | MachineState::Pumping
                | MachineState::Cooling
                | MachineState::Descaling
                | MachineState::HotWater
                | MachineState::Steaming
        )
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum MachineCommand {
    RunProfile(crate::profiles::BrewProfile),
    Brew,
    Stop,
    Steam,
    Flush,
    Descale,
    DirectPump(f32),
    ProfileFinished, // Sent by hardware when it finishes naturally
    SaveMachine(crate::settings::MachineSettings),
    SavePids(crate::settings::PidSettings, crate::settings::PidSettings),
    SaveWifi(crate::settings::WifiSettings),
    TogglePower,
    SetSessionTemp(f32),
}

impl defmt::Format for MachineCommand {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            MachineCommand::RunProfile(_) => defmt::write!(fmt, "RunProfile"),
            MachineCommand::Brew => defmt::write!(fmt, "Brew"),
            MachineCommand::Stop => defmt::write!(fmt, "Stop"),
            MachineCommand::Steam => defmt::write!(fmt, "Steam"),
            MachineCommand::Flush => defmt::write!(fmt, "Flush"),
            MachineCommand::Descale => defmt::write!(fmt, "Descale"),
            MachineCommand::DirectPump(p) => defmt::write!(fmt, "DirectPump({})", p),
            MachineCommand::ProfileFinished => defmt::write!(fmt, "ProfileFinished"),
            MachineCommand::SaveMachine(_) => defmt::write!(fmt, "SaveMachine"),
            MachineCommand::SavePids(_, _) => defmt::write!(fmt, "SavePids"),
            MachineCommand::SaveWifi(_) => defmt::write!(fmt, "SaveWifi"),
            MachineCommand::TogglePower => defmt::write!(fmt, "TogglePower"),
            MachineCommand::SetSessionTemp(t) => defmt::write!(fmt, "SetSessionTemp({})", t),
        }
    }
}

pub static SIG_COMMAND: Signal<CriticalSectionRawMutex, MachineCommand> = Signal::new();

/// Commands the coordinator dispatches to `operations::hardware_task`, which
/// owns the solenoid valve and runs the actual hardware sequences. Kept next to
/// `MachineCommand` because both are inter-task command channels; `operations`
/// consumes this one rather than defining it.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum HardwareCommand {
    RunProfile(crate::profiles::BrewProfile),
    Steam,
    Descale,
    DirectPump(f32),
    CooldownFlush,
    HotWater,
}

pub static SIG_HARDWARE_CMD: Signal<CriticalSectionRawMutex, HardwareCommand> = Signal::new();

/// Cancellation channel for whatever hardware operation is currently running.
/// The coordinator signals it on Stop or on a transition out of a busy state;
/// `operations::HardwareExecutor` races every operation against it.
pub static SIG_PROFILE_ABORT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Everything the UI and the LED task need to render the machine, published
/// as one snapshot. Written by two producers: `adc::adc_task` fills in the
/// measured `pressure_bar`/`temp_c`, and `control::ac_sync_control_task` fills
/// in the setpoints and heater duty it derives from them. Both run on core0's
/// executor and neither awaits between reading and sending, so the
/// read-modify-write is atomic with respect to the other.
#[derive(Clone, Copy, Default)]
pub struct Telemetry {
    pub pressure_bar: f32,
    pub temp_c: f32,
    pub target_bar: f32,
    pub target_temp: f32,
    pub flow_limit_ml_s: f32,
    pub heater_duty: f32,
}

impl Telemetry {
    /// Returns `(display_temp, display_target_temp)` with the boiler offset
    /// subtracted for non-steam modes.
    pub fn display_temps(&self, offset: f32, is_steaming: bool) -> (f32, f32) {
        if is_steaming {
            (self.temp_c, self.target_temp)
        } else {
            let t = self.temp_c - offset;
            let tt = if self.target_temp > 0.0 {
                self.target_temp - offset
            } else {
                0.0
            };
            (t, tt)
        }
    }
}

pub static TELEMETRY_WATCH: Watch<CriticalSectionRawMutex, Telemetry, 4> = Watch::new();

/// The latest telemetry snapshot, or a plausible cold-machine reading if no
/// producer has published yet — callers would otherwise see a 0 °C boiler for
/// the first few milliseconds after boot and act on it.
pub fn get_telemetry() -> Telemetry {
    TELEMETRY_WATCH.try_get().unwrap_or(Telemetry {
        pressure_bar: 0.0,
        temp_c: 20.0,
        target_bar: 0.0,
        target_temp: 20.0,
        flow_limit_ml_s: 0.0,
        heater_duty: 0.0,
    })
}

// The Watch channel acts as our centralized, broadcasted state for tasks that want notifications.
pub static MACHINE_STATE: Watch<CriticalSectionRawMutex, MachineState, 4> = Watch::new();
static CURRENT_STATE: AtomicU8 = AtomicU8::new(0); // 0 = Idle

pub fn get_state() -> MachineState {
    // The raw value is only ever written via set_state(), so it always
    // matches a valid discriminant; Idle is a safe fallback regardless.
    MachineState::try_from_primitive(CURRENT_STATE.load(Ordering::Relaxed))
        .unwrap_or(MachineState::Idle)
}

pub fn set_state(state: MachineState) {
    let old_state = get_state();
    if old_state != state {
        defmt::info!("State Change: {:?} -> {:?}", old_state, state);
        CURRENT_STATE.store(state.into(), Ordering::Relaxed);
        MACHINE_STATE.sender().send(state);
    }
}

pub static SESSION_BREW_TEMP: AtomicU32 = AtomicU32::new(0);

pub fn get_session_brew_temp() -> f32 {
    f32::from_bits(SESSION_BREW_TEMP.load(Ordering::Relaxed))
}

pub fn set_session_brew_temp(temp: f32) {
    SESSION_BREW_TEMP.store(temp.to_bits(), Ordering::Relaxed);
}
