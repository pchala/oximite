//! Central state machine: the coordinator task owns `MachineState` and is the
//! only place that decides what a `MachineCommand` does.
//!
//! It also *runs* the operation it decided on, awaiting `operations::execute`
//! directly while racing it against the command queue. That structure is what
//! keeps the machine honest: an operation finishing is a future returning, and
//! cancelling one is dropping that future. There is no "operation finished"
//! message that could be delivered after the coordinator moved on, and no
//! abort signal that could outlive its target — both were previously possible.
//!
//! Long operations stay responsive because the same `select` that awaits the
//! operation also serves commands, so a long steam session never blocks a Stop.
//! Peripheral/task wiring lives in `main.rs`.

use embassy_futures::select::{select, Either};
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Timer};

use crate::control::{self, TargetTempMode};
use crate::operations::{self, Operation};
use crate::settings::{FlashUpdate, Settings, SIG_FLASH_UPDATE};
use crate::state::{self, MachineCommand, MachineState};

// ==========================================
// POWER MANAGEMENT
// ==========================================

/// The heater setpoint implied by a machine state.
///
/// Making this a function rather than a parameter is what keeps the setpoint
/// and the state from ever disagreeing. It also means an operation can be
/// cancelled at any point without leaving a stale target behind: the setpoint
/// belongs to the state, and the state is always restored.
fn target_for(state: MachineState) -> TargetTempMode {
    match state {
        MachineState::Steaming => TargetTempMode::Steam,
        // Cooling deliberately runs the heater off — it is fighting the
        // incoming cold water otherwise — and a sleeping machine has no
        // reason to heat at all.
        MachineState::Cooling | MachineState::Sleeping => TargetTempMode::Off,
        MachineState::Idle
        | MachineState::Brewing
        | MachineState::Pumping
        | MachineState::HotWater => TargetTempMode::Brew,
    }
}

/// The only way the machine changes state. Applies the setpoint the new state
/// implies, so no caller can pick one that contradicts it.
async fn enter(state: MachineState) {
    state::set_state(state);
    control::set_target_temp(target_for(state)).await;
}

async fn go_to_sleep() {
    defmt::info!("Power Management: Going to SLEEP mode.");
    enter(MachineState::Sleeping).await;
}

/// Reads the configured sleep timeout (minutes) from settings and converts
/// it to a `Duration`. Negative values are clamped to 0 (instant sleep).
async fn sleep_timeout() -> Duration {
    let minutes = Settings::get().await.machine.sleep_timeout_min;
    Duration::from_secs((minutes.max(0.0) * 60.0) as u64)
}

async fn wake_up() {
    defmt::info!("Power Management: WAKING UP.");
    enter(MachineState::Idle).await;
}

// ==========================================
// TRANSITION HELPERS
// ==========================================

/// Enters `new_state` and resets shot volume, then hands `op` back for the
/// caller to run. This is the shape of every "start an operation" arm.
async fn start(new_state: MachineState, op: Operation) -> Option<Operation> {
    crate::flow_meter::reset_volume();
    enter(new_state).await;
    Some(op)
}

/// Returns to Idle.
///
/// Note what this deliberately does *not* do: cancel the running operation.
/// Moving out of a busy state is *itself* the cancellation signal — see
/// `run_operation`, which observes the state change and drops the operation
/// future right after this returns, unwinding its pump and valve guards.
/// Setting the pump idle here is therefore belt-and-braces, not the mechanism.
/// Volume is left alone on purpose, so the shot total stays readable.
async fn stop_to_idle() {
    enter(MachineState::Idle).await;
    control::PumpMode::Idle.apply();
}

// ==========================================
// STATE MACHINE TRANSITION TABLE
// ==========================================

