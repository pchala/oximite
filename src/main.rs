#![no_std]
#![no_main]

mod buttons;
mod control;
mod flow_meter;
mod leds;
mod settings;
mod state;
#[cfg(feature = "hil_test")]
mod uart_task;
#[cfg(feature = "wifi")]
mod wifi_task;

use core::pin::pin;
use core::ptr::addr_of_mut;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_rp::adc::{Adc, Config as AdcConfig};
use embassy_rp::bind_interrupts;
use embassy_rp::flash::Flash;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::multicore::{spawn_core1, Stack as CoreStack};
use embassy_rp::peripherals::{PIO0, PIO1, UART1};
use embassy_rp::pwm::{Config as PwmConfig, Pwm};
#[cfg(feature = "hil_test")]
use embassy_rp::uart::{Config as UartConfig, Uart};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use fixed::FixedU16;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _, rp2040_boot2 as _};

use crate::leds::Rgb;
use crate::settings::{BrewProfile, SettingsManager};
use crate::state::{MachineCommand, MachineState, MACHINE_STATE, SIG_COMMAND};

static mut CORE1_STACK: CoreStack<32768> = CoreStack::new();
static EXECUTOR_CORE1: StaticCell<embassy_executor::Executor> = StaticCell::new();

pub enum SystemEvent {
    SaveSettings,
    SaveProfile(u8),
    DeleteProfile(u8),
}

pub static SIG_SYSTEM_EVENT: Signal<CriticalSectionRawMutex, SystemEvent> = Signal::new();
pub static SIG_WIFI_RECONFIG: Signal<CriticalSectionRawMutex, ()> = Signal::new();

bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    ADC_IRQ_FIFO => embassy_rp::adc::InterruptHandler;
    UART1_IRQ => embassy_rp::uart::InterruptHandler<UART1>;
});

