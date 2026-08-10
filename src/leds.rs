use embassy_futures::select::select;
use embassy_rp::pio::{
    Common, Config, Direction, FifoJoin, Instance, Pin, ShiftDirection, StateMachine,
};
use embassy_time::{Duration, Instant, Timer};
use fixed::FixedU32;
use pio::pio_asm;

use crate::state::{self, MachineState, MACHINE_STATE};

#[derive(Clone, Copy)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
    pub const fn off() -> Self {
        Self::new(0, 0, 0)
    }
}

pub fn setup_ws2812_sm<P: Instance, const SM: usize>(
    common: &mut Common<'static, P>,
    sm: &mut StateMachine<'static, P, SM>,
    pin: Pin<'static, P>,
) {
    let prg = pio_asm!(
        ".side_set 1",
        ".wrap_target",
        "get_data:",
        "pull block      side 0", // STALL: Forces line LOW when FIFO is empty!
        "set y, 23       side 0", // Loop 24 times for 1 LED
        "bitloop:",
        "out x, 1        side 0 [2]",
        "jmp !x do_zero  side 1 [1]",
        "do_one:",
        "jmp y-- bitloop side 1 [4]", // Long High
        "jmp get_data    side 0",     // Done 24 bits. Force line LOW.
        "do_zero:",
        "jmp y-- bitloop side 0 [4]", // Long Low
        ".wrap"
    );

    let loaded = common.load_program(&prg.program);
    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[&pin]);
    cfg.set_out_pins(&[&pin]);
    cfg.clock_divider = FixedU32::from_num(crate::board::SYS_CLK_HZ / 8_000_000.0); // 18.75 → 8 MHz PIO clock for WS2812 timing
    cfg.shift_out.direction = ShiftDirection::Left;
    cfg.shift_out.auto_fill = false;
    cfg.fifo_join = FifoJoin::TxOnly;

    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[&pin]);
    sm.set_enable(true);
}

type LedSm = StateMachine<'static, embassy_rp::peripherals::PIO2, 1>;

/// Clocks one frame out to the strip. Both words go into the FIFO back-to-back
/// with no `await` between them, so the PIO never stalls mid-frame — a stall
/// forces the line low and would latch the pixels early.
fn write_frame(sm: &mut LedSm, leds: &[Rgb; 2]) {
    const BRIGHTNESS: u32 = 30; // ~20% (50/255)

    for led in leds {
        // Apply brightness scaling
        let r = (led.r as u32 * BRIGHTNESS) >> 8;
        let g = (led.g as u32 * BRIGHTNESS) >> 8;
        let b = (led.b as u32 * BRIGHTNESS) >> 8;

        // WS2812 expects GRB format (MSB first)
        // We push a 32-bit word, the PIO will pull it and use the top 24 bits
        sm.tx().push((g << 24) | (r << 16) | (b << 8));
    }
}

// ==========================================
// LED COLOR HELPERS
// ==========================================

/// Pump duty the brew LED treats as "on target", and the ±span of the
/// crossfade around it: solid blue at or below 60%, green at 80%, solid red at
/// 100%.
const PUMP_TARGET_DUTY: f32 = 80.0;
const PUMP_DUTY_WINDOW: f32 = 20.0;

/// Blue (cold/low) → Green (on target) → Red (hot/high), with a ±`window` crossfade zone.
fn temp_color(current: f32, target: f32, window: f32) -> Rgb {
    let (r, g, b) = if current <= target {
        let lower_bound = target - window;
        if current <= lower_bound {
            (0.0_f32, 0.0_f32, 255.0_f32)
        } else {
            let g = 255.0 * (current - lower_bound) / window;
            (0.0, g, 255.0 - g)
        }
    } else {
        let upper_bound = target + window;
        if current >= upper_bound {
            (255.0_f32, 0.0_f32, 0.0_f32)
        } else {
            let r = 255.0 * (current - target) / window;
            (r, 255.0 - r, 0.0)
        }
    };

    Rgb::new(r as u8, g as u8, b as u8)
}

// ==========================================
// LED UI TASK
// ==========================================

/// Renders the machine state onto the two WS2812s and clocks the frame out.
/// Redraws on every state change and at 10 Hz otherwise. WS2812s latch and
/// hold their colour indefinitely, so the periodic redraw is not required to
/// keep them lit — it is insurance against a frame corrupted by the triac
/// switching mains right next to the data line.
#[embassy_executor::task]
pub async fn led_task(mut sm: LedSm) {
    let mut state_rx = MACHINE_STATE.receiver().unwrap();
    let mut last_wakeup = Instant::now();
    let mut was_sleeping = false;

    loop {
        let current_state = state::get_state();

        // Track wakeup time so we can detect the 6-minute warmup window.
        if was_sleeping && current_state != MachineState::Sleeping {
            last_wakeup = Instant::now();
        }
        was_sleeping = current_state == MachineState::Sleeping;

        let a = state::get_telemetry();
        let boiler = temp_color(a.temp_c, a.target_temp, 10.0);

        let (led0, led1) = match current_state {
            MachineState::Sleeping => {
                // Both LEDs magenta in sleep.
                (Rgb::new(255, 0, 255), Rgb::new(255, 0, 255))
            }

            MachineState::Brewing => {
                // LED1: pump-power crossfade vs the 80% target.
                let pump = if a.pump_duty > 0.0 {
                    temp_color(a.pump_duty, PUMP_TARGET_DUTY, PUMP_DUTY_WINDOW)
                } else {
                    Rgb::off()
                };
                (boiler, pump)
            }

            MachineState::Steaming => {
                // LED1: boiler colors.
                (Rgb::off(), boiler)
            }

            // Warmup: only reaches here for inactive states (Idle, Pumping, etc.)
            _ if last_wakeup.elapsed() < Duration::from_secs(6 * 60) => {
                // LED1: red brightness proportional to heater power (0–100% → 0–255).
                let r = (a.heater_duty.clamp(0.0, 100.0) * 2.55) as u8;
                (boiler, Rgb::new(r, 0, 0))
            }

            _ => (boiler, Rgb::off()),
        };

        write_frame(&mut sm, &[led0, led1]);

        // Refresh at ~10 Hz or immediately when the state changes.
        let _ = select(Timer::after(Duration::from_millis(100)), state_rx.changed()).await;
    }
}
