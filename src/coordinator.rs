//! Central state machine: the coordinator task owns `MachineState` and is the
//! only place that decides what a `MachineCommand` does. Hardware execution
//! lives in `control.rs` and peripheral/task wiring lives in `main.rs`; this
//! module is pure transition logic so it stays easy to read and extend as
//! states/commands are added.

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};

use crate::control::{self, TargetTempMode};
use crate::settings::{FlashUpdate, Settings, SIG_FLASH_UPDATE};
use crate::state::{self, MachineCommand, MachineState, SIG_COMMAND};

// ==========================================
// POWER MANAGEMENT
// ==========================================
async fn go_to_sleep() {
    defmt::info!("Power Management: Going to SLEEP mode.");
    state::set_state(MachineState::Sleeping);
    control::set_target_temp(TargetTempMode::Off).await;
}

/// Reads the configured sleep timeout (minutes) from settings and converts
/// it to a `Duration`. Negative values are clamped to 0 (instant sleep).
async fn sleep_timeout() -> Duration {
    let minutes = Settings::get().await.machine.sleep_timeout_min;
    Duration::from_secs((minutes.max(0.0) * 60.0) as u64)
}

async fn wake_up() {
    defmt::info!("Power Management: WAKING UP.");
    state::set_state(MachineState::Idle);
    control::set_target_temp(TargetTempMode::Brew).await;
}

// ==========================================
// TRANSITION HELPERS
// ==========================================

/// Switches to `new_state`, resets shot volume, and optionally updates the
/// target temperature. Shared by every transition below.
///
/// Only signals `SIG_PROFILE_ABORT` if a hardware operation was actually in
/// flight (`is_busy()`), rather than unconditionally on every transition.
/// Unconditional signaling would leave a stale abort pending whenever we
/// start fresh from Idle — `run_cancellable` would then have to defensively
/// `reset()` it before listening, which can also discard a *real* Stop that
/// happens to land in that same window (see the race this replaced).
async fn transition_state(new_state: MachineState, target_mode: Option<TargetTempMode>) {
    let was_busy = state::get_state().is_busy();
    crate::flow_meter::FlowMonitor::new().reset_volume();
    state::set_state(new_state);
    if was_busy {
        control::SIG_PROFILE_ABORT.signal(());
    }
    if let Some(m) = target_mode {
        control::set_target_temp(m).await;
    }
}

/// Transitions to `new_state`, sets the target temperature, and dispatches
/// `hw_cmd` to the hardware task. This is the common shape of every "start an
/// operation" arm in `handle_command`; adding a new operation is a single
/// call to this helper rather than hand-rolled transition + signal code.
async fn start(
    new_state: MachineState,
    temp_mode: TargetTempMode,
    hw_cmd: control::HardwareCommand,
) {
    transition_state(new_state, Some(temp_mode)).await;
    control::SIG_HARDWARE_CMD.signal(hw_cmd);
}

async fn stop_to_idle(abort_hardware: bool) {
    let was_busy = state::get_state().is_busy();
    state::set_state(MachineState::Idle);
    if was_busy && abort_hardware {
        control::SIG_PROFILE_ABORT.signal(());
    }
    control::set_target_temp(TargetTempMode::Brew).await;
    control::set_target_pressure(0.0);
    control::set_direct_pump(None);
}

