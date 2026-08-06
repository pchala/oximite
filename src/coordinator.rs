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
//! Long operations stay responsive because a single `select` awaits the
//! operation, the command queue and a housekeeping tick together, so a long
//! steam session never blocks a Stop. There is exactly one command path: when
//! nothing is running the operation slot holds a future that never completes,
//! so the idle and busy cases are the same code rather than two loops that can
//! drift apart. Peripheral/task wiring lives in `main.rs`.

use embassy_futures::select::{select3, Either3};
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Instant, Ticker};

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

/// What a command means for the *running operation*, kept separate from what
/// it means for `MachineState`.
///
/// Saying this out loud is what stops the two from disagreeing. The
/// coordinator used to infer cancellation by comparing `MachineState` before
/// and after `handle_command`, so any future transition that moved between two
/// busy states would have looked like a cancellation: the operation would be
/// dropped while the state still claimed the machine was busy, leaving it
/// wedged with no pump, no valve and no timeout. That is now unrepresentable —
/// a transition into a busy state has to hand back the operation that serves
/// it.
///
/// `Start` inherits `Operation`'s ~360 bytes, but like `Operation` this is
/// only ever a local in the coordinator's future — never queued, never copied
/// per-message — so it costs one slot of task stack rather than N slots of
/// queue.
#[allow(clippy::large_enum_variant)]
enum Outcome {
    /// Leave the operation slot alone. Either the command was ambient, or the
    /// machine was idle and there was nothing to disturb.
    Continue,
    /// The table moved the machine out of its busy state: the running
    /// operation is over and must be dropped.
    Cancel,
    /// Run this, replacing anything already running.
    Start(Operation),
}

/// Enters `new_state` and resets shot volume, then hands `op` back for the
/// caller to run. This is the shape of every "start an operation" arm.
async fn start(new_state: MachineState, op: Operation) -> Outcome {
    crate::flow_meter::reset_volume();
    enter(new_state).await;
    Outcome::Start(op)
}

