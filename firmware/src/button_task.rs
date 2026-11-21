//! Thin GPIO button polling task driving `debounce::Debouncer`.

use embassy_rp::gpio::Input;
use embassy_time::{Duration, Timer};
use oximite_proto::model::MachineCommand;

use crate::bus;
use crate::debounce::Debouncer;

#[embassy_executor::task]
pub async fn run_button_task(
    btn_power: Input<'static>,
    btn_brew: Input<'static>,
    btn_steam: Input<'static>,
    btn_flush: Input<'static>,
) {
    let mut debouncers = [
        (
            Debouncer::new(btn_power.is_low(), MachineCommand::TogglePower),
            btn_power,
        ),
        (
            Debouncer::new(btn_brew.is_low(), MachineCommand::Brew),
            btn_brew,
        ),
        (
            Debouncer::new(btn_steam.is_low(), MachineCommand::Steam),
            btn_steam,
        ),
        (
            Debouncer::new(btn_flush.is_low(), MachineCommand::Flush),
            btn_flush,
        ),
    ];

    loop {
        for (db, pin) in &mut debouncers {
            if let Some(cmd) = db.poll(pin.is_low()) {
                defmt::info!("Button pressed: {}", cmd.name());
                let _ = bus::send_command(cmd, None, false);
            }
        }
        Timer::after(Duration::from_millis(10)).await;
    }
}