#[cfg(feature = "simulation")]
#[embassy_executor::task]
async fn mains_50hz_task(mut pwm: Pwm<'static>) {
    let mut config = PwmConfig::default();
    config.divider = FixedU16::from_num(125.0);
    config.top = 20000;
    config.compare_a = 9600;
    pwm.set_config(&config);
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[cfg(feature = "simulation")]
#[embassy_executor::task]
async fn flow_sim_task(mut pwm: Pwm<'static>) {
    let mut config = PwmConfig::default();
    config.divider = FixedU16::from_num(125.0);
    config.top = 50000;
    config.compare_a = 25000;
    loop {
        pwm.set_config(&config);
        Timer::after(Duration::from_secs(1)).await;
        let mut stop = config.clone();
        stop.compare_a = 0;
        pwm.set_config(&stop);
        Timer::after(Duration::from_secs(3)).await;
    }
}

// ==========================================
// POWER MANAGEMENT
// ==========================================
async fn go_to_sleep() {
    defmt::info!("Power Management: Going to SLEEP mode.");
    crate::state::set_state(MachineState::Sleeping);
    crate::control::set_target_temp(0.0);
}

async fn wake_up() {
    defmt::info!("Power Management: WAKING UP.");
    crate::state::set_state(MachineState::Idle);
    let s = crate::settings::SettingsManager::get().await;
    crate::control::set_target_temp(s.brew_temp);
}

// ==========================================
// DECOUPLED LED UI TASK
// ==========================================
#[embassy_executor::task]
async fn led_update_task() {
    let mut state_rx = MACHINE_STATE.receiver().unwrap();

    loop {
        let current_state = crate::state::get_state();
        let a = control::AdcMonitor::new().get_state().await;
        let f = flow_meter::FlowMonitor::new().get_state().await;

        // LED 1 (Temperature)
        // Colder than set (Heating): Blue
        // In Range (Ready): Solid White.
        // Hotter than set (Over-temp): Red.
        let l1 = if a.temp_c < a.target_temp - 1.0 {
            Rgb::new(0, 0, 255) // Blue
        } else if a.temp_c > a.target_temp + 1.0 {
            Rgb::new(255, 0, 0) // Red
        } else {
            Rgb::new(0, 255, 0) // Green
        };

        // LED 2 (Pressure & Flow)
        let mut l2 = Rgb::off();

        if (current_state == MachineState::Brewing) && (a.target_bar > 0.0) {
            if (a.flow_limit_ml_s > 0.0) && (f.flow_rate_ml_s >= a.flow_limit_ml_s) {
                l2 = Rgb::new(255, 128, 0); // Pulsing Orange
            } else if (a.pressure_bar - a.target_bar).abs() < 0.2 {
                l2 = Rgb::new(0, 255, 0); // Solid Green
            } else if a.pressure_bar < a.target_bar {
                l2 = Rgb::new(0, 0, 255); // Pulsing Blue
            }
        }

        leds::set_leds([l1, l2]).await;

        // Refresh LEDs dynamically, or immediately if the state changes
        let _ = select(Timer::after(Duration::from_millis(100)), state_rx.changed()).await;
    }
}

// ==========================================
// BACKGROUND FLASH EVENT HANDLER
// ==========================================
#[embassy_executor::task]
async fn system_events_task(
    mut flash: Flash<'static, embassy_rp::peripherals::FLASH, embassy_rp::flash::Async, 2097152>,
) {
    loop {
        let event = SIG_SYSTEM_EVENT.wait().await;
        match event {
            SystemEvent::SaveSettings => {
                let s = SettingsManager::get().await;
                SettingsManager::save_to_flash(&mut flash, &s).await;
            }
            SystemEvent::SaveProfile(slot) => {
                if let Some(p) = crate::settings::get_profile_from_ram(slot).await {
                    let _ = crate::settings::save_profile_to_flash(&mut flash, slot, &p).await;
                }
            }
            SystemEvent::DeleteProfile(slot) => {
                let _ = crate::settings::delete_profile_from_flash(&mut flash, slot).await;
            }
        }
    }
}

// ==========================================
// CENTRAL STATE DICTATOR (The Coordinator)
// ==========================================
#[embassy_executor::task]
async fn coordinator_task() {
    const SLEEP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
    let mut last_activity = embassy_time::Instant::now();

    crate::state::set_state(MachineState::Idle);
    wake_up().await;

    loop {
        let current_state = crate::state::get_state();

        // Non-blocking wait: Wake up on Command OR Timeout
        match select(SIG_COMMAND.wait(), Timer::after(Duration::from_millis(100))).await {
            // 1. Timeout Check
            Either::Second(_) => {
                if current_state == MachineState::Idle && last_activity.elapsed() >= SLEEP_TIMEOUT {
                    crate::state::set_state(MachineState::Sleeping);
                    go_to_sleep().await;
                }
            }

            // 2. Command Processing
            Either::First(cmd) => {
                defmt::info!("Coordinator received command: {:?}", cmd);
                last_activity = embassy_time::Instant::now();

                // Auto-wake mechanism
                if current_state == MachineState::Sleeping {
                    wake_up().await;
                    crate::state::set_state(MachineState::Idle);
                    if let MachineCommand::Stop | MachineCommand::SaveSettings(_) = cmd {
                        continue;
                    }
                }

                // Strictly typed State Machine routing
                match (current_state, cmd.clone()) {
                    (MachineState::Idle, MachineCommand::Brew)
                    | (MachineState::Idle, MachineCommand::RunProfile(_))
                    | (MachineState::Idle, MachineCommand::Flush) => {
                        crate::flow_meter::FlowMonitor::new().reset_volume().await;
                        crate::state::set_state(MachineState::Brewing);
                        control::SIG_PROFILE_ABORT.signal(());
                        control::set_target_temp(SettingsManager::get().await.brew_temp);

                        let p = match cmd {
                            MachineCommand::RunProfile(p) => p,
                            MachineCommand::Brew => SettingsManager::get_default_profile().await,
                            MachineCommand::Flush => {
                                let json = r#"{"name":"Flush","steps":[{"time_s":5.0,"volume":0.0,"pressure":4.0,"flow":0.0}]}"#;
                                serde_json_core::from_str::<BrewProfile>(json).unwrap().0
                            }
                            _ => unreachable!(),
                        };
                        control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::RunProfile(p));
                    }

                    (MachineState::Idle, MachineCommand::Steam) => {
                        crate::flow_meter::FlowMonitor::new().reset_volume().await;
                        crate::state::set_state(MachineState::Steaming);
                        control::SIG_PROFILE_ABORT.signal(());
                        control::set_target_temp(SettingsManager::get().await.steam_temp);
                        control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::Steam);
                    }

                    (MachineState::Idle, MachineCommand::Descale) => {
                        crate::flow_meter::FlowMonitor::new().reset_volume().await;
                        crate::state::set_state(MachineState::Descaling);
                        control::SIG_PROFILE_ABORT.signal(());
                        control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::Descale);
                    }

                    // Global Stop - Instantly rips machine back to safe Idle
                    (_, MachineCommand::Stop) | (_, MachineCommand::ProfileFinished) => {
                        crate::state::set_state(MachineState::Idle);
                        control::SIG_PROFILE_ABORT.signal(());
                        control::set_target_temp(SettingsManager::get().await.brew_temp);
                        control::set_target_pressure(0.0);
                    }

                    // Settings processing (valid in any state)
                    (_, MachineCommand::SaveSettings(new_s)) => {
                        let old_s = SettingsManager::get().await;
                        let wifi_changed = old_s.wifi_ssid != new_s.wifi_ssid
                            || old_s.wifi_password != new_s.wifi_password;
                        SettingsManager::update_ram(new_s).await;
                        SIG_SYSTEM_EVENT.signal(SystemEvent::SaveSettings);
                        if wifi_changed {
                            SIG_WIFI_RECONFIG.signal(());
                        }
                    }

                    (state, cmd) => {
                        // Safety Catch-All: Ignore invalid/dangerous commands
                        defmt::warn!(
                            "Invalid transition requested while in state {:?} cmd {:?}",
                            state,
                            cmd
                        );
                    }
                }
            }
        }
    }
}

