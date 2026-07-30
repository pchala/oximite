//! Analog sensor acquisition: oversamples the pressure and boiler-temperature
//! channels, filters them, converts to physical units, and publishes the
//! result into `state::TELEMETRY_WATCH` for every other task to read.

use embassy_rp::adc::{Adc, Async, Channel};
use embassy_time::Duration;

use crate::calibration::{get_pressure_from_adc, get_temp_from_adc};
use crate::state::TELEMETRY_WATCH;

// Samples `total` conversions from `ch`, discarding the leading `total - keep`
// (letting the sample-and-hold cap settle) and returning the average of the rest.
async fn sample_avg(
    adc: &mut Adc<'static, Async>,
    ch: &mut Channel<'static>,
    total: usize,
    keep: usize,
) -> f32 {
    let discard = total - keep;
    let mut sum: u32 = 0;
    for i in 0..total {
        let v = adc.read(ch).await.unwrap_or(0) as u32;
        if i >= discard {
            sum += v;
        }
    }
    sum as f32 / keep as f32
}

#[embassy_executor::task]
pub async fn adc_task(
    mut adc: Adc<'static, Async>,
    mut ch_p: Channel<'static>,
    mut ch_t: Channel<'static>,
) {
    let (mut p_ema, mut t_ema) = (0.0f32, 0.0f32);
    let mut initialized = false;

    let mut ticker = embassy_time::Ticker::every(Duration::from_hz(500));

    loop {
        // Sample each channel `total` times; discard the first `total - keep` to allow the ADC
        // sample-and-hold capacitor to fully charge through the 1k series resistor on the analog
        // lines, then average the last `keep` samples to knock down noise before the EMA filter.
        let raw_p = sample_avg(&mut adc, &mut ch_p, 10, 5).await;
        let raw_t = sample_avg(&mut adc, &mut ch_t, 10, 5).await;

        if !initialized {
            p_ema = raw_p;
            t_ema = raw_t;
            initialized = true;
        } else {
            const ALPHA_P: f32 = 0.01; // ~0.8 Hz cutoff (rejects ~200ms beat from unsynced ADC/pump sampling)
            const ALPHA_T: f32 = 0.2; // ~20.0 Hz Cutoff
            p_ema = p_ema + ALPHA_P * (raw_p - p_ema);
            t_ema = t_ema + ALPHA_T * (raw_t - t_ema);
        }

        // Convert the filtered counts to physical units
        let p_bar = get_pressure_from_adc(p_ema);
        let t_c = get_temp_from_adc(t_ema);

        // Fetch current state, update it, and broadcast
        let mut state = TELEMETRY_WATCH.try_get().unwrap_or_default();
        state.pressure_bar = p_bar;
        state.temp_c = t_c;
        TELEMETRY_WATCH.sender().send(state);

        ticker.next().await;
    }
}
