//! PIO0 setup functions (preserved verbatim) and 50 Hz AC-sync control loop task.

use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{Common, Config, Direction, Pin, StateMachine};
use embassy_time::{Duration, Instant};
use fixed::FixedU32;
use pio::pio_asm;

use crate::bus;
use crate::controller::{ControlEngine, ControlInput};

pub fn setup_trigger_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 1>,
    zc_pin: &Pin<'static, PIO0>,
) {
    let prg = pio_asm!(
        ".wrap_target",
        "wait 1 pin 0", // Wait for pin to go high
        "wait 0 pin 0", // Wait for pin to go low (detect falling edge of zero-cross)
        "mov x, !null", // Initialize X counter to 0xFFFFFFFF
        "low_loop:",
        "jmp pin, rising_edge", // If pin goes high, we found the next edge
        "jmp x--, low_loop",    // Decrement X and loop
        "rising_edge:",
        "mov isr, !x",  // ISR = NOT(X) = elapsed cycles
        "push noblock", // Push the period measurement to the RX FIFO
        ".wrap"
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_in_pins(&[zc_pin]);
    cfg.set_jmp_pin(zc_pin);
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 2_000_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::In, &[zc_pin]);
    sm.set_enable(true);
}

pub fn setup_triac_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 2>,
    triac_pin: &Pin<'static, PIO0>,
    zc_pin: &Pin<'static, PIO0>,
) {
    let prg = pio_asm!(
        ".wrap_target",
        "pull block",   // Pull phase delay from TX FIFO (block if empty)
        "mov x, osr",   // Move delay value to X counter
        "wait 1 pin 0", // Wait for Zero-Cross signal high
        "wait 0 pin 0", // Wait for Zero-Cross signal low (start of half-wave)
        "lp:",
        "jmp x-- lp",       // Wait for 'X' microseconds
        "set pins, 1 [30]", // Trigger Triac (pulse high for ~30 cycles)
        "set pins, 0",      // Set Triac gate low
        ".wrap"
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_set_pins(&[triac_pin]);
    cfg.set_out_pins(&[triac_pin]);
    cfg.set_in_pins(&[zc_pin]);
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 1_000_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[triac_pin]);
    sm.set_enable(true);
}

/// Heater PIO SM: zero-cross-synced ON/OFF with a hardware fail-safe.
pub fn setup_heater_sm(
    common: &mut Common<'static, PIO0>,
    sm: &mut StateMachine<'static, PIO0, 0>,
    heater_pin: &Pin<'static, PIO0>,
    zc_pin: &Pin<'static, PIO0>,
) {
    let prg = pio_asm!(
        ".wrap_target",
        "wait 0 pin 0", // Sync: sensed start of positive half-wave (chunk boundary)
        "nop [31]",     // Fixed settle delay: 32 cycles @ 16kHz = exactly 2ms, clears
        // the true-zero-cross race window regardless of transformer lag
        "set x, 0",     // Sentinel: 0 = "nothing queued this chunk"
        "pull noblock", // FIFO has data -> OSR=data; empty -> OSR=x (0)
        "mov x, osr",
        "jmp !x, off",
        "set pins, 1", // Flag present -> heater ON for the whole chunk
        "jmp cont",
        "off:",
        "set pins, 0", // FIFO empty -> heater OFF for the whole chunk (fail-safe)
        "cont:",
        "wait 1 pin 0", // Ride out the rest of the positive half-wave
        ".wrap"         // Loop back to "wait 0 pin 0": rides out the negative half-wave too
    );
    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_set_pins(&[heater_pin]);
    cfg.set_in_pins(&[zc_pin]);
    // 16kHz SM clock: only governs the settle-delay loop's granularity and the
    // "wait pin" instructions' edge-detection latency (both ms-scale-tolerant
    // here) — independent of the trigger/triac SMs' own clock dividers.
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 16_000.0);
    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[heater_pin]);
    sm.set_enable(true);
}

#[embassy_executor::task]
pub async fn ac_sync_control_task(
    mut sm_trigger: StateMachine<'static, PIO0, 1>,
    mut sm_pump: StateMachine<'static, PIO0, 2>,
    mut sm_heater: StateMachine<'static, PIO0, 0>,
) {
    let mut snap = bus::get_settings_snapshot();
    let mut engine = ControlEngine::new(snap.control, snap.settings_rev);
    let mut last_tick = Instant::now();

    loop {
        let zc_res =
            embassy_time::with_timeout(Duration::from_millis(25), sm_trigger.rx().wait_pull())
                .await;

        let ac_period_us = if let Ok(mut period_us_raw) = zc_res {
            while let Some(latest) = sm_trigger.rx().try_pull() {
                period_us_raw = latest;
            }
            Some(period_us_raw)
        } else {
            None
        };

        let now = Instant::now();
        let dt_s = now.duration_since(last_tick).as_micros() as f32 / 1_000_000.0;
        last_tick = now;

        let sensors = bus::get_sensors();
        if bus::get_settings_rev() != snap.settings_rev {
            snap = bus::get_settings_snapshot();
        }
        let intent = bus::get_control_intent();

        let out = engine.tick(&ControlInput {
            ac_period_us,
            dt_s,
            pressure_bar: sensors.pressure_bar,
            temp_c: sensors.temp_c,
            flow_ml_s: bus::flow_rate_ml_s(),
            volume_ml: bus::shot_volume_ml(),
            intent,
            settings: snap.control,
            settings_rev: snap.settings_rev,
        });

        if let Some(delay_us) = out.triac_delay_us {
            sm_pump.tx().push(delay_us);
        }
        if out.fire_heater {
            sm_heater.tx().push(1);
        }

        bus::publish_telemetry(out.telemetry);
    }
}
