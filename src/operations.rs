//! Machine operations (brew profile, steam, cooldown flush, hot water, raw
//! pump) and the valve-owning dispatcher that runs them.
//!
//! Each operation is a plain future built from the control-loop primitives in
//! `control.rs`. `coordinator_task` awaits [`execute`] directly, racing it
//! against the command queue, so cancellation is dropping the future and
//! completion is that future returning. There is no abort signal and no
//! "operation finished" message — neither can therefore be applied to the
//! wrong operation.
//!
//! Operations never set the heater target: that is a pure function of
//! `MachineState`, applied by the coordinator when it enters the state. An
//! operation therefore cannot leave a setpoint behind when it is cancelled.

use embassy_futures::select::{select, Either};
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Timer};

use crate::control::{BrewActiveGuard, PumpGuard, PumpMode};
use crate::profiles::BrewProfile;
use crate::settings::Settings;

/// Pump power used for flush and cooldown operations (%).
pub const PUMP_POWER: f32 = 80.0;

/// Safety ceiling for the pump-only operations (flush, raw direct pump,
/// cooldown flush). These have no target to stop them — they run until the
/// user says stop — and the buttons are edge-triggered, so a single press
/// would otherwise pump until the tank ran dry. Auto-sleep cannot rescue them
/// either: it only fires while Idle. Set well above any plausible manual use
/// so it never interferes with normal operation.
const MAX_PUMP_RUN_S: u64 = 60;

/// Hot water gets a longer ceiling than a flush — filling a mug at a few ml/s
/// is legitimately slower than rinsing the group head.
const MAX_HOT_WATER_S: u64 = 120;

/// Per-step ceiling, racing whatever limits the step set for itself. Catches a
/// step whose declared time exceeds this, and is the *only* bound on a
/// volume-only step whose target is never reached — which is why such a step
/// needs no default duration of its own.
const STEP_SAFETY_S: u64 = 120;

async fn execute_profile(profile: BrewProfile) {
    defmt::info!("Executing profile: {}", profile.name.as_str());
    let mut pump = PumpGuard::engage(PumpMode::Idle);
    let _brew_active = BrewActiveGuard::engage();

    for (i, step) in profile.steps.iter().enumerate() {
        let time_s = step.time_s.unwrap_or(0.0);
        let volume = step.volume.unwrap_or(0.0);
        let pressure = step.pressure.unwrap_or(0.0);
        let flow = step.flow.unwrap_or(0.0);

        // Neither a time nor a volume limit means nothing would ever end the
        // step but the safety ceiling.
        if time_s <= 0.0 && volume <= 0.0 {
            continue;
        }

        defmt::info!(
            "Step {}: P={}bar, F={}ml/s, T={}s, V={}ml",
            i,
            pressure,
            flow,
            time_s,
            volume
        );

        if pressure >= 10.0 {
            pump.set_mode(PumpMode::DirectPump(pressure));
        } else {
            pump.set_mode(PumpMode::Pressure {
                bar: pressure,
                flow_limit_ml_s: flow,
            });
        }

        let by_time = async {
            if time_s <= 0.0 {
                core::future::pending::<()>().await;
            }
            Timer::after(Duration::from_millis((time_s * 1000.0) as u64)).await;
        };
        let by_volume = async {
            if volume <= 0.0 {
                core::future::pending::<()>().await;
            }
            // Cumulative across the whole profile, not per step —
            // `coordinator::start()` zeroes it once at profile start.
            while crate::flow_meter::shot_volume_ml() < volume {
                Timer::after(Duration::from_millis(50)).await;
            }
        };

        match select(
            select(by_time, by_volume),
            Timer::after(Duration::from_secs(STEP_SAFETY_S)),
        )
        .await
        {
            Either::First(Either::First(())) => defmt::info!("Step {} time limit reached", i),
            Either::First(Either::Second(())) => defmt::info!("Step {} volume limit reached", i),
            Either::Second(()) => {
                defmt::warn!("Step {} hit safety timeout ({}s)!", i, STEP_SAFETY_S)
            }
        }
    }
    defmt::info!("Profile '{}' completed\r\n", profile.name.as_str());
    // `pump` drops here (or at the cancellation point if aborted), sending
    // PumpMode::Idle to reset pressure target, flow limit, and direct pump.
}

/// Holds the boiler at steam temperature for the configured limit. The target
/// itself comes from entering `MachineState::Steaming`; this only bounds how
/// long the machine stays there.
async fn execute_steam() {
    let s = Settings::get().await;
    Timer::after(Duration::from_secs(s.machine.steam_time_limit_s as u64)).await;
}

