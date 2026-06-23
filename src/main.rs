#![no_std]
#![no_main]

mod buttons;
mod control;
mod flow_meter;
mod leds;
mod settings;
mod state;
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
use embassy_rp::peripherals::{PIO0, PIO1};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _, rp2040_boot2 as _};

use crate::leds::Rgb;
use crate::settings::SettingsManager;
use crate::state::{MachineCommand, MachineState, MACHINE_STATE, SIG_COMMAND};

static mut CORE1_STACK: CoreStack<32768> = CoreStack::new();
static EXECUTOR_CORE1: StaticCell<embassy_executor::Executor> = StaticCell::new();

pub enum SystemEvent {
    SaveSettings(SettingsManager),
    SaveProfile(u8),
    DeleteProfile(u8),
}

pub static SIG_SYSTEM_EVENT: Signal<CriticalSectionRawMutex, SystemEvent> = Signal::new();
pub static SIG_WIFI_RECONFIG: Signal<CriticalSectionRawMutex, ()> = Signal::new();

bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    ADC_IRQ_FIFO => embassy_rp::adc::InterruptHandler;
});

// ==========================================
// POWER MANAGEMENT
// ==========================================
async fn go_to_sleep() {
    defmt::info!("Power Management: Going to SLEEP mode.");
    crate::state::set_state(MachineState::Sleeping);
    crate::control::set_target_temp(control::TargetTempMode::Off).await;
}

async fn wake_up() {
    defmt::info!("Power Management: WAKING UP.");
    crate::state::set_state(MachineState::Idle);
    crate::control::set_target_temp(control::TargetTempMode::Brew).await;
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
        } else if current_state == MachineState::Steaming {
            // LED1 off, LED2 shows steam temperature progress
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
            SystemEvent::SaveSettings(old_s) => {
                let new_s = SettingsManager::get().await;
                SettingsManager::save_changes_to_flash(&mut flash, &old_s, &new_s).await;
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
async fn transition_state(new_state: MachineState, target_mode: Option<control::TargetTempMode>) {
    crate::flow_meter::FlowMonitor::new().reset_volume().await;
    crate::state::set_state(new_state);
    control::SIG_PROFILE_ABORT.signal(());
    if let Some(m) = target_mode {
        control::set_target_temp(m).await;
    }
}

async fn stop_to_idle() {
    crate::state::set_state(MachineState::Idle);
    control::SIG_PROFILE_ABORT.signal(());
    control::set_target_temp(control::TargetTempMode::Brew).await;
    control::set_target_pressure(0.0);
    control::set_direct_pump(None);
}

// ==========================================
// STATE MACHINE TRANSITION TABLE
// ==========================================
async fn handle_command(state: MachineState, cmd: MachineCommand) {
    match (state, cmd) {
        // Power toggle
        (MachineState::Idle, MachineCommand::TogglePower) => {
            go_to_sleep().await;
        }
        (_, MachineCommand::TogglePower) => {
            // If busy, act as Stop
            stop_to_idle().await;
        }

        // Brew
        (MachineState::Idle, MachineCommand::Brew) => {
            transition_state(MachineState::Brewing, Some(control::TargetTempMode::Brew)).await;
            let p = SettingsManager::get_default_profile().await;
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::RunProfile(p));
        }
        (MachineState::Idle, MachineCommand::RunProfile(p)) => {
            transition_state(MachineState::Brewing, Some(control::TargetTempMode::Brew)).await;
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::RunProfile(p));
        }

        // Flush
        (MachineState::Idle, MachineCommand::Flush) => {
            transition_state(MachineState::Pumping, Some(control::TargetTempMode::Brew)).await;
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::DirectPump(80.0));
        }
        (MachineState::Steaming, MachineCommand::Flush) => {
            transition_state(MachineState::Cooling, Some(control::TargetTempMode::Brew)).await;
            control::set_direct_pump(None);
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::CooldownFlush);
        }

        // Steam
        (MachineState::Idle, MachineCommand::Steam) => {
            transition_state(MachineState::Steaming, Some(control::TargetTempMode::Steam)).await;
            control::set_direct_pump(None);
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::Steam);
        }
        (MachineState::Steaming, MachineCommand::Steam) => {
            stop_to_idle().await;
        }

        // Descale
        (MachineState::Idle, MachineCommand::Descale) => {
            transition_state(MachineState::Descaling, Some(control::TargetTempMode::Descale)).await;
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::Descale);
        }

        // Direct pump (dev/diagnostic, valid from any state)
        (_, MachineCommand::DirectPump(power)) => {
            transition_state(MachineState::Brewing, Some(control::TargetTempMode::Brew)).await;
            control::SIG_HARDWARE_CMD.signal(control::HardwareCommand::DirectPump(power));
        }

        // Stop: button press during active process, explicit Stop, or natural finish
        (
            MachineState::Brewing
            | MachineState::Pumping
            | MachineState::Cooling
            | MachineState::Descaling,
            MachineCommand::Brew | MachineCommand::Steam | MachineCommand::Flush,
        )
        | (_, MachineCommand::Stop)
        | (_, MachineCommand::ProfileFinished) => {
            stop_to_idle().await;
        }

        // Settings (valid in any state)
        (_, MachineCommand::SaveSettings(new_s)) => {
            let old_s = SettingsManager::get().await;
            let wifi_changed =
                old_s.wifi.ssid != new_s.wifi.ssid || old_s.wifi.password != new_s.wifi.password;
            SettingsManager::update_ram(new_s).await;
            SIG_SYSTEM_EVENT.signal(SystemEvent::SaveSettings(old_s));
            if wifi_changed {
                SIG_WIFI_RECONFIG.signal(());
            }
        }

        (state, cmd) => {
            // Safety catch-all: ignore invalid/dangerous commands
            defmt::warn!(
                "Invalid transition requested while in state {:?} cmd {:?}",
                state,
                cmd
            );
        }
    }
}