/// Commands that are valid in any state and must never disturb a running
/// operation. Returns true if the command was consumed.
///
/// Split out because `run_operation` needs exactly this set: saving settings
/// or nudging the session temperature mid-shot has to be applied without
/// cancelling the shot.
async fn handle_ambient(cmd: &MachineCommand) -> bool {
    match cmd {
        MachineCommand::SaveMachine(m) => {
            let mut s = Settings::get().await;
            s.machine = *m;
            Settings::update_ram(s).await;
            SIG_FLASH_UPDATE.signal(FlashUpdate::SaveMachine(*m));
        }
        MachineCommand::SavePids(t, p) => {
            let mut s = Settings::get().await;
            s.temp_pid = *t;
            s.press_pid = *p;
            Settings::update_ram(s).await;
            SIG_FLASH_UPDATE.signal(FlashUpdate::SavePids(*t, *p));
        }
        MachineCommand::SaveWifi(w) => {
            defmt::info!("Settings: New SSID: {}", w.ssid.as_str());
            let mut s = Settings::get().await;
            s.wifi = w.clone();
            Settings::update_ram(s).await;
            SIG_FLASH_UPDATE.signal(FlashUpdate::SaveWifi(w.clone()));
        }
        MachineCommand::SetSessionTemp(t) => {
            state::set_session_brew_temp(*t);
            // Apply instantly, but only if the current state actually wants
            // brew temp — `Steaming` and `Cooling` have their own targets and
            // must not be overridden.
            if matches!(target_for(state::get_state()), TargetTempMode::Brew) {
                control::set_target_temp(TargetTempMode::Brew).await;
            }
        }
        _ => return false,
    }
    true
}

/// Decides what `cmd` means in `state`, applying the transition and returning
/// the operation to run (if any). Running it is the caller's job.
async fn handle_command(state: MachineState, cmd: MachineCommand) -> Option<Operation> {
    if handle_ambient(&cmd).await {
        return None;
    }

    match (state, cmd) {
        // --- Idle: the only state an operation can start from ---
        (MachineState::Idle, MachineCommand::TogglePower) => {
            go_to_sleep().await;
            None
        }
        (MachineState::Idle, MachineCommand::Brew) => {
            let p = crate::profiles::get_default_profile().await;
            start(MachineState::Brewing, Operation::Profile(p)).await
        }
        (MachineState::Idle, MachineCommand::RunProfile(p)) => {
            start(MachineState::Brewing, Operation::Profile(p)).await
        }
        (MachineState::Idle, MachineCommand::Steam) => {
            control::PumpMode::Idle.apply();
            start(MachineState::Steaming, Operation::Steam).await
        }
        (MachineState::Idle, MachineCommand::Flush) => {
            start(
                MachineState::Pumping,
                Operation::DirectPump(operations::PUMP_POWER),
            )
            .await
        }
        (MachineState::Idle, MachineCommand::DirectPump(power)) => {
            start(MachineState::Pumping, Operation::DirectPump(power)).await
        }
        // Stop when already stopped: harmless resync, not a warning.
        (MachineState::Idle, MachineCommand::Stop) => {
            stop_to_idle().await;
            None
        }

        // --- Steaming: the one state that modifies rather than cancels ---
        // The user has the wand open; Brew means "hot water out of the wand,
        // not steam", and Flush means "cool the boiler back down". Both get a
        // state of their own so the next button press stops them cleanly via
        // the busy arm below rather than cycling.
        (MachineState::Steaming, MachineCommand::Brew) => {
            start(MachineState::HotWater, Operation::HotWater).await
        }
        (MachineState::Steaming, MachineCommand::Flush) => {
            control::PumpMode::Idle.apply();
            start(MachineState::Cooling, Operation::CooldownFlush).await
        }

        // --- Any other command while busy stops the machine ---
        // Nothing is silently ignored: if the machine is doing something and
        // you ask for anything the two arms above don't cover, it stops.
        // MachineState::is_busy() is the single definition of "an operation
        // is running", so a new busy state is covered here automatically.
        (s, _) if s.is_busy() => {
            stop_to_idle().await;
            None
        }
        (state, cmd) => {
            // Unreachable in practice — Idle handles every command above, and
            // Sleeping is intercepted by the auto-wake in `coordinator_task`.
            // Kept for exhaustiveness so a new state or command shows up in
            // the log instead of failing to compile somewhere subtler.
            defmt::warn!(
                "Invalid transition requested while in state {:?} cmd {:?}",
                state,
                cmd
            );
            None
        }
    }
}