// ==========================================
// STATE MACHINE TRANSITION TABLE
// ==========================================
async fn handle_command(state: MachineState, cmd: MachineCommand) {
    use control::HardwareCommand;

    match (state, cmd) {
        // Power toggle
        (MachineState::Idle, MachineCommand::TogglePower) => {
            go_to_sleep().await;
        }
        (_, MachineCommand::TogglePower) => {
            // If busy, act as Stop
            stop_to_idle(true).await;
        }

        // Brew
        (MachineState::Idle, MachineCommand::Brew) => {
            let p = Settings::get_default_profile().await;
            start(
                MachineState::Brewing,
                TargetTempMode::Brew,
                HardwareCommand::RunProfile(p),
            )
            .await;
        }
        (MachineState::Idle, MachineCommand::RunProfile(p)) => {
            start(
                MachineState::Brewing,
                TargetTempMode::Brew,
                HardwareCommand::RunProfile(p),
            )
            .await;
        }

        // Flush
        (MachineState::Idle, MachineCommand::Flush) => {
            start(
                MachineState::Pumping,
                TargetTempMode::Brew,
                HardwareCommand::DirectPump(control::PUMP_POWER),
            )
            .await;
        }
        (MachineState::Steaming, MachineCommand::Flush) => {
            control::set_direct_pump(None);
            start(
                MachineState::Cooling,
                TargetTempMode::Brew,
                HardwareCommand::CooldownFlush,
            )
            .await;
        }

        // Steam
        (MachineState::Idle, MachineCommand::Steam) => {
            control::set_direct_pump(None);
            start(
                MachineState::Steaming,
                TargetTempMode::Steam,
                HardwareCommand::Steam,
            )
            .await;
        }
        (MachineState::Steaming, MachineCommand::Steam) => {
            stop_to_idle(true).await;
        }

        // Hot water: while steaming (wand already open by the user), Brew
        // drops the target back to brew temp and forces the pump on so hot
        // water — not steam — comes out of the wand. Valve stays closed.
        // A dedicated HotWater state ensures any button press stops it
        // cleanly (see the busy-state stop arm below) rather than
        // restarting/cycling.
        (MachineState::Steaming, MachineCommand::Brew) => {
            start(
                MachineState::HotWater,
                TargetTempMode::Brew,
                HardwareCommand::HotWater,
            )
            .await;
        }

        // Descale
        (MachineState::Idle, MachineCommand::Descale) => {
            start(
                MachineState::Descaling,
                TargetTempMode::Descale,
                HardwareCommand::Descale,
            )
            .await;
        }

        // Direct pump (dev/diagnostic, valid from any state)
        (_, MachineCommand::DirectPump(power)) => {
            start(
                MachineState::Pumping,
                TargetTempMode::Brew,
                HardwareCommand::DirectPump(power),
            )
            .await;
        }

        // Stop: button press during an active operation, explicit Stop, or
        // natural finish. `MachineState::is_busy()` is the single place that
        // defines which states count as "an operation is running" — add a
        // new busy state there and it's automatically covered here.
        (s, MachineCommand::Brew | MachineCommand::Steam | MachineCommand::Flush)
            if s.is_busy() =>
        {
            stop_to_idle(true).await;
        }
        (_, MachineCommand::Stop) => {
            stop_to_idle(true).await;
        }
        (_, MachineCommand::ProfileFinished) => {
            stop_to_idle(false).await;
        }

        // Settings (valid in any state)
        (_, MachineCommand::SaveSettings(new_s)) => {
            let old_s = Settings::get().await;
            Settings::update_ram(new_s).await;
            SIG_FLASH_UPDATE.signal(FlashUpdate::SaveSettings(old_s));
        }

        (state, cmd) => {
            // Safety catch-all: ignore invalid/dangerous commands
            defmt::warn!(
                "Invalid transition requested while in state {:?} cmd {:?}",
                state,
                cmd
            );
        }
    }
}

// ==========================================
// COORDINATOR TASK
// ==========================================
#[embassy_executor::task]
pub async fn coordinator_task() {
    let mut last_activity = embassy_time::Instant::now();

    state::set_state(MachineState::Idle);
    wake_up().await;

    loop {
        match select(SIG_COMMAND.wait(), Timer::after(Duration::from_millis(100))).await {
            Either::Second(_) => {
                if state::get_state() == MachineState::Idle
                    && last_activity.elapsed() >= sleep_timeout().await
                {
                    go_to_sleep().await;
                }
            }

            Either::First(cmd) => {
                defmt::info!("Coordinator received command: {:?}", cmd);
                last_activity = embassy_time::Instant::now();

                // Auto-wake: any command except SaveSettings wakes the machine.
                // The waking command itself is dropped — we don't want to start
                // a cold brew if the user pressed Brew just to wake it up.
                if state::get_state() == MachineState::Sleeping {
                    if let MachineCommand::SaveSettings(_) = cmd {
                        // fall through — settings save silently without waking
                    } else {
                        wake_up().await;
                        continue;
                    }
                }

                handle_command(state::get_state(), cmd).await;
            }
        }
    }
}