// ==========================================
// COORDINATOR TASK
// ==========================================
#[embassy_executor::task]
async fn coordinator_task() {
    const SLEEP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
    let mut last_activity = embassy_time::Instant::now();

    crate::state::set_state(MachineState::Idle);
    wake_up().await;

    loop {
        match select(SIG_COMMAND.wait(), Timer::after(Duration::from_millis(100))).await {
            Either::Second(_) => {
                if crate::state::get_state() == MachineState::Idle
                    && last_activity.elapsed() >= SLEEP_TIMEOUT
                {
                    go_to_sleep().await;
                }
            }

            Either::First(cmd) => {
                defmt::info!("Coordinator received command: {:?}", cmd);
                last_activity = embassy_time::Instant::now();

                // Auto-wake: any command except SaveSettings wakes the machine.
                // The waking command itself is dropped — we don't want to start
                // a cold brew if the user pressed Brew just to wake it up.
                if crate::state::get_state() == MachineState::Sleeping {
                    if let MachineCommand::SaveSettings(_) = cmd {
                        // fall through — settings save silently without waking
                    } else {
                        wake_up().await;
                        continue;
                    }
                }

                handle_command(crate::state::get_state(), cmd).await;
            }
        }
    }
}

// ==========================================
// HARDWARE EXECUTOR TASK
// ==========================================
async fn run_cancellable<F: core::future::Future>(
    valve: &mut Output<'static>,
    valve_high: bool,
    action_name: &'static str,
    fut: F,
) {
    if valve_high {
        valve.set_high();
    } else {
        valve.set_low();
    }
    control::SIG_PROFILE_ABORT.reset();
    let abort = pin!(control::SIG_PROFILE_ABORT.wait());
    let run = pin!(fut);
    match select(run, abort).await {
        Either::First(_) => {
            defmt::info!("Hardware: {} finished naturally", action_name);
            crate::state::SIG_COMMAND.signal(MachineCommand::ProfileFinished);
        }
        Either::Second(_) => {
            defmt::warn!("Hardware: {} aborted", action_name);
        }
    }
    valve.set_low();
}

// Separated from the hardware loop so the loop stays readable.
// pumped_ml must be captured synchronously before any await.
async fn record_operation(is_descale: bool, pumped_ml: f32) {
    let mut s = SettingsManager::get().await;
    let old_s = s.clone();

    if is_descale {
        s.usage.ml_at_last_descale = s.usage.total_ml_all_time;
        defmt::info!(
            "Stored ml_at_last_descale = {} after descale",
            s.usage.ml_at_last_descale
        );
    } else {
        s.usage.total_ml_all_time += pumped_ml;
        if pumped_ml > 0.0 {
            defmt::info!("Added {} ml to total usage", pumped_ml);
        }
    }

    if s.usage != old_s.usage {
        SettingsManager::update_ram(s).await;
        SIG_SYSTEM_EVENT.signal(SystemEvent::SaveSettings(old_s));
    }
}

