//! Polls the ADC button ladder published by `adc_task` and drives `buttons::LadderDebouncer`.

use embassy_time::{Duration, Timer};

use crate::bus;
use crate::buttons::LadderDebouncer;

#[embassy_executor::task]
pub async fn run_button_task() {
    let mut debouncer = LadderDebouncer::new();
    let mut fault = false;

    loop {
        let raw = bus::get_button_raw();
        // No ADC sample yet.
        if raw != u16::MAX {
            if let Some(button) = debouncer.poll(raw) {
                let cmd = button.command();
                defmt::info!("Button pressed: {}", cmd.name());
                let _ = bus::send_command(cmd);
            }
            let now_fault = debouncer.stable().is_fault();
            if now_fault != fault {
                fault = now_fault;
                if fault {
                    defmt::warn!("Button panel fault: open or short, buttons ignored");
                } else {
                    defmt::info!("Button panel fault cleared");
                }
            }
        }
        Timer::after(Duration::from_millis(10)).await;
    }
}
