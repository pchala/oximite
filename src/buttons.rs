use crate::state::{MachineCommand, SIG_COMMAND};
use embassy_rp::gpio::Input;
use embassy_time::{Duration, Timer};

#[derive(PartialEq, Clone, Copy, defmt::Format)]
pub enum ButtonId {
    Power,
    Brew,
    Steam,
    Flush,
}

#[derive(PartialEq, defmt::Format)]
pub enum ButtonEvent {
    Pressed(ButtonId),
    Released(ButtonId),
}

#[derive(PartialEq)]
enum InternalState {
    Pressed,
    Released,
}

struct Debouncer<'a> {
    input: Input<'a>,
    id: ButtonId,
    state: InternalState,
    integrator: u8,
    max: u8,
}

impl<'a> Debouncer<'a> {
    fn new(input: Input<'a>, id: ButtonId, max: u8) -> Self {
        let is_pressed = input.is_low();
        Self {
            input,
            id,
            state: if is_pressed {
                InternalState::Pressed
            } else {
                InternalState::Released
            },
            integrator: if is_pressed { max } else { 0 },
            max,
        }
    }

    /// Returns the event if the button state changed and has been stable for `max` samples.
    fn poll(&mut self) -> Option<ButtonEvent> {
        let is_pressed = self.input.is_low();

        if is_pressed {
            if self.integrator < self.max {
                self.integrator += 1;
            }
        } else {
            if self.integrator > 0 {
                self.integrator -= 1;
            }
        }

        if self.state == InternalState::Released && self.integrator == self.max {
            self.state = InternalState::Pressed;
            Some(ButtonEvent::Pressed(self.id))
        } else if self.state == InternalState::Pressed && self.integrator == 0 {
            self.state = InternalState::Released;
            Some(ButtonEvent::Released(self.id))
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
    let mut db_power = Debouncer::new(btn_power, ButtonId::Power, 5);
    let mut db_brew = Debouncer::new(btn_brew, ButtonId::Brew, 5);
    let mut db_steam = Debouncer::new(btn_steam, ButtonId::Steam, 5);
    let mut db_flush = Debouncer::new(btn_flush, ButtonId::Flush, 5);

    loop {
        if let Some(evt) = db_power.poll() {
            handle_button_event(evt);
        }
        if let Some(evt) = db_brew.poll() {
            handle_button_event(evt);
        }
        if let Some(evt) = db_steam.poll() {
            handle_button_event(evt);
        }
        if let Some(evt) = db_flush.poll() {
            handle_button_event(evt);
        }

        // 10ms poll interval for the integrator
        Timer::after(Duration::from_millis(10)).await;
    }
}

fn handle_button_event(event: ButtonEvent) {
    defmt::info!("Button event: {:?}", event);

    // We only act on Pressed events to avoid ghost presses on slow release
    let ButtonEvent::Pressed(id) = event else {
        return;
    };

    let cmd = match id {
        ButtonId::Power => MachineCommand::TogglePower,
        ButtonId::Brew => MachineCommand::Brew,
        ButtonId::Steam => MachineCommand::Steam,
        ButtonId::Flush => MachineCommand::Flush,
    };

    SIG_COMMAND.signal(cmd);
}