#[embassy_executor::task]
async fn hardware_task(mut valve: Output<'static>) {
    loop {
        let cmd = control::SIG_HARDWARE_CMD.wait().await;
        defmt::info!("Hardware task received command");

        let is_descale = matches!(cmd, control::HardwareCommand::Descale);

        match cmd {
            control::HardwareCommand::RunProfile(p) => {
                defmt::info!("Hardware: Starting profile '{}'", p.name.as_str());
                run_cancellable(&mut valve, true, "Profile", control::execute_profile(p)).await;
            }
            control::HardwareCommand::Steam => {
                defmt::info!("Hardware: Starting steam");
                run_cancellable(&mut valve, false, "Steam", control::execute_steam()).await;
            }
            control::HardwareCommand::Descale => {
                defmt::info!("Hardware: Starting descale");
                run_cancellable(&mut valve, true, "Descale", control::execute_descale()).await;
            }
            control::HardwareCommand::CooldownFlush => {
                defmt::info!("Hardware: Starting cooldown flush");
                run_cancellable(
                    &mut valve,
                    false,
                    "Cooldown flush",
                    control::execute_cooldown_flush(),
                )
                .await;
            }
            control::HardwareCommand::DirectPump(power) => {
                defmt::info!("Hardware: Starting direct pump {}%", power);
                run_cancellable(
                    &mut valve,
                    true,
                    "Direct pump",
                    control::execute_direct_pump(power),
                )
                .await;
            }
        }

        // Capture volume synchronously — no await, no yield — before the coordinator
        // can process ProfileFinished and call transition_state() → reset_volume().
        let pumped_ml = crate::flow_meter::FLOW_WATCH
            .try_get()
            .unwrap_or_default()
            .total_volume_ml;

        record_operation(is_descale, pumped_ml).await;
    }
}

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
    }
    let mut flash: Flash<'static, _, embassy_rp::flash::Async, 2097152> =
        Flash::new(p.FLASH, p.DMA_CH1);
    SettingsManager::load_from_flash(&mut flash).await;
    crate::settings::load_all_profiles_from_flash(&mut flash).await;

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
            p.DMA_CH0,
        );
        (pwr, spi)
    };

    defmt::info!("Spawning Core 1...");
    spawn_core1(
        p.CORE1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            defmt::info!("Core 1: Starting...");
            let executor = EXECUTOR_CORE1.init(embassy_executor::Executor::new());
            executor.run(|spawner| {
                defmt::info!("Core 1: Spawning wifi_init_task");
                spawner.spawn(wifi_init_task(spawner, pwr, spi)).unwrap();
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
    let valve_output = Output::new(p.PIN_3, Level::Low);

    spawner
        .spawn(control::adc_task(adc, ch_press, ch_temp))
        .unwrap();

    spawner
        .spawn(control::ac_sync_control_task(sm1, sm2, heater_output))
        .unwrap();

    let btn_power = Input::new(p.PIN_5, Pull::Up);
    let btn_brew = Input::new(p.PIN_6, Pull::Up);
    let btn_steam = Input::new(p.PIN_7, Pull::Up);
    let btn_flush = Input::new(p.PIN_8, Pull::Up);
    spawner
        .spawn(buttons::run_button_task(
            btn_power, btn_brew, btn_steam, btn_flush,
        ))
        .unwrap();

    // Spawn the decoupled architectural tasks
    spawner.spawn(led_update_task()).unwrap();
    spawner.spawn(system_events_task(flash)).unwrap();
    spawner.spawn(coordinator_task()).unwrap();
    spawner.spawn(hardware_task(valve_output)).unwrap();
}

#[embassy_executor::task]
async fn wifi_init_task(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0, embassy_rp::peripherals::DMA_CH0>,
) {
    defmt::info!("Wifi: init task started");
    wifi_task::setup_wifi(spawner, pwr, spi).await;
}
