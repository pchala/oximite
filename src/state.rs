use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
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
            MachineCommand::SaveMachine(_) => defmt::write!(fmt, "SaveMachine"),
            MachineCommand::SavePids(_, _) => defmt::write!(fmt, "SavePids"),
            MachineCommand::SaveWifi(_) => defmt::write!(fmt, "SaveWifi"),
            MachineCommand::TogglePower => defmt::write!(fmt, "TogglePower"),
            MachineCommand::SetSessionTemp(t) => defmt::write!(fmt, "SetSessionTemp({})", t),
        }
    }
}

/// Commands headed for `coordinator::coordinator_task`.
///
/// This is a queue, not a `Signal`, because it has several independent
/// producers — the button task on core0 and the web API on core1 — and every
/// one of their commands means something different. A `Signal` holds a single
/// slot and overwrites it, so two commands issued before the coordinator was
/// scheduled would silently collapse into one, and a genuinely concurrent
/// core1 request could destroy a button press outright. Ordering matters too:
/// a `Stop` followed by a start command must arrive as two separate events.
///
/// Depth 4 covers the realistic worst case — all four buttons resolving in one
/// debounce pass — without spending much RAM on the large `RunProfile` variant.
static COMMAND_QUEUE: Channel<CriticalSectionRawMutex, MachineCommand, 4> = Channel::new();

/// Queues a command for the coordinator, dropping it if the queue is full.
///
/// Deliberately non-blocking: the callers are the button poller and HTTP
/// request handlers, which must not stall waiting on the coordinator. A full
/// queue means the coordinator is wedged, which the warning surfaces instead
/// of hiding.
pub fn send_command(cmd: MachineCommand) {
    if COMMAND_QUEUE.try_send(cmd).is_err() {
        defmt::warn!("Command queue full — command dropped");
    }
}

/// Waits for the next queued command. Single-consumer: only the coordinator
/// calls this.
pub async fn next_command() -> MachineCommand {
    COMMAND_QUEUE.receive().await
}

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
