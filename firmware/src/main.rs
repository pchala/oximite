//! Entry point and hardware peripheral wiring for the oximite RP2350B firmware.

#![no_std]
#![no_main]

mod adc_task;
mod backoff;
mod board;
mod bus;
mod button_task;
mod buttons;
mod control_task;
mod controller;
mod coordinator_task;
mod cyw43_nvram;
mod dhcp_codec;
mod dhcp_task;
mod diag_task;
mod flow;
mod flow_task;
mod http_codec;
mod http_task;
mod led_task;
mod leds;
mod machine;
mod pid;
mod sensors;
mod storage_task;
mod tcp;
mod wifi_task;

use embassy_executor::Spawner;
use embassy_rp::adc::{Adc, Config as AdcConfig};
use embassy_rp::bind_interrupts;
use embassy_rp::flash::Flash;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::multicore::{spawn_core1, Stack as CoreStack};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0, PIO1, PIO2};
use static_cell::{ConstStaticCell, StaticCell};
use {defmt_rtt as _, panic_probe as _};

static CORE1_STACK: ConstStaticCell<CoreStack<32768>> = ConstStaticCell::new(CoreStack::new());
static EXECUTOR_CORE1: StaticCell<embassy_executor::Executor> = StaticCell::new();

bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    PIO2_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO2>;
    ADC_IRQ_FIFO => embassy_rp::adc::InterruptHandler;
    // Every DMA channel shares DMA_IRQ_0, so both handlers bind to the same line.
    // CH0 = cyw43 SPI (serviced on core1), CH1 = flash.
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>, embassy_rp::dma::InterruptHandler<DMA_CH1>;
});