async fn execute_cooldown_flush() {
    let s = Settings::get().await;
    // The heater is already off — `MachineState::Cooling` maps to a 0 °C
    // target — because the heater fighting the incoming cold water only slows
    // the cooldown down.
    let _pump = PumpGuard::engage(PumpMode::DirectPump(PUMP_POWER));

    let cooled = async {
        let stop_at = s.machine.brew_temp + s.machine.temp_offset * 2.0;
        defmt::info!(
            "Cooldown flush: boiler {} C, stopping at {} C",
            crate::state::get_telemetry().temp_c,
            stop_at
        );
        loop {
            let t_c = crate::state::get_telemetry().temp_c;
            if t_c <= stop_at {
                break;
            }
            Timer::after(Duration::from_millis(100)).await;
        }
    };

    // Bounded like the other pump-only operations — this one stops on a sensor
    // reading, so a stuck-high temperature would otherwise pump indefinitely.
    let timeout = Timer::after(Duration::from_secs(MAX_PUMP_RUN_S));
    if let Either::Second(_) = select(cooled, timeout).await {
        defmt::warn!("Cooldown flush hit its {}s safety limit", MAX_PUMP_RUN_S);
    }
}

/// Runs the pump at a fixed power until cancelled, or until `max_run_s` — see
/// [`MAX_PUMP_RUN_S`]. Returning normally means the coordinator drops back to
/// Idle and the valve closes.
async fn execute_direct_pump(power: f32, max_run_s: u64) {
    let _pump = PumpGuard::engage(PumpMode::DirectPump(power));
    Timer::after(Duration::from_secs(max_run_s)).await;
    defmt::warn!("Direct pump hit its {}s safety limit", max_run_s);
}

// ==========================================
// OPERATION DISPATCH
// ==========================================

/// RAII guard for the solenoid valve: opens it when created and
/// unconditionally closes it again when dropped. This is the Rust equivalent
/// of a context manager — every exit path (natural finish, cancellation, or an
/// early return) closes the valve without relying on a manual `set_low()` at
/// the end of the function. Only constructed for operations that actually open
/// the valve (see [`Operation::opens_valve`]).
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

/// A hardware sequence the coordinator has decided to run.
///
/// This is a plain value handed to [`execute`], not a message posted to
/// another task. That is deliberate: the coordinator awaits the operation
/// directly, so "it finished" is a future returning rather than a
/// notification that could be delivered after the coordinator has already
/// moved on to a different operation.
///
/// `Profile` makes this enum ~360 bytes, but it is only ever a local in the
/// coordinator's future — never queued, never copied per-message — so the
/// size difference costs one slot of task stack rather than N slots of queue.
#[allow(clippy::large_enum_variant)]
pub enum Operation {
    Profile(BrewProfile),
    Steam,
    CooldownFlush,
    DirectPump(f32),
    HotWater,
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Operation::Profile(_) => "Profile",
            Operation::Steam => "Steam",
            Operation::CooldownFlush => "Cooldown flush",
            Operation::DirectPump(_) => "Direct pump",
            Operation::HotWater => "Hot water",
        }
    }

    /// Whether this operation needs the solenoid valve open (pressurizing the
    /// group head) while it runs. The rest leave it alone — it's already
    /// closed by the invariant that `SolenoidGuard` always closes it again on
    /// drop, and it starts closed at boot.
    fn opens_valve(&self) -> bool {
        match self {
            Operation::Profile(_) | Operation::DirectPump(_) => true,
            Operation::Steam | Operation::CooldownFlush | Operation::HotWater => false,
        }
    }
}

/// Runs one operation to completion, holding the solenoid valve open for the
/// ones that need it.
///
/// Every variant collapses into this single future type, which is what lets
/// `coordinator_task` race any operation against the command queue with one
/// `select`.
///
/// Cancellation is simply dropping this future. Everything an operation
/// touches is held by an RAII guard — `PumpGuard`, `BrewActiveGuard` and
/// `SolenoidGuard` — so an abandoned operation unwinds to a safe state on its
/// own. Drop order matters and is correct by construction: `_solenoid` is
/// declared before the operation is awaited, so it is dropped *last*, and the
/// pump is always returned to idle before the valve closes.
pub async fn execute(op: Operation, valve: &mut Output<'static>) {
    // Operations that don't open the valve never touch it — it's already closed.
    let _solenoid = op.opens_valve().then(|| SolenoidGuard::open(valve));
    match op {
        Operation::Profile(p) => execute_profile(p).await,
        Operation::Steam => execute_steam().await,
        Operation::CooldownFlush => execute_cooldown_flush().await,
        Operation::DirectPump(power) => execute_direct_pump(power, MAX_PUMP_RUN_S).await,
        Operation::HotWater => execute_direct_pump(PUMP_POWER, MAX_HOT_WATER_S).await,
    }
}