// ==========================================
// COORDINATOR TASK
// ==========================================

/// Runs `op` — and any operation that replaces it — until the machine leaves
/// the busy state.
///
/// Commands keep being served while the operation is in flight, and
/// `handle_command` stays the single authority on what each one means: the
/// operation is cancelled exactly when the table moves `MachineState` out from
/// under it. A command the table *ignores* in this state (a stray `RunProfile`
/// from the web UI mid-shot, say) leaves the state alone and the shot keeps
/// pouring. That is the same rule the old signal-based design followed — there,
/// an abort could only be signalled from inside a state transition — except
/// that here it is structural instead of a signal that could outlive its
/// target.
///
/// Cancelling *is* dropping `running`; its `PumpGuard`, `BrewActiveGuard` and
/// `SolenoidGuard` unwind the hardware to a safe state. That drop happens at
/// the end of the inner block, so a replacement operation is never constructed
/// until the previous one has fully released the pump and valve.
async fn run_operation(op: Operation, valve: &mut Output<'static>) {
    let mut op = op;
    loop {
        let name = op.name();
        defmt::info!("Operation: {} started", name);

        let replacement = {
            let mut running = core::pin::pin!(operations::execute(op, valve));
            loop {
                match select(running.as_mut(), state::next_command()).await {
                    Either::First(()) => {
                        defmt::info!("Operation: {} finished naturally", name);
                        stop_to_idle().await;
                        return;
                    }
                    Either::Second(cmd) => {
                        defmt::info!("Coordinator received command: {:?}", cmd);
                        let before = state::get_state();
                        match handle_command(before, cmd).await {
                            // Table asked for a different operation.
                            Some(next) => break next,
                            // Consumed without leaving this state (a settings
                            // save, or a command that does not apply here) —
                            // the operation is untouched and continues.
                            None if state::get_state() == before => continue,
                            // Transitioned out: the operation is over.
                            None => return,
                        }
                    }
                }
            }
        };

        op = replacement;
    }
}

#[embassy_executor::task]
pub async fn coordinator_task(valve: Output<'static>) {
    let mut valve = valve;
    let mut last_activity = embassy_time::Instant::now();

    let initial_temp = crate::settings::ControlSettings::current()
        .machine
        .brew_temp;
    state::set_session_brew_temp(initial_temp);
    // Applies the Idle setpoint now that the session temp is known. `enter()`
    // is the only writer of MachineState, so this is also what establishes it.
    wake_up().await;

    loop {
        let cmd = match select(
            state::next_command(),
            Timer::after(Duration::from_millis(100)),
        )
        .await
        {
            Either::First(cmd) => cmd,
            Either::Second(_) => {
                if state::get_state() == MachineState::Idle
                    && last_activity.elapsed() >= sleep_timeout().await
                {
                    go_to_sleep().await;
                }
                continue;
            }
        };

        defmt::info!("Coordinator received command: {:?}", cmd);
        last_activity = embassy_time::Instant::now();

        // Auto-wake: any command except a settings save wakes the machine.
        // The waking command itself is dropped — we don't want to start a cold
        // brew if the user pressed Brew just to wake it up.
        if state::get_state() == MachineState::Sleeping {
            match cmd {
                MachineCommand::SaveMachine(_)
                | MachineCommand::SavePids(_, _)
                | MachineCommand::SaveWifi(_)
                // Adjusting the brew target must not fire up the boiler, and
                // must not be discarded either — it is stored and takes effect
                // on the next wake, when `enter()` reads it.
                | MachineCommand::SetSessionTemp(_) => {
                    // fall through — these apply silently without waking
                }
                _ => {
                    wake_up().await;
                    continue;
                }
            }
        }

        if let Some(op) = handle_command(state::get_state(), cmd).await {
            run_operation(op, &mut valve).await;
            // Restart the sleep countdown from the *end* of the operation —
            // a long steam session would otherwise finish with a stale
            // timestamp and drop straight into sleep.
            last_activity = embassy_time::Instant::now();
        }
    }
}