// ==========================================
// HARDWARE EXECUTOR TASK
// ==========================================
#[embassy_executor::task]
async fn hardware_task() {
    loop {
        let cmd = control::SIG_HARDWARE_CMD.wait().await;
        defmt::info!("Hardware task received command");
        match cmd {
            control::HardwareCommand::RunProfile(p) => {
                defmt::info!("Hardware: Starting profile '{}'", p.name.as_str());
                control::SIG_PROFILE_ABORT.reset();
                let abort = pin!(control::SIG_PROFILE_ABORT.wait());
                let run = pin!(control::execute_profile(p));
                match select(run, abort).await {
                    Either::First(_) => {
                        defmt::info!("Hardware: Profile finished naturally");
                        crate::state::SIG_COMMAND.signal(MachineCommand::ProfileFinished);
                    }
                    Either::Second(_) => {
                        defmt::warn!("Hardware: Profile aborted");
                    }
                }
            }
            control::HardwareCommand::Steam => {
                defmt::info!("Hardware: Starting steam");
                control::SIG_PROFILE_ABORT.reset();
                let abort = pin!(control::SIG_PROFILE_ABORT.wait());
                let run = pin!(control::execute_steam());
                match select(run, abort).await {
                    Either::First(_) => {
                        defmt::info!("Hardware: Steam finished naturally");
                        crate::state::SIG_COMMAND.signal(MachineCommand::ProfileFinished);
                    }
                    Either::Second(_) => {
                        defmt::warn!("Hardware: Steam aborted");
                    }
                }
            }
            control::HardwareCommand::Descale => {
                defmt::info!("Hardware: Starting descale");
                control::SIG_PROFILE_ABORT.reset();
                let abort = pin!(control::SIG_PROFILE_ABORT.wait());
                let run = pin!(control::execute_descale());
                match select(run, abort).await {
                    Either::First(_) => {
                        defmt::info!("Hardware: Descale finished naturally");
                        crate::state::SIG_COMMAND.signal(MachineCommand::ProfileFinished);
                    }
                    Either::Second(_) => {
                        defmt::warn!("Hardware: Descale aborted");
                    }
                }
            }
        }
    }
}

