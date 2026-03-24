use crate::state::{get_state, MachineCommand, MachineState, SIG_COMMAND};
use embassy_rp::gpio::Input;
use embassy_time::{Duration, Timer};

#[embassy_executor::task]
pub async fn run_button_task(
    btn_brew: Input<'static>,
    btn_steam: Input<'static>,
    btn_flush: Input<'static>,
) {
    loop {
        if btn_brew.is_low() {
            let current = get_state();
            if current == MachineState::Idle || current == MachineState::Sleeping {
                SIG_COMMAND.signal(MachineCommand::Brew);
            } else {
                SIG_COMMAND.signal(MachineCommand::Stop);
            }
            Timer::after(Duration::from_millis(500)).await; // Debounce + lockout
        }

        if btn_steam.is_low() {
            SIG_COMMAND.signal(MachineCommand::Steam);
            Timer::after(Duration::from_millis(500)).await;
        }

        if btn_flush.is_low() {
            SIG_COMMAND.signal(MachineCommand::Flush);
            Timer::after(Duration::from_millis(500)).await;
        }

        Timer::after(Duration::from_millis(50)).await;
    }
}
