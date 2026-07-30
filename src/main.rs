#![no_std]
#![no_main]

mod adc;
mod board;
mod buttons;
mod calibration;
mod control;
mod coordinator;
mod cyw43_nvram;
mod dhcp;
mod flow_meter;
mod leds;
mod operations;
mod pid;
mod profiles;
mod settings;
mod state;
mod web_api;
mod wifi_task;

use core::ptr::addr_of_mut;
use embassy_executor::Spawner;
use embassy_rp::adc::{Adc, Config as AdcConfig};
use embassy_rp::bind_interrupts;
use embassy_rp::flash::Flash;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::multicore::{spawn_core1, Stack as CoreStack};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0, PIO1, PIO2};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use crate::settings::SettingsStore;

static mut CORE1_STACK: CoreStack<32768> = CoreStack::new();
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

// ==========================================
// MAIN
// ==========================================
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Initialize unused pins to Output Low to improve EMI immunity and power efficiency.
    unsafe {
        let p_steal = embassy_rp::Peripherals::steal();
        macro_rules! init_pins_low {
            ($p:expr; $($pin:ident),*) => {
                $(let _ = Output::new($p.$pin, Level::Low);)*
            };
        }
        init_pins_low!(p_steal; PIN_1, PIN_4, PIN_11, PIN_12, PIN_13, PIN_14, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, PIN_22, PIN_28);
        // GP26-27 share the A0-A1 analog nets — keep as HiZ so they don't load the ADC inputs
        macro_rules! init_pins_hiz {
            ($p:expr; $($pin:ident),*) => {
                $(let _ = Input::new($p.$pin, Pull::None);)*
            };
        }
        init_pins_hiz!(p_steal; PIN_26, PIN_27);
    }
    let mut flash: Flash<'static, _, embassy_rp::flash::Async, { board::FLASH_SIZE }> =
        Flash::new(p.FLASH, p.DMA_CH1, Irqs);
    SettingsStore::load(&mut flash).await;
    crate::profiles::load_all_profiles_from_flash(&mut flash).await;

    // Safe-boot fallback: if the 'flush' button (PIN 8) is held during boot, force AP mode.
    let mut force_ap = false;
    {
        let btn_flush = Input::new(unsafe { embassy_rp::Peripherals::steal().PIN_8 }, Pull::Up);
        if btn_flush.is_low() {
            defmt::warn!("Hardware override: Forcing AP mode!");
            force_ap = true;
        }
    }

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
    // timing on core0. Waking across cores is fine: the thread-mode pender is
    // `sev`, which is broadcast to both cores.
    //
    // ORDER MATTERS: the NVIC is per-core and these lines are only enabled by
    // `Pio::new` / `dma::Channel::new`, which run on core0. Every such call must
    // therefore stay ABOVE this point — embassy-rp 0.10 moved the DMA enable out
    // of `embassy_rp::init` and into `dma::Channel::new`, so constructing a
    // channel after this would silently re-enable DMA_IRQ_0 on core0 and undo
    // the reroute. Today that means `Flash::new` and `PioSpi::new` above.
    //
    // Note the mask covers the whole line, not one channel: DMA_CH1 (flash) is
    // rerouted too, so `settings::flash_update_task` (core0) relies on core1
    // servicing its completions. embassy-rp hardwires every channel to
    // DMA_IRQ_0/`inte(0)`, so the two cannot be split onto separate lines.
    cortex_m::peripheral::NVIC::mask(embassy_rp::interrupt::Interrupt::PIO1_IRQ_0);
    cortex_m::peripheral::NVIC::mask(embassy_rp::interrupt::Interrupt::DMA_IRQ_0);

    defmt::info!("Spawning Core 1...");
    spawn_core1(
        p.CORE1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
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
                spawner.spawn(wifi_task::wifi_init_task(spawner, pwr, spi, force_ap).unwrap());
            })
        },
    );

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

    let adc_peri = p.ADC;
    let adc = Adc::new(adc_peri, Irqs, AdcConfig::default());

    // Flow meter and WS2812 LEDs both live on PIO2 (unrelated to the
    // zero-cross-synced group below), freeing two PIO0 SM slots for the
    // heater's PIO-driven pin control and keeping PIO0 exactly at its
    // 32-word instruction-memory budget (trigger + triac + heater = 25/32).
    let mut flow_pin = common2.make_pio_pin(p.PIN_15);
    flow_pin.set_pull(Pull::Up); // new flow sensor requires pull-up
    flow_meter::setup_flow_sm(&mut common2, &mut sm2_0, flow_pin);
    spawner.spawn(flow_meter::run_flow_task(sm2_0).unwrap());

    let zc_pin = common0.make_pio_pin(p.PIN_10);
    let triac_pin = common0.make_pio_pin(p.PIN_0);
    control::setup_trigger_sm(&mut common0, &mut sm1, &zc_pin);
    control::setup_triac_sm(&mut common0, &mut sm2, &triac_pin, &zc_pin);

    let heater_pin = common0.make_pio_pin(p.PIN_2);
    control::setup_heater_sm(&mut common0, &mut sm0, &heater_pin, &zc_pin);

    let led_pin = common2.make_pio_pin(p.PIN_9);
    leds::setup_ws2812_sm(&mut common2, &mut sm2_1, led_pin);
    spawner.spawn(leds::run_led_task(sm2_1).unwrap());

    // The PIO peripherals stay configured for the whole program, but `main`
    // returns once everything is spawned, which would drop these `Common`
    // handles. embassy-rp refcounts them and, on the last drop, resets every
    // registered PIO pin's FUNCSEL back to NULL — silently disconnecting GP0
    // (triac), GP2 (heater), GP9 (LEDs), GP10 (zero-cross) and GP15 (flow).
    // Worse, embassy-rp (still, as of 0.10) keeps that refcount in a single
    // `static` shared by PIO0/PIO1/PIO2 — a function-local static inside a
    // default trait method is one item, not one per impl — so the three blocks
    // corrupt each other's counts and the wipe triggers here. Leak the handles:
    // they are owned for the lifetime of the firmware by design.
    core::mem::forget(common0);
    core::mem::forget(common1);
    core::mem::forget(common2);

    // A0 (GP40) = pressure sensor, A1 (GP41) = temperature sensor.
    // GP26-28 are already held HiZ above — they share the same PCB nets as A0-A2.
    let ch_press = embassy_rp::adc::Channel::new_pin(p.PIN_40, Pull::None);
    let ch_temp = embassy_rp::adc::Channel::new_pin(p.PIN_41, Pull::None);
    let valve_output = Output::new(p.PIN_3, Level::Low);

    spawner.spawn(adc::adc_task(adc, ch_press, ch_temp).unwrap());

    spawner.spawn(control::ac_sync_control_task(sm1, sm2, sm0).unwrap());

    let btn_power = Input::new(p.PIN_5, Pull::Up);
    let btn_brew = Input::new(p.PIN_6, Pull::Up);
    let btn_steam = Input::new(p.PIN_7, Pull::Up);
    let btn_flush = Input::new(p.PIN_8, Pull::Up);
    spawner.spawn(buttons::run_button_task(btn_power, btn_brew, btn_steam, btn_flush).unwrap());

    // Spawn the decoupled architectural tasks
    spawner.spawn(leds::led_update_task().unwrap());
    spawner.spawn(settings::flash_update_task(flash).unwrap());
    spawner.spawn(coordinator::coordinator_task().unwrap());
    spawner.spawn(operations::hardware_task(valve_output).unwrap());
}
