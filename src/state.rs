use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use portable_atomic::AtomicU8;

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
        )
    }
}

#[derive(Clone)]
pub enum MachineCommand {
    RunProfile(crate::settings::BrewProfile),
    Brew,
    Stop,
    Steam,
    Flush,
    Descale,
    DirectPump(f32),
    ProfileFinished, // Sent by hardware when it finishes naturally
    SaveSettings(crate::settings::Settings),
    TogglePower,
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
            MachineCommand::SaveSettings(_) => defmt::write!(fmt, "SaveSettings"),
            MachineCommand::TogglePower => defmt::write!(fmt, "TogglePower"),
        }
    }
}

pub static SIG_COMMAND: Signal<CriticalSectionRawMutex, MachineCommand> = Signal::new();

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
