use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use portable_atomic::AtomicU8;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, defmt::Format)]
pub enum MachineState {
    Idle = 0,
    Brewing = 1,
    Steaming = 2,
    Descaling = 3,
    Sleeping = 4,
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
    SaveSettings(crate::settings::SettingsManager),
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
        }
    }
}

pub static SIG_COMMAND: Signal<CriticalSectionRawMutex, MachineCommand> = Signal::new();

// The Watch channel acts as our centralized, broadcasted state for tasks that want notifications.
pub static MACHINE_STATE: Watch<CriticalSectionRawMutex, MachineState, 4> = Watch::new();
static CURRENT_STATE: AtomicU8 = AtomicU8::new(0); // 0 = Idle

pub fn get_state() -> MachineState {
    match CURRENT_STATE.load(Ordering::Relaxed) {
        1 => MachineState::Brewing,
        2 => MachineState::Steaming,
        3 => MachineState::Descaling,
        4 => MachineState::Sleeping,
        _ => MachineState::Idle,
    }
}

pub fn set_state(state: MachineState) {
    let old_state = get_state();
    if old_state != state {
        defmt::info!("State Change: {:?} -> {:?}", old_state, state);
        CURRENT_STATE.store(state as u8, Ordering::Relaxed);
        MACHINE_STATE.sender().send(state);
    }
}
