//! RP2350 ADC acquisition task driving `sensors::SensorFilter`.

use embassy_rp::adc::{Adc, Async, Channel};
use embassy_time::Duration;

use crate::bus;
use crate::sensors::{average_settled, SensorFilter, SAMPLE_TOTAL};

/// A failed read counts as `on_error`. Buttons use full scale (open circuit, a fault) so an error never looks like a press.
async fn sample_raw(
    adc: &mut Adc<'static, Async>,
    ch: &mut Channel<'static>,
    on_error: u16,
) -> f32 {
    let mut raw = [0u16; SAMPLE_TOTAL];
    for slot in &mut raw {
        *slot = adc.read(ch).await.unwrap_or(on_error);
    }
    average_settled(&raw)
}

#[embassy_executor::task]
pub async fn adc_task(
    mut adc: Adc<'static, Async>,
    mut ch_p: Channel<'static>,
    mut ch_t: Channel<'static>,
    mut ch_b: Channel<'static>,
) {
    let mut filter = SensorFilter::new();
    let mut ticker = embassy_time::Ticker::every(Duration::from_hz(500));

    loop {
        let raw_p = sample_raw(&mut adc, &mut ch_p, 0).await;
        let raw_t = sample_raw(&mut adc, &mut ch_t, 0).await;
        let reading = filter.update(raw_p, raw_t);
        bus::publish_sensors(reading);
        let raw_b = sample_raw(&mut adc, &mut ch_b, 4095).await;
        bus::publish_button_raw(raw_b as u16);
        ticker.next().await;
    }
}
