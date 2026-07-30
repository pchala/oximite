//! Machine operations (brew profile, steam, descale, cooldown flush, hot
//! water, raw pump) and the task that executes them.
//!
//! Each operation is a plain cancellable future built from the control-loop
//! primitives in `control.rs`; `hardware_task` owns the solenoid valve and
//! runs whichever one the coordinator asks for, racing it against
//! `SIG_PROFILE_ABORT`.

use embassy_futures::select::{select, Either};
use embassy_futures::yield_now;
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Timer};

use crate::control::{set_target_temp, BrewActiveGuard, PumpGuard, PumpMode, TargetTempMode};
use crate::profiles::BrewProfile;
use crate::settings::Settings;
use crate::state::{
    HardwareCommand, MachineCommand, SIG_COMMAND, SIG_HARDWARE_CMD, SIG_PROFILE_ABORT,
};

/// Pump power used for flush and cooldown operations (%).
pub const PUMP_POWER: f32 = 80.0;

pub async fn execute_profile(profile: BrewProfile) {
    defmt::info!("Executing profile: {}", profile.name.as_str());
    let mut pump = PumpGuard::engage(PumpMode::Idle);
    let _brew_active = BrewActiveGuard::engage();

    // Yield once so the flow task can process the SIG_RESET_VOLUME signal
    yield_now().await;

    for (i, step) in profile.steps.iter().enumerate() {
        let mut time_s = step.time_s.unwrap_or(120.0);
        let volume = step.volume.unwrap_or(0.0);
        let pressure = step.pressure.unwrap_or(0.0);
        let flow = step.flow.unwrap_or(0.0);

        if time_s == 0.0 && volume == 0.0 {
            continue;
        }

        if time_s == 0.0 {
            time_s = 120.0;
        }

        defmt::info!(
            "Step {}: P={}bar, F={}ml/s, T={}s, V={}ml",
            i,
            pressure,
            flow,
            time_s,
            volume
        );

        if pressure > 10.0 {
            pump.set_mode(PumpMode::DirectPump(pressure));
        } else {
            pump.set_mode(PumpMode::Pressure {
                bar: pressure,
                flow_limit_ml_s: flow,
            });
        }

        let time_fut = async {
            if time_s > 0.0 {
                Timer::after(Duration::from_millis((time_s * 1000.0) as u64)).await;
                defmt::info!("Step {} time limit reached", i);
            } else {
                core::future::pending::<()>().await;
            }
        };
        let vol_fut = async {
            if volume > 0.0 {
                loop {
                    // volume is cumulative across the whole profile (volume_ml
                    // is reset to 0 at profile start by transition_state).
                    if crate::flow_meter::get_flow().volume_ml >= volume {
                        defmt::info!("Step {} volume limit reached", i);
                        break;
                    }
                    Timer::after(Duration::from_millis(50)).await;
                }
            } else {
                core::future::pending::<()>().await;
            }
        };
        let res = select(
            select(time_fut, vol_fut),
            Timer::after(Duration::from_secs(120)),
        )
        .await;

        if let Either::Second(_) = res {
            defmt::warn!("Step {} hit safety timeout (120s)!", i);
        }
    }
    defmt::info!("Profile '{}' completed\r\n", profile.name.as_str());
    // `pump` drops here (or at the cancellation point if aborted), sending
    // PumpMode::Idle to reset pressure target, flow limit, and direct pump.
}

pub async fn execute_steam() {
    let s = Settings::get().await;
    set_target_temp(TargetTempMode::Steam).await;
    Timer::after(Duration::from_secs(s.machine.steam_time_limit_s as u64)).await;
    set_target_temp(TargetTempMode::Brew).await;
}