// ==========================================
// MAIN
// ==========================================
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Initialize all pins to Output Low to improve EMI immunity and power efficiency.
    // The used pins will be seamlessly redefined by the rest of the code.
    unsafe {
        let p_steal = embassy_rp::Peripherals::steal();
        let _ = Output::new(p_steal.PIN_0, Level::Low);
        let _ = Output::new(p_steal.PIN_1, Level::Low);
        let _ = Output::new(p_steal.PIN_2, Level::Low);
        let _ = Output::new(p_steal.PIN_3, Level::Low);
        let _ = Output::new(p_steal.PIN_4, Level::Low);
        let _ = Output::new(p_steal.PIN_5, Level::Low);
        let _ = Output::new(p_steal.PIN_6, Level::Low);
        let _ = Output::new(p_steal.PIN_7, Level::Low);
        let _ = Output::new(p_steal.PIN_8, Level::Low);
        let _ = Output::new(p_steal.PIN_9, Level::Low);
        let _ = Output::new(p_steal.PIN_10, Level::Low);
        let _ = Output::new(p_steal.PIN_11, Level::Low);
        let _ = Output::new(p_steal.PIN_12, Level::Low);
        let _ = Output::new(p_steal.PIN_13, Level::Low);
        let _ = Output::new(p_steal.PIN_14, Level::Low);
        let _ = Output::new(p_steal.PIN_15, Level::Low);
        let _ = Output::new(p_steal.PIN_16, Level::Low);
        let _ = Output::new(p_steal.PIN_17, Level::Low);
        let _ = Output::new(p_steal.PIN_18, Level::Low);
        let _ = Output::new(p_steal.PIN_19, Level::Low);
        let _ = Output::new(p_steal.PIN_20, Level::Low);
        let _ = Output::new(p_steal.PIN_21, Level::Low);
        let _ = Output::new(p_steal.PIN_22, Level::Low);
        let _ = Output::new(p_steal.PIN_23, Level::Low);
        let _ = Output::new(p_steal.PIN_24, Level::Low);
        let _ = Output::new(p_steal.PIN_25, Level::Low);
        let _ = Output::new(p_steal.PIN_26, Level::Low);
        let _ = Output::new(p_steal.PIN_27, Level::Low);
        let _ = Output::new(p_steal.PIN_28, Level::Low);
        let _ = Output::new(p_steal.PIN_29, Level::Low);
    }
    let mut flash: Flash<'static, _, embassy_rp::flash::Async, 2097152> =
        Flash::new(p.FLASH, p.DMA_CH1);
    SettingsManager::load_from_flash(&mut flash).await;
    crate::settings::load_all_profiles_from_flash(&mut flash).await;

    #[cfg(feature = "wifi")]
    let embassy_rp::pio::Pio {
        common: mut common1,
        sm0: sm1_0,
        irq0: irq1_0,
        ..
    } = embassy_rp::pio::Pio::new(p.PIO1, Irqs);

    #[cfg(feature = "wifi")]
    let (pwr, spi) = {
        let pwr = Output::new(p.PIN_23, Level::Low);
        let cs = Output::new(p.PIN_25, Level::High);
        let spi = cyw43_pio::PioSpi::new(
            &mut common1,
            sm1_0,
            cyw43_pio::OVERCLOCK_CLOCK_DIVIDER,
            irq1_0,
            cs,
            p.PIN_24,
            p.PIN_29,
            p.DMA_CH0,
        );
        (pwr, spi)
    };

    #[cfg(feature = "hil_test")]
    let uart_halves = {
        let mut uart_cfg = UartConfig::default();
        uart_cfg.baudrate = 2_000_000;
        let uart1 = Uart::new(
            p.UART1, p.PIN_4, p.PIN_5, Irqs, p.DMA_CH2, p.DMA_CH3, uart_cfg,
        );
        uart1.split()
    };

    defmt::info!("Spawning Core 1...");
    spawn_core1(
        p.CORE1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            defmt::info!("Core 1: Starting...");
            let executor = EXECUTOR_CORE1.init(embassy_executor::Executor::new());
            executor.run(|spawner| {
                #[cfg(feature = "wifi")]
                {
                    defmt::info!("Core 1: Spawning wifi_init_task");
                    spawner.spawn(wifi_init_task(spawner, pwr, spi)).unwrap();
                }
                #[cfg(feature = "hil_test")]
                {
                    let (uart_tx, uart_rx) = uart_halves;
                    spawner.spawn(uart_task::uart_tx_task(uart_tx)).unwrap();
                    spawner.spawn(uart_task::uart_rx_task(uart_rx)).unwrap();
                }
            })
        },
    );

    let embassy_rp::pio::Pio {
        common: mut common0,
        mut sm0,
        mut sm1,
        mut sm2,
        mut sm3,
        ..
    } = embassy_rp::pio::Pio::new(p.PIO0, Irqs);

    let adc_peri = p.ADC;
    let adc = Adc::new(adc_peri, Irqs, AdcConfig::default());

    let flow_pin = common0.make_pio_pin(p.PIN_15);
    flow_meter::setup_flow_sm(&mut common0, &mut sm0, flow_pin);
    spawner.spawn(flow_meter::run_flow_task(sm0)).unwrap();

    let zc_pin = common0.make_pio_pin(p.PIN_10);
    let triac_pin = common0.make_pio_pin(p.PIN_0);
    control::setup_trigger_sm(&mut common0, &mut sm1, &zc_pin);
    control::setup_triac_sm(&mut common0, &mut sm2, &triac_pin, &zc_pin);

    let led_pin = common0.make_pio_pin(p.PIN_9);
    leds::setup_ws2812_sm(&mut common0, &mut sm3, led_pin);
    spawner.spawn(leds::run_led_task(sm3)).unwrap();

    let ch_press = embassy_rp::adc::Channel::new_pin(p.PIN_26, Pull::None);
    let ch_temp = embassy_rp::adc::Channel::new_pin(p.PIN_27, Pull::None);
    let heater_output = Output::new(p.PIN_2, Level::Low);

    spawner
        .spawn(control::adc_task(adc, ch_press, ch_temp))
        .unwrap();

    spawner
        .spawn(control::run_unified_hardware_control(
            sm1,
            sm2,
            heater_output,
        ))
        .unwrap();

    let btn_brew = Input::new(p.PIN_6, Pull::Up);
    let btn_steam = Input::new(p.PIN_7, Pull::Up);
    let btn_flush = Input::new(p.PIN_8, Pull::Up);
    spawner
        .spawn(buttons::run_button_task(btn_brew, btn_steam, btn_flush))
        .unwrap();

    #[cfg(feature = "simulation")]
    {
        let mut mains_conf = PwmConfig::default();
        mains_conf.divider = FixedU16::from_num(125.0);
        mains_conf.top = 20000;
        mains_conf.compare_a = 9600;
        let pwm_mains = Pwm::new_output_a(p.PWM_SLICE1, p.PIN_18, mains_conf);
        spawner.spawn(mains_50hz_task(pwm_mains)).unwrap();

        let mut flow_conf = PwmConfig::default();
        flow_conf.divider = FixedU16::from_num(125.0);
        flow_conf.top = 50000;
        flow_conf.compare_a = 25000;
        let pwm_flow = Pwm::new_output_a(p.PWM_SLICE0, p.PIN_16, flow_conf);
        spawner.spawn(flow_sim_task(pwm_flow)).unwrap();
    }

    // Spawn the decoupled architectural tasks
    spawner.spawn(led_update_task()).unwrap();
    spawner.spawn(system_events_task(flash)).unwrap();
    spawner.spawn(coordinator_task()).unwrap();
    spawner.spawn(hardware_task()).unwrap();
}

#[cfg(feature = "wifi")]
#[embassy_executor::task]
async fn wifi_init_task(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0, embassy_rp::peripherals::DMA_CH0>,
) {
    defmt::info!("Wifi: init task started");
    wifi_task::setup_wifi(spawner, pwr, spi).await;
}
