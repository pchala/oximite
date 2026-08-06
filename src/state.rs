use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::watch::Watch;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use portable_atomic::{AtomicU32, AtomicU8};

/// Discriminants are a wire contract with the web UI: `web_api` sends the raw
/// value as telemetry `st`, and `index.html` indexes a positional name array
/// with it. The HTML is gzipped into the firmware by `build.rs` and the value
/// is never persisted to flash, so the two always ship together — but renumber
/// both sides in the same commit.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, defmt::Format, IntoPrimitive, TryFromPrimitive)]
pub enum MachineState {
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
    /// running). The coordinator treats this as "an operation is in flight",
    /// so any command that isn't one of `Steaming`'s two onward transitions
    /// stops the machine. Update this in one place when adding new states.
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
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum MachineCommand {
    RunProfile(crate::profiles::BrewProfile),
    Brew,
    Stop,
    Steam,
    Flush,
    DirectPump(f32),
    SaveMachine(crate::settings::MachineSettings),
    SavePids(
        crate::settings::PidSettings,
        crate::settings::PidSettings,
        Option<crate::settings::PidSettings>,
    ),
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
            MachineCommand::DirectPump(p) => defmt::write!(fmt, "DirectPump({})", p),
            MachineCommand::SaveMachine(_) => defmt::write!(fmt, "SaveMachine"),
            MachineCommand::SavePids(_, _, _) => defmt::write!(fmt, "SavePids"),
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

/// Filtered analog readings, published by `adc::adc_task`.
#[derive(Clone, Copy)]
pub struct SensorReading {
    pub pressure_bar: f32,
    pub temp_c: f32,
}

impl Default for SensorReading {
    fn default() -> Self {
        // Not derived: a 0 °C boiler is a huge error to the temperature PID,
        // and this default is what the control loop reads on the first ticks
        // after boot, before the ADC has published anything.
        Self {
            pressure_bar: 0.0,
            temp_c: 20.0,
        }
    }
}

pub static SENSOR_WATCH: Watch<CriticalSectionRawMutex, SensorReading, 4> = Watch::new();

/// The latest filtered sensor reading, or a plausible cold-machine reading if
/// the ADC has not published yet.
pub fn get_sensors() -> SensorReading {
    SENSOR_WATCH.try_get().unwrap_or_default()
}

/// Everything the UI and the LED task need to render the machine, published
/// as one snapshot by `control::ac_sync_control_task`, once per 50 Hz control
/// tick. Single producer: the measured values are copied in from
/// `SENSOR_WATCH` rather than written by the ADC task directly, so every field
/// in a given snapshot describes the same tick.
#[derive(Clone, Copy, Default)]
pub struct Telemetry {
    /// Control-loop tick this snapshot describes. Lets a consumer tell a
    /// duplicate sample from a fresh one, and count frames it never saw.
    pub tick: u32,
    pub pressure_bar: f32,
    pub temp_c: f32,
    /// Setpoint the profile asked for. 0 whenever the pump is under flow
    /// control, which ignores pressure entirely.
    pub target_bar: f32,
    /// Setpoint the pressure PID chased this tick, or 0 when the pressure loop
    /// isn't running (idle, direct pump, or flow control).
    pub effective_target_bar: f32,
    pub target_temp: f32,
    /// `target_temp` plus the brew-flow feed-forward — what the temperature PID
    /// actually chased. Diverges from `target_temp` by several degrees during a
    /// shot, which otherwise reads as unexplained overshoot.
    pub effective_target_temp: f32,
    pub flow_limit_ml_s: f32,
    /// True while the flow PID owns the pump. Without it a log can't tell a
    /// flow-controlled step from a pressure step that happens to be flowing.
    pub flow_controlled: bool,
    pub heater_duty: f32,
    /// Triac duty the pump was driven at this tick, 0-100.
    pub pump_duty: f32,
}

impl Telemetry {
    /// Returns `(display_temp, display_target_temp, display_effective_target)`
    /// with the boiler offset subtracted for non-steam modes.
    pub fn display_temps(&self, offset: f32, is_steaming: bool) -> (f32, f32, f32) {
        if is_steaming {
            (self.temp_c, self.target_temp, self.effective_target_temp)
        } else {
            let adj = |v: f32| if v > 0.0 { v - offset } else { 0.0 };
            (
                self.temp_c - offset,
                adj(self.target_temp),
                adj(self.effective_target_temp),
            )
        }
    }
}

pub static TELEMETRY_WATCH: Watch<CriticalSectionRawMutex, Telemetry, 4> = Watch::new();

/// The latest telemetry snapshot, or a plausible cold-machine reading if no
/// producer has published yet — callers would otherwise see a 0 °C boiler for
/// the first few milliseconds after boot and act on it.
pub fn get_telemetry() -> Telemetry {
    TELEMETRY_WATCH.try_get().unwrap_or(Telemetry {
        tick: 0,
        pressure_bar: 0.0,
        temp_c: 20.0,
        target_bar: 0.0,
        effective_target_bar: 0.0,
        target_temp: 20.0,
        effective_target_temp: 20.0,
        flow_limit_ml_s: 0.0,
        flow_controlled: false,
        heater_duty: 0.0,
        pump_duty: 0.0,
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
