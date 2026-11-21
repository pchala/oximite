//! WS2812 PIO2 SM1 setup (preserved verbatim) and 10 Hz status LED task.

use embassy_futures::select::select;
use embassy_rp::pio::{Common, Config, Direction, FifoJoin, Pin, ShiftDirection, StateMachine};
use embassy_time::{Duration, Instant, Timer};
use fixed::FixedU32;
use oximite_proto::model::MachineState;
use pio::pio_asm;

use crate::bus;
use crate::leds::{pack_grb, render_leds, Rgb, WARMUP_MS};

pub type LedSm = StateMachine<'static, embassy_rp::peripherals::PIO2, 1>;

pub fn setup_ws2812_sm(
    common: &mut Common<'static, embassy_rp::peripherals::PIO2>,
    sm: &mut LedSm,
    pin: Pin<'static, embassy_rp::peripherals::PIO2>,
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

/// Clocks one frame out to the strip. Both words go into the FIFO back-to-back
/// with no `await` between them, so the PIO never stalls mid-frame.
fn write_frame(sm: &mut LedSm, leds: &[Rgb; 2]) {
    for &led in leds {
        sm.tx().push(pack_grb(led));
    }
}

#[embassy_executor::task]
pub async fn led_task(mut sm: LedSm) {
    let mut state_rx = bus::MACHINE_STATE.receiver().unwrap();
    let mut last_wakeup = Instant::now();
    let mut was_sleeping = false;

    loop {
        let current_state = bus::get_state();

        if was_sleeping && current_state != MachineState::Sleeping {
            last_wakeup = Instant::now();
        }
        was_sleeping = current_state == MachineState::Sleeping;

        let telem = bus::get_telemetry();
        let in_warmup = last_wakeup.elapsed().as_millis() < WARMUP_MS;
        let frame = render_leds(current_state, &telem, in_warmup);
        write_frame(&mut sm, &frame);

        let _ = select(Timer::after(Duration::from_millis(100)), state_rx.changed()).await;
    }
}