/// Reads the button ladder at boot. Returns true when Flush is held, which forces AP mode.
async fn flush_held_at_boot(
    adc: &mut Adc<'static, embassy_rp::adc::Async>,
    ch: &mut embassy_rp::adc::Channel<'static>,
) -> bool {
    let mut debouncer = buttons::LadderDebouncer::new();
    for _ in 0..buttons::DEBOUNCE_SAMPLES * 2 {
        let raw = adc.read(ch).await.unwrap_or(u16::MAX);
        debouncer.poll(raw);
        embassy_time::Timer::after_millis(2).await;
    }
    debouncer.stable() == buttons::Reading::Pressed(buttons::Button::Flush)
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut rng = embassy_rp::clocks::RoscRng;
    let chip_id = embassy_rp::otp::get_chipid().unwrap_or(0);
    let raw_seed = chip_id ^ rng.next_u64();
    let net_seed = if raw_seed == 0 {
        0x0123_4567_89ab_cdef
    } else {
        raw_seed
    };

    // Initialize unused pins to Output Low to improve EMI immunity and power efficiency.
    // Safe-boot fallback: if the 'flush' button is held during boot, force AP mode.
    let mut adc = Adc::new(p.ADC, Irqs, AdcConfig::default());
    let mut ch_btn = embassy_rp::adc::Channel::new_pin(p.PIN_42, Pull::None);
    unsafe {
        let p_steal = embassy_rp::Peripherals::steal();
        macro_rules! init_pins_low {
            ($p:expr; $($pin:ident),*) => {
                $(let _ = Output::new($p.$pin, Level::Low);)*
            };
        }
        init_pins_low!(p_steal; PIN_1, PIN_4, PIN_5, PIN_6, PIN_7, PIN_8, PIN_11, PIN_12, PIN_13, PIN_14, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, PIN_22);
        // GP26-27 share the A0-A1 analog nets — keep as HiZ so they don't load the ADC inputs
        macro_rules! init_pins_hiz {
            ($p:expr; $($pin:ident),*) => {
                $(let _ = Input::new($p.$pin, Pull::None);)*
            };
        }
        init_pins_hiz!(p_steal; PIN_26, PIN_27, PIN_28);
    }
    let force_ap = flush_held_at_boot(&mut adc, &mut ch_btn).await;
    if force_ap {
        defmt::warn!("Hardware override: Forcing AP mode!");
    }

    let mut flash: storage_task::AppFlash = Flash::new(p.FLASH, p.DMA_CH1, Irqs);
    let (initial_settings, initial_profiles) = storage_task::load_all(&mut flash).await;
    bus::publish_settings(
        oximite_proto::model::ControlSettings::from(&initial_settings),
        initial_settings.to_public(),
        &initial_settings.wifi,
        1,
        1,
    );
    bus::publish_profiles(&initial_profiles, 1);

    let embassy_rp::pio::Pio {
        common: mut common1,
        sm0: sm1_0,
        irq0: irq1_0,
        ..
    } = embassy_rp::pio::Pio::new(p.PIO1, Irqs);

    let (pwr, spi) = {
        let pwr = Output::new(p.PIN_23, Level::Low);
        let cs = Output::new(p.PIN_25, Level::High);
        let spi = cyw43_pio::PioSpi::new(
            &mut common1,
            sm1_0,
            cyw43_pio::DEFAULT_CLOCK_DIVIDER,
            irq1_0,
            cs,
            p.PIN_24,
            p.PIN_29,
            embassy_rp::dma::Channel::new(p.DMA_CH0, Irqs),
        );
        (pwr, spi)
    };

    let vtor = unsafe { (*cortex_m::peripheral::SCB::PTR).vtor.read() };

    // Move PIO1_IRQ_0 and DMA_IRQ_0 from core0 to core1, so that servicing the
    // cyw43 SPI transfers never interrupts the zero-cross-synced triac/heater
    // timing on core0.
    cortex_m::peripheral::NVIC::mask(embassy_rp::interrupt::Interrupt::PIO1_IRQ_0);
    cortex_m::peripheral::NVIC::mask(embassy_rp::interrupt::Interrupt::DMA_IRQ_0);

    defmt::info!("Spawning Core 1...");
    spawn_core1(p.CORE1, CORE1_STACK.take(), move || {
        unsafe {
            (*cortex_m::peripheral::SCB::PTR).vtor.write(vtor);
            cortex_m::asm::dsb();
            cortex_m::asm::isb();
            cortex_m::peripheral::NVIC::unmask(embassy_rp::interrupt::Interrupt::PIO1_IRQ_0);
            cortex_m::peripheral::NVIC::unmask(embassy_rp::interrupt::Interrupt::DMA_IRQ_0);
        }
        defmt::info!("Core 1: Starting...");

        let executor = EXECUTOR_CORE1.init(embassy_executor::Executor::new());
        executor.run(|spawner| {
            defmt::info!("Core 1: Spawning wifi_init_task");
            spawner
                .spawn(wifi_task::wifi_init_task(spawner, pwr, spi, force_ap, net_seed).unwrap());
        })
    });

    let embassy_rp::pio::Pio {
        common: mut common0,
        mut sm0,
        mut sm1,
        mut sm2,
        ..
    } = embassy_rp::pio::Pio::new(p.PIO0, Irqs);

    let embassy_rp::pio::Pio {
        common: mut common2,
        sm0: mut sm2_0,
        sm1: mut sm2_1,
        ..
    } = embassy_rp::pio::Pio::new(p.PIO2, Irqs);

    let mut flow_pin = common2.make_pio_pin(p.PIN_15);
    flow_pin.set_pull(Pull::Up);
    flow_task::setup_flow_sm(&mut common2, &mut sm2_0, flow_pin);
    spawner.spawn(flow_task::run_flow_task(sm2_0).unwrap());

    let zc_pin = common0.make_pio_pin(p.PIN_10);
    let triac_pin = common0.make_pio_pin(p.PIN_0);
    control_task::setup_trigger_sm(&mut common0, &mut sm1, &zc_pin);
    control_task::setup_triac_sm(&mut common0, &mut sm2, &triac_pin, &zc_pin);

    let heater_pin = common0.make_pio_pin(p.PIN_2);
    control_task::setup_heater_sm(&mut common0, &mut sm0, &heater_pin, &zc_pin);

    let led_pin = common2.make_pio_pin(p.PIN_9);
    led_task::setup_ws2812_sm(&mut common2, &mut sm2_1, led_pin);
    spawner.spawn(led_task::led_task(sm2_1).unwrap());

    core::mem::forget(common0);
    core::mem::forget(common1);
    core::mem::forget(common2);

    let ch_press = embassy_rp::adc::Channel::new_pin(p.PIN_40, Pull::None);
    let ch_temp = embassy_rp::adc::Channel::new_pin(p.PIN_41, Pull::None);
    let valve_output = Output::new(p.PIN_3, Level::Low);

    spawner.spawn(adc_task::adc_task(adc, ch_press, ch_temp, ch_btn).unwrap());
    spawner.spawn(control_task::ac_sync_control_task(sm1, sm2, sm0).unwrap());

    spawner.spawn(button_task::run_button_task().unwrap());

    spawner.spawn(storage_task::storage_task(flash).unwrap());
    spawner.spawn(
        coordinator_task::coordinator_task(valve_output, initial_settings, initial_profiles)
            .unwrap(),
    );
}
