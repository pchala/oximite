//! Core 0 coordinator task owning `Machine`, the solenoid valve, and bus publication.

use embassy_futures::select::{select, Either};
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Instant, Ticker};
use oximite_proto::model::{BrewProfile, Settings, MAX_PROFILES};

use crate::bus;
use crate::machine::{Machine, MachineStepEffects};

struct SolenoidGuard<'a> {
    valve: &'a mut Output<'static>,
    is_open: bool,
}

impl<'a> SolenoidGuard<'a> {
    fn new(valve: &'a mut Output<'static>) -> Self {
        valve.set_low();
        Self {
            valve,
            is_open: false,
        }
    }

    fn set(&mut self, open: bool) {
        if open && !self.is_open {
            self.valve.set_high();
            self.is_open = true;
        } else if !open && self.is_open {
            self.valve.set_low();
            self.is_open = false;
        }
    }
}

impl Drop for SolenoidGuard<'_> {
    fn drop(&mut self) {
        self.valve.set_low();
    }
}

fn apply_effects(machine: &Machine, effects: MachineStepEffects, solenoid: &mut SolenoidGuard<'_>) {
    if effects.reset_volume {
        bus::reset_volume();
    }

    // Publish coherent intent BEFORE updating solenoid state so the pump
    // returns to Idle before the valve closes when stopping an operation.
    bus::publish_control_intent(machine.intent());
    bus::set_state(machine.state());
    bus::set_session_brew_temp(machine.session_brew_temp());
    solenoid.set(machine.valve_open());

    if effects.settings_changed {
        bus::publish_settings(
            machine.control_settings(),
            machine.public_settings(),
            &machine.settings().wifi,
            machine.settings_rev(),
            machine.wifi_rev(),
        );
    }
    if effects.profiles_changed {
        bus::publish_profiles(machine.profiles(), machine.profiles_rev());
    }
    if effects.dirty_flash != 0 {
        bus::mark_flash_dirty(effects.dirty_flash);
    }
    for ev in effects.events {
        bus::push_event(ev);
    }
}

#[embassy_executor::task]
pub async fn coordinator_task(
    mut valve: Output<'static>,
    initial_settings: Settings,
    initial_profiles: [Option<BrewProfile>; MAX_PROFILES as usize],
) {
    let mut solenoid = SolenoidGuard::new(&mut valve);
    let mut machine = Machine::new(
        initial_settings,
        initial_profiles,
        Instant::now().as_millis(),
    );

    bus::publish_control_intent(machine.intent());
    bus::set_state(machine.state());
    bus::set_session_brew_temp(machine.session_brew_temp());
    bus::publish_settings(
        machine.control_settings(),
        machine.public_settings(),
        &machine.settings().wifi,
        machine.settings_rev(),
        machine.wifi_rev(),
    );
    bus::publish_profiles(machine.profiles(), machine.profiles_rev());

    let mut ticker = Ticker::every(Duration::from_millis(20));

    loop {
        match select(bus::next_command(), ticker.next()).await {
            Either::First(q) => {
                defmt::info!("Coordinator received command: {}", q.cmd.name());
                let now_ms = Instant::now().as_millis();
                let effects = machine.handle_command(q.cmd, now_ms);
                apply_effects(&machine, effects, &mut solenoid);
                bus::mark_served();
                if q.reply_diag {
                    if let Some(req_id) = q.req_id {
                        bus::push_diag_reply(req_id, q.ticket);
                    }
                }
            }
            Either::Second(()) => {
                let now_ms = Instant::now().as_millis();
                let effects =
                    machine.tick(now_ms, bus::shot_volume_ml(), bus::get_sensors().temp_c);
                apply_effects(&machine, effects, &mut solenoid);
            }
        }
    }
}