pub async fn execute_cooldown_flush() {
    let s = Settings::get().await;
    // Drop the target to 0 (heater off) instead of brew temp — the heater
    // fighting the incoming cold water only slows the cooldown down.
    set_target_temp(TargetTempMode::Off).await;
    let _pump = PumpGuard::engage(PumpMode::DirectPump(PUMP_POWER));

    loop {
        let t_c = crate::state::get_telemetry().temp_c;
        if t_c <= s.machine.brew_temp + s.machine.temp_offset {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

pub async fn execute_descale() {
    const DESCALE_VOLUME_ML: f32 = 200.0;
    const DESCALE_SOAK_SECS: u64 = 2 * 60;
    const FLOW_START_GRACE_SECS: u64 = 1;
    const FLOW_STALL_THRESHOLD_ML_S: f32 = 0.5;

    set_target_temp(TargetTempMode::Descale).await;

    loop {
        // --- Pump 200 ml ---
        crate::flow_meter::reset_volume();
        let tank_empty = {
            let _pump = PumpGuard::engage(PumpMode::DirectPump(PUMP_POWER));

            // Allow time for flow to establish before checking for an empty tank.
            Timer::after(Duration::from_secs(FLOW_START_GRACE_SECS)).await;

            loop {
                Timer::after(Duration::from_millis(200)).await;
                let state = crate::flow_meter::get_flow();

                if state.flow_rate_ml_s < FLOW_STALL_THRESHOLD_ML_S {
                    // Flow stopped while pump is running — tank is empty.
                    defmt::info!("Descale: no flow detected, tank empty.");
                    break true;
                }
                if state.volume_ml >= DESCALE_VOLUME_ML {
                    defmt::info!("Descale: {}ml dispensed.", DESCALE_VOLUME_ML);
                    break false;
                }
            }
            // `_pump` drops here, returning the pump to idle before the soak.
        };

        if tank_empty {
            break;
        }

        // --- Soak ---
        defmt::info!("Descale: soaking for {} s...", DESCALE_SOAK_SECS);
        Timer::after(Duration::from_secs(DESCALE_SOAK_SECS)).await;
    }

    set_target_temp(TargetTempMode::Brew).await;
}

pub async fn execute_direct_pump(power: f32) {
    let _pump = PumpGuard::engage(PumpMode::DirectPump(power));
    core::future::pending::<()>().await;
}

// ==========================================
// HARDWARE EXECUTOR TASK
// ==========================================

/// Whether a hardware operation needs the solenoid valve open (pressurizing
/// the group head) while it runs. Operations that don't need it just leave
/// it alone — it's already closed by the invariant that `SolenoidGuard`
/// always closes it again on drop, and it starts closed at boot.
#[derive(Clone, Copy)]
enum Solenoid {
    Open,
    Closed,
}

/// RAII guard for the solenoid valve: opens it when created and
/// unconditionally closes it again when dropped. This is the Rust
/// equivalent of a context manager — every exit path (natural finish,
/// abort, or a future early return/panic) closes the valve without relying
/// on a manual `set_low()` at the end of the function. Only constructed for
/// operations that actually open the valve (see `Solenoid`).
struct SolenoidGuard<'a> {
    valve: &'a mut Output<'static>,
}

impl<'a> SolenoidGuard<'a> {
    fn open(valve: &'a mut Output<'static>) -> Self {
        valve.set_high();
        Self { valve }
    }
}

impl Drop for SolenoidGuard<'_> {
    fn drop(&mut self) {
        self.valve.set_low();
    }
}

/// Owns the single solenoid valve GPIO and drives cancellable hardware
/// operations through it. Since there is exactly one valve in the system,
/// bundling it here means call sites just say `executor.run_cancellable(...)`
/// instead of threading `&mut valve` through every call.
struct HardwareExecutor {
    valve: Output<'static>,
}

impl HardwareExecutor {
    fn new(valve: Output<'static>) -> Self {
        Self { valve }
    }

    async fn run_cancellable<F: core::future::Future>(
        &mut self,
        solenoid: Solenoid,
        action_name: &'static str,
        fut: F,
    ) {
        // Closed operations never touch the valve — it's already closed.
        let _solenoid =
            matches!(solenoid, Solenoid::Open).then(|| SolenoidGuard::open(&mut self.valve));
        let abort = core::pin::pin!(SIG_PROFILE_ABORT.wait());
        let run = core::pin::pin!(fut);
        match select(run, abort).await {
            Either::First(_) => {
                defmt::info!("Hardware: {} finished naturally", action_name);
                // A Stop (or a new transition) racing the last instant of this
                // operation may have signaled abort just as `run` won the race
                // above — discard it now so it can't spuriously cancel the
                // *next* operation (see coordinator::transition_state/stop_to_idle,
                // which only signal abort while a busy operation is in flight).
                SIG_PROFILE_ABORT.reset();
                SIG_COMMAND.signal(MachineCommand::ProfileFinished);
            }
            Either::Second(_) => {
                defmt::warn!("Hardware: {} aborted", action_name);
            }
        }
        // `_solenoid` drops here (if it was Open), closing the valve regardless
        // of which branch ran.
    }
}

#[embassy_executor::task]
pub async fn hardware_task(valve: Output<'static>) {
    let mut executor = HardwareExecutor::new(valve);
    loop {
        let cmd = SIG_HARDWARE_CMD.wait().await;
        defmt::info!("Hardware task received command");

        match cmd {
            HardwareCommand::RunProfile(p) => {
                defmt::info!("Hardware: Starting profile '{}'", p.name.as_str());
                executor
                    .run_cancellable(Solenoid::Open, "Profile", execute_profile(p))
                    .await;
            }
            HardwareCommand::Steam => {
                defmt::info!("Hardware: Starting steam");
                executor
                    .run_cancellable(Solenoid::Closed, "Steam", execute_steam())
                    .await;
            }
            HardwareCommand::Descale => {
                defmt::info!("Hardware: Starting descale");
                executor
                    .run_cancellable(Solenoid::Closed, "Descale", execute_descale())
                    .await;
            }
            HardwareCommand::CooldownFlush => {
                defmt::info!("Hardware: Starting cooldown flush");
                executor
                    .run_cancellable(Solenoid::Closed, "Cooldown flush", execute_cooldown_flush())
                    .await;
            }
            HardwareCommand::DirectPump(power) => {
                defmt::info!("Hardware: Starting direct pump {}%", power);
                executor
                    .run_cancellable(Solenoid::Open, "Direct pump", execute_direct_pump(power))
                    .await;
            }
            HardwareCommand::HotWater => {
                defmt::info!("Hardware: Starting hot water");
                executor
                    .run_cancellable(
                        Solenoid::Closed,
                        "Hot water",
                        execute_direct_pump(PUMP_POWER),
                    )
                    .await;
            }
        }
    }
}
