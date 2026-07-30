//! cyw43 radio bring-up and network stack ownership.
//!
//! Runs entirely on core1. Brings up the radio from the firmware blobs in
//! flash, starts embassy-net, then either joins the configured AP or falls
//! back to soft-AP setup mode, spawning the servers in `web_api` and `dhcp`.

use embassy_executor::Spawner;
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::PIO1;
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use crate::dhcp::dhcp_server_task;
use crate::settings::Settings;
use crate::web_api::{tcp_telemetry_task, wifi_server_task};

#[embassy_executor::task]
pub async fn wifi_driver_task(
    runner: cyw43::Runner<
        'static,
        cyw43::SpiBus<Output<'static>, cyw43_pio::PioSpi<'static, PIO1, 0>>,
    >,
) {
    runner.run().await
}

#[embassy_executor::task]
pub async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) {
    runner.run().await
}

/// Entry point for the WiFi stack, spawned onto core1 by `main`. Keeping the
/// task here rather than in `main.rs` means the cyw43 types stay contained in
/// this module.
#[embassy_executor::task]
pub async fn wifi_init_task(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0>,
    force_ap: bool,
) {
    defmt::info!("Wifi: init task started");
    setup_wifi(spawner, pwr, spi, force_ap).await;
}

async fn setup_wifi(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0>,
    force_ap: bool,
) {
    defmt::info!("Wifi: setup_wifi started");
    // Firmware stored at reserved flash addresses (see flash-wifi.bat and memory.x)
    let fw = unsafe { core::slice::from_raw_parts(0x10FB0000 as *const u8, 231077) };
    let clm = unsafe { core::slice::from_raw_parts(0x10FEF000 as *const u8, 984) };
    // cyw43 wants `&Aligned<A4, [u8]>`; `Aligned` is `#[repr(C)]` with a
    // zero-sized alignment marker followed by the payload, so this is a pure
    // metadata-preserving cast. Both blobs sit at 4-byte-aligned flash addresses.
    let fw = unsafe { &*(fw as *const [u8] as *const cyw43::Aligned<cyw43::A4, [u8]>) };

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    defmt::info!("Wifi: calling cyw43::new");
    let (net_device, mut control, runner) =
        cyw43::new(state, pwr, spi, fw, crate::cyw43_nvram::NVRAM).await;

    spawner.spawn(wifi_driver_task(runner).unwrap());
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    static STACK: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<embassy_net::StackResources<5>> = StaticCell::new();
    let mut dhcp_config = embassy_net::DhcpConfig::default();
    dhcp_config.hostname = Some("oximite".try_into().unwrap());

    let (stack_alloc, runner_alloc) = embassy_net::new(
        net_device,
        embassy_net::Config::dhcpv4(dhcp_config),
        RESOURCES.init(embassy_net::StackResources::<5>::new()),
        0x0123_4567_89ab_cdef,
    );
    let stack = STACK.init(stack_alloc);
    spawner.spawn(net_task(runner_alloc).unwrap());

    spawner.spawn(wifi_server_task(stack).unwrap());
    spawner.spawn(tcp_telemetry_task(stack).unwrap());

    let settings = Settings::get().await;
    let is_ap = force_ap || settings.wifi.ssid.is_empty();

    if is_ap {
        defmt::info!("Wi-Fi: Booting strictly in AP Mode");
        stack.set_config_v4(embassy_net::ConfigV4::Static(embassy_net::StaticConfigV4 {
            address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
            gateway: None,
            dns_servers: Default::default(),
        }));
        if let Ok(token) = dhcp_server_task(stack) {
            spawner.spawn(token);
        }
        control.start_ap_wpa2("Oximite-Setup", "password", 6).await;

        core::future::pending::<()>().await;
    } else {
        defmt::info!("Wi-Fi: Booting in Client Mode");

        loop {
            defmt::info!(
                "Wi-Fi: Attempting to connect to SSID: {}",
                settings.wifi.ssid.as_str()
            );
            match control
                .join(
                    settings.wifi.ssid.as_str(),
                    cyw43::JoinOptions::new(settings.wifi.password.as_bytes()),
                )
                .await
            {
                Ok(_) => {
                    defmt::info!("Wi-Fi: Connected to SSID");
                    let mut link_up = false;
                    for _ in 0..50 {
                        if stack.is_link_up() {
                            link_up = true;
                            break;
                        }
                        Timer::after(Duration::from_millis(100)).await;
                    }
                    if link_up {
                        loop {
                            if !stack.is_link_up() {
                                break;
                            }
                            Timer::after(Duration::from_millis(500)).await;
                        }
                    }
                }
                Err(_) => {
                    defmt::warn!("Wi-Fi: Join failed, retrying in 5s...");
                    Timer::after(Duration::from_secs(5)).await;
                }
            }
        }
    }
    defmt::error!("Wi-Fi: Logic task exited unexpectedly!");
}
