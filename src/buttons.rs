use crate::state::{send_command, MachineCommand};
use embassy_rp::gpio::Input;
use embassy_time::{Duration, Timer};

/// Consecutive agreeing samples before a level change is believed. At the
/// 10 ms poll interval below, 5 samples is a 50 ms debounce.
const DEBOUNCE_SAMPLES: u8 = 5;

struct Debouncer<'a> {
    input: Input<'a>,
    command: MachineCommand,
    pressed: bool,
    integrator: u8,
}

impl<'a> Debouncer<'a> {
    fn new(input: Input<'a>, command: MachineCommand) -> Self {
        let pressed = input.is_low();
        Self {
            input,
            command,
            pressed,
            integrator: if pressed { DEBOUNCE_SAMPLES } else { 0 },
        }
    }

    /// Returns `Some(command)` when the button transitions to the stable pressed state.
    fn poll(&mut self) -> Option<MachineCommand> {
        if self.input.is_low() {
            self.integrator = (self.integrator + 1).min(DEBOUNCE_SAMPLES);
        } else {
            self.integrator = self.integrator.saturating_sub(1);
        }

        match (self.pressed, self.integrator) {
            (false, DEBOUNCE_SAMPLES) => {
                self.pressed = true;
                Some(self.command.clone())
            }
            (true, 0) => {
                self.pressed = false;
                None
            }
            _ => None,
        }
    }
}

#[embassy_executor::task]
pub async fn run_button_task(
    btn_power: Input<'static>,
    btn_brew: Input<'static>,
    btn_steam: Input<'static>,
    btn_flush: Input<'static>,
) {
    let mut debouncers = [
        Debouncer::new(btn_power, MachineCommand::TogglePower),
        Debouncer::new(btn_brew, MachineCommand::Brew),
        Debouncer::new(btn_steam, MachineCommand::Steam),
        Debouncer::new(btn_flush, MachineCommand::Flush),
    ];

    loop {
        for db in debouncers.iter_mut() {
            if let Some(cmd) = db.poll() {
                defmt::info!("Button pressed: {:?}", cmd);
                // A front-panel press has no client waiting on a ticket.
                let _ = send_command(cmd);
            }
        }

        // 10ms poll interval for the integrator
        Timer::after(Duration::from_millis(10)).await;
    }
}
