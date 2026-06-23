use crate::state::{MachineCommand, SIG_COMMAND};
use embassy_rp::gpio::Input;
use embassy_time::{Duration, Timer};

#[derive(PartialEq)]
enum InternalState {
    Pressed,
    Released,
}

struct Debouncer<'a> {
    input: Input<'a>,
    command: MachineCommand,
    state: InternalState,
    integrator: u8,
    max: u8,
}

impl<'a> Debouncer<'a> {
    fn new(input: Input<'a>, command: MachineCommand, max: u8) -> Self {
        let is_pressed = input.is_low();
        Self {
            input,
            command,
            state: if is_pressed { InternalState::Pressed } else { InternalState::Released },
            integrator: if is_pressed { max } else { 0 },
            max,
        }
    }

    /// Returns `Some(command)` when the button transitions to the stable pressed state.
    fn poll(&mut self) -> Option<MachineCommand> {
        let is_pressed = self.input.is_low();

        if is_pressed {
            if self.integrator < self.max {
                self.integrator += 1;
            }
        } else if self.integrator > 0 {
            self.integrator -= 1;
        }

        if self.state == InternalState::Released && self.integrator == self.max {
            self.state = InternalState::Pressed;
            Some(self.command.clone())
        } else if self.state == InternalState::Pressed && self.integrator == 0 {
            self.state = InternalState::Released;
            None
        } else {
            None
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
    // 5 samples at 10ms = 50ms debounce time
    let mut debouncers = [
        Debouncer::new(btn_power, MachineCommand::TogglePower, 5),
        Debouncer::new(btn_brew, MachineCommand::Brew, 5),
        Debouncer::new(btn_steam, MachineCommand::Steam, 5),
        Debouncer::new(btn_flush, MachineCommand::Flush, 5),
    ];

    loop {
        for db in debouncers.iter_mut() {
            if let Some(cmd) = db.poll() {
                defmt::info!("Button pressed: {:?}", cmd);
                SIG_COMMAND.signal(cmd);
            }
        }

        // 10ms poll interval for the integrator
        Timer::after(Duration::from_millis(10)).await;
    }
}
