use embassy_futures::select::select;
use embassy_rp::pio::{
    Common, Config, Direction, FifoJoin, Instance, Pin, ShiftDirection, StateMachine,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use fixed::FixedU32;
use pio::pio_asm;

use crate::control;
use crate::flow_meter;
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

static LED_DATA: Mutex<CriticalSectionRawMutex, [Rgb; 2]> = Mutex::new([Rgb::off(), Rgb::off()]);

pub async fn set_leds(leds: [Rgb; 2]) {
    *LED_DATA.lock().await = leds;
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
    cfg.clock_divider = FixedU32::from_num(150_000_000.0 / 8_000_000.0); // 18.75 → 8 MHz PIO clock for WS2812 timing
    cfg.shift_out.direction = ShiftDirection::Left;
    cfg.shift_out.auto_fill = false;
    cfg.fifo_join = FifoJoin::TxOnly;

    sm.set_config(&cfg);
    sm.set_pin_dirs(Direction::Out, &[&pin]);
    sm.set_enable(true);
}

#[embassy_executor::task]
pub async fn run_led_task(mut sm: StateMachine<'static, embassy_rp::peripherals::PIO0, 3>) {
    const BRIGHTNESS: u32 = 30; // ~20% (50/255)

    loop {
        let leds = *LED_DATA.lock().await;
        for led in leds.iter() {
            // Apply brightness scaling
            let r = (led.r as u32 * BRIGHTNESS) >> 8;
            let g = (led.g as u32 * BRIGHTNESS) >> 8;
            let b = (led.b as u32 * BRIGHTNESS) >> 8;

            // WS2812 expects GRB format (MSB first)
            // We push a 32-bit word, the PIO will pull it and use the top 24 bits
            let word = (g << 24) | (r << 16) | (b << 8);
            sm.tx().push(word);
        }
        embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
    }
}

// ==========================================
// DECOUPLED LED UI TASK
// ==========================================
#[embassy_executor::task]
pub async fn led_update_task() {
    let mut state_rx = MACHINE_STATE.receiver().unwrap();

    loop {
        let current_state = state::get_state();
        let a = control::AdcMonitor::new().get_state().await;
        let f = flow_meter::FlowMonitor::new().get_state().await;

        let temp_color = if a.temp_c < a.target_temp - 1.0 {
            Rgb::new(0, 0, 255) // Blue  — heating
        } else if a.temp_c > a.target_temp + 1.0 {
            Rgb::new(255, 0, 0) // Red   — over-temp
        } else {
            Rgb::new(0, 255, 0) // Green — at target
        };

        let (l1, l2) = if current_state == MachineState::Sleeping {
            // Sleep indicator: magenta on LED1, LED2 off
            (Rgb::new(255, 0, 255), Rgb::off())
        } else if current_state == MachineState::Steaming || current_state == MachineState::HotWater
        {
            // LED1 off, LED2 shows steam/hot-water temperature progress
            (Rgb::off(), temp_color)
        } else {
            // LED1 shows brew temperature, LED2 shows pressure/flow
            let mut pressure_color = Rgb::off();
            if current_state == MachineState::Brewing && a.target_bar > 0.0 {
                if a.flow_limit_ml_s > 0.0 && f.flow_rate_ml_s >= a.flow_limit_ml_s {
                    pressure_color = Rgb::new(255, 128, 0); // Orange — flow limit
                } else if (a.pressure_bar - a.target_bar).abs() < 0.2 {
                    pressure_color = Rgb::new(0, 255, 0); // Green  — on target
                } else if a.pressure_bar < a.target_bar {
                    pressure_color = Rgb::new(0, 0, 255); // Blue   — building
                } else {
                    pressure_color = Rgb::new(255, 0, 0); // Red    — over pressure
                }
            }
            (temp_color, pressure_color)
        };

        set_leds([l1, l2]).await;

        // Refresh LEDs dynamically, or immediately if the state changes
        let _ = select(Timer::after(Duration::from_millis(100)), state_rx.changed()).await;
    }
}