/// Returns to Idle.
///
/// Note what this deliberately does *not* do: cancel the running operation.
/// The caller does that — the busy arm of `handle_command` pairs this with
/// `Outcome::Cancel`, and `coordinator_task` drops the operation future in
/// response, unwinding its pump and valve guards. Setting the pump idle here
/// is therefore belt-and-braces, not the mechanism. Volume is left alone on
/// purpose, so the shot total stays readable.
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
/// Split out because this is exactly the set that has to survive an operation:
/// saving settings or nudging the session temperature mid-shot must be applied
/// without cancelling the shot.
async fn handle_ambient(cmd: &MachineCommand) -> bool {
    match cmd {
        MachineCommand::SaveMachine(m) => {
            let mut s = Settings::get().await;
            s.machine = *m;
            Settings::update_ram(s).await;
            SIG_FLASH_UPDATE.signal(FlashUpdate::SaveMachine(*m));
        }
        MachineCommand::SavePids(t, p, f) => {
            let mut s = Settings::get().await;
            s.temp_pid = *t;
            s.press_pid = *p;
            if let Some(f) = f {
                s.flow_pid = *f;
            }
            Settings::update_ram(s).await;
            SIG_FLASH_UPDATE.signal(FlashUpdate::SavePids(*t, *p, *f));
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
/// what should happen to the running operation. Acting on that is the caller's
/// job.
async fn handle_command(state: MachineState, cmd: MachineCommand) -> Outcome {
    if handle_ambient(&cmd).await {
        return Outcome::Continue;
    }

    match (state, cmd) {
        // --- Idle: the only state an operation can start from ---
        // Nothing is running here, so the arms that don't start anything are
        // `Continue` rather than `Cancel`.
        (MachineState::Idle, MachineCommand::TogglePower) => {
            go_to_sleep().await;
            Outcome::Continue
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
            Outcome::Continue
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
            Outcome::Cancel
        }
        (state, cmd) => {
            // Unreachable in practice — Idle handles every command above, and
            // Sleeping is intercepted by the auto-wake in `serve`. Kept for
            // exhaustiveness so a new state or command shows up in the log
            // instead of failing to compile somewhere subtler.
            defmt::warn!(
                "Invalid transition requested while in state {:?} cmd {:?}",
                state,
                cmd
            );
            Outcome::Continue
        }
    }
}

// ==========================================
// COORDINATOR TASK
// ==========================================

/// Housekeeping period.
///
/// A `Ticker`, not a `Timer`: it keeps its own deadline, so it fires at a
/// fixed rate rather than 100 ms after whatever last woke the loop, and a
/// burst of commands cannot starve it.
///
/// This ticks whether or not an operation is in flight, so anything
/// supervisory added here keeps running during a shot. The previous structure
/// had this timer in the idle-only loop, where it silently stopped while the
/// machine was hot and pumping — the exact window a safety check would need.
const HOUSEKEEPING: Duration = Duration::from_millis(100);

/// The operation slot as a single future, so idle and busy share one `select`.
///
/// `None` becomes a future that never completes, which is precisely what an
/// idle machine is: something that only the command queue can move.
async fn operation_or_idle(op: Option<Operation>, valve: &mut Output<'static>) {
    match op {
        Some(op) => operations::execute(op, valve).await,
        None => core::future::pending::<()>().await,
    }
}

/// Everything a command gets regardless of what the machine is doing:
/// activity tracking, the sleep auto-wake, then the transition table.
///
/// One path, so the idle and busy cases cannot drift apart. The previous
/// two-loop structure updated `last_activity` and honoured the auto-wake in
/// only one of them, and the other was correct only because `Sleeping` happens
/// not to be in `MachineState::is_busy()` — an invariant held in another file.
async fn serve(cmd: MachineCommand, last_activity: &mut Instant) -> Outcome {
    defmt::info!("Coordinator received command: {:?}", cmd);
    *last_activity = Instant::now();

    // Auto-wake: any command except a settings save wakes the machine.
    // The waking command itself is dropped — we don't want to start a cold
    // brew if the user pressed Brew just to wake it up.
    if state::get_state() == MachineState::Sleeping {
        match cmd {
            MachineCommand::SaveMachine(_)
            | MachineCommand::SavePids(_, _, _)
            | MachineCommand::SaveWifi(_)
            // Adjusting the brew target must not fire up the boiler, and
            // must not be discarded either — it is stored and takes effect
            // on the next wake, when `enter()` reads it.
            | MachineCommand::SetSessionTemp(_) => {
                // fall through — these apply silently without waking
            }
            _ => {
                wake_up().await;
                return Outcome::Continue;
            }
        }
    }

    handle_command(state::get_state(), cmd).await
}

/// Owns `MachineState`, the valve, and the one operation that may be running.
///
/// `handle_command` stays the single authority on what a command means. Only
/// the ambient commands (`handle_ambient`: settings saves and
/// `SetSessionTemp`) leave a running operation alone — they are consumed
/// without touching the state, so a mid-shot save applies while the shot keeps
/// pouring. Every other command arriving in a busy state either takes one of
/// `Steaming`'s two onward transitions or falls to the `is_busy()` arm, which
/// stops the machine; that includes a stray `RunProfile` from the web UI,
/// which aborts the running shot rather than being ignored.
///
/// Cancelling *is* dropping `running`; its `PumpGuard`, `BrewActiveGuard` and
/// `SolenoidGuard` unwind the hardware to a safe state. That drop happens when
/// the outer loop body ends, so a replacement operation is never constructed
/// until the previous one has fully released the pump and valve.
#[embassy_executor::task]
pub async fn coordinator_task(valve: Output<'static>) {
    let mut valve = valve;
    // Assigned at the top of every outer iteration, which is also the point
    // an operation ends — see the comment there.
    let mut last_activity;
    let mut current: Option<Operation> = None;
    let mut housekeeping = Ticker::every(HOUSEKEEPING);

    let initial_temp = crate::settings::ControlSettings::current()
        .machine
        .brew_temp;
    state::set_session_brew_temp(initial_temp);
    // Applies the Idle setpoint now that the session temp is known. `enter()`
    // is the only writer of MachineState, so this is also what establishes it.
    wake_up().await;

    loop {
        // Restart the sleep countdown from the *end* of the previous
        // operation — a long steam session would otherwise finish with a
        // stale timestamp and drop straight into sleep.
        last_activity = Instant::now();

        let name = current.as_ref().map(|op| op.name());
        if let Some(name) = name {
            defmt::info!("Operation: {} started", name);
        }

        // Pinned once and reborrowed by every `select3` below, so neither a
        // command nor a housekeeping tick disturbs the operation in flight.
        let mut running = core::pin::pin!(operation_or_idle(current.take(), &mut valve));

        loop {
            match select3(
                running.as_mut(),
                state::next_command(),
                housekeeping.next(),
            )
            .await
            {
                // Only reachable with an operation in the slot: the idle slot
                // never completes.
                Either3::First(()) => {
                    if let Some(name) = name {
                        defmt::info!("Operation: {} finished naturally", name);
                    }
                    stop_to_idle().await;
                    break;
                }
                Either3::Second(cmd) => match serve(cmd, &mut last_activity).await {
                    Outcome::Continue => continue,
                    Outcome::Cancel => break,
                    Outcome::Start(next) => {
                        current = Some(next);
                        break;
                    }
                },
                // Auto-sleep. The `Idle` check short-circuits before
                // `sleep_timeout()` reads settings, so this stays cheap while
                // an operation is running.
                Either3::Third(()) => {
                    if state::get_state() == MachineState::Idle
                        && last_activity.elapsed() >= sleep_timeout().await
                    {
                        go_to_sleep().await;
                    }
                }
            }
        }
    }
}

