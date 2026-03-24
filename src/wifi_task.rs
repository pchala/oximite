use core::str::from_utf8;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::{DMA_CH0, PIO1};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use serde::{Deserialize, Serialize};
use static_cell::StaticCell;

use crate::control::AdcMonitor;
use crate::flow_meter::FlowMonitor;
use crate::settings::{
    BrewProfile, HardwareSettings, MachineSettings, PidSettings, SettingsManager, WifiSettings,
};
use crate::state::{get_state, MachineCommand, SIG_COMMAND};
use crate::{SystemEvent, SIG_SYSTEM_EVENT, SIG_WIFI_RECONFIG};

static INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

#[derive(Deserialize)]
struct ApiCommand<'a> {
    cmd: &'a str,
    profile: Option<BrewProfile>,
    slot: Option<u8>,
    machine: Option<MachineSettings>,
    hardware: Option<HardwareSettings>,
    temp_pid: Option<PidSettings>,
    press_pid: Option<PidSettings>,
    wifi: Option<WifiSettings>,
    power: Option<f32>,
}

#[derive(Serialize)]
struct TelemetryData {
    t: f32,
    p: f32,
    fl: f32,
    vol: f32,
    st: u32,
}

#[derive(Serialize)]
struct ProfileHeader<'a> {
    slot: u8,
    name: &'a str,
}

#[embassy_executor::task]
pub async fn wifi_server_task(stack: &'static embassy_net::Stack<'static>) {
    let mut rx_buffer = [0; 2048];
    let mut tx_buffer = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(5)));

        if let Err(_) = socket.accept(80).await {
            continue;
        }

        let remote_endpoint = socket.remote_endpoint();
        defmt::info!("HTTP: Accepted connection from {}", remote_endpoint);

        let mut buf = [0u8; 4096];
        if let Ok(n) = socket.read(&mut buf).await {
            if n > 0 {
                let request = from_utf8(&buf[..n]).unwrap_or("");
                let first_line = request.lines().next().unwrap_or("");
                defmt::info!("HTTP Request: {}", first_line);

                if request.starts_with("GET / ") || request.starts_with("GET /index.html") {
                    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(INDEX_HTML_GZ).await;
                } else if request.starts_with("GET /api/telemetry") {
                    let a = AdcMonitor::new().get_state().await;
                    let f = FlowMonitor::new().get_state().await;
                    let st = get_state() as u32;
                    let data = TelemetryData {
                        t: a.temp_c,
                        p: a.pressure_bar,
                        fl: f.flow_rate_ml_s,
                        vol: f.total_volume_ml,
                        st,
                    };
                    if let Ok(json_str) = serde_json_core::to_string::<_, 256>(&data) {
                        let mut resp = heapless::String::<512>::new();
                        let _ = resp.push_str("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n");
                        let _ = resp.push_str(json_str.as_str());
                        let _ = socket.write_all(resp.as_bytes()).await;
                    }
                } else if request.starts_with("GET /api/settings") {
                    let s = crate::settings::SettingsManager::get().await;
                    if let Ok(json_str) = serde_json_core::to_string::<_, 1024>(&s) {
                        let mut resp = heapless::String::<2048>::new();
                        let _ = resp.push_str("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n");
                        let _ = resp.push_str(json_str.as_str());
                        let _ = socket.write_all(resp.as_bytes()).await;
                    }
                } else if request.starts_with("GET /api/profiles") {
                    let p_list = crate::settings::get_all_profiles_from_ram().await;
                    let mut headers: heapless::Vec<ProfileHeader, 10> = heapless::Vec::new();
                    for (slot, p) in p_list.iter() {
                        let _ = headers.push(ProfileHeader {
                            slot: *slot,
                            name: p.name.as_str(),
                        });
                    }

                    let mut resp_buf = [0u8; 2048];
                    let header = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                    resp_buf[..header.len()].copy_from_slice(header);
                    if let Ok(len) =
                        serde_json_core::to_slice(&headers, &mut resp_buf[header.len()..])
                    {
                        let _ = socket.write_all(&resp_buf[..header.len() + len]).await;
                    }
                } else if request.starts_with("GET /api/profile/") {
                    if let Some(s_idx) = request.find("/api/profile/") {
                        let sub = &request[s_idx + 13..];
                        let end = sub.find(' ').unwrap_or(sub.len());
                        if let Ok(slot) = sub[..end].parse::<u8>() {
                            if let Some(p) = crate::settings::get_profile_from_ram(slot).await {
                                let mut resp_buf = [0u8; 2048];
                                let header = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                                resp_buf[..header.len()].copy_from_slice(header);
                                if let Ok(len) =
                                    serde_json_core::to_slice(&p, &mut resp_buf[header.len()..])
                                {
                                    let _ = socket.write_all(&resp_buf[..header.len() + len]).await;
                                }
                            }
                        }
                    }
                } else if request.starts_with("POST /api/cmd") {
                    if let Some(body_start) = request.find("\r\n\r\n") {
                        let json_body = &request[(body_start + 4)..];
                        defmt::info!("API POST Body: {}", json_body);
                        if let Ok((payload, _)) = serde_json_core::from_str::<ApiCommand>(json_body)
                        {
                            defmt::info!("API Command Received: {}", payload.cmd);
                            match payload.cmd {
                                "brew" => SIG_COMMAND.signal(MachineCommand::Brew),
                                "stop" => SIG_COMMAND.signal(MachineCommand::Stop),
                                "steam" => SIG_COMMAND.signal(MachineCommand::Steam),
                                "flush" => SIG_COMMAND.signal(MachineCommand::Flush),
                                "descale" => SIG_COMMAND.signal(MachineCommand::Descale),
                                "direct_pump" => {
                                    SIG_COMMAND.signal(MachineCommand::Stop);
                                    if let Some(p) = payload.power {
                                        SIG_COMMAND.signal(MachineCommand::DirectPump(p));
                                    }
                                }
                                "save_settings" => {
                                    let mut s = crate::settings::SettingsManager::get().await;
                                    if let Some(m) = payload.machine {
                                        s.machine = m;
                                    }
                                    if let Some(h) = payload.hardware {
                                        s.hardware = h;
                                    }
                                    if let Some(p) = payload.temp_pid {
                                        s.temp_pid = p;
                                    }
                                    if let Some(p) = payload.press_pid {
                                        s.press_pid = p;
                                    }
                                    if let Some(w) = payload.wifi {
                                        defmt::info!("API: New SSID: {}", w.ssid.as_str());
                                        s.wifi = w;
                                    }
                                    SIG_COMMAND.signal(MachineCommand::SaveSettings(s));
                                }
                                "profile" => {
                                    if let Some(p) = payload.profile {
                                        SIG_COMMAND.signal(MachineCommand::RunProfile(p));
                                    }
                                }
                                "run_slot" => {
                                    if let Some(slot) = payload.slot {
                                        if let Some(p) =
                                            crate::settings::get_profile_from_ram(slot).await
                                        {
                                            SIG_COMMAND.signal(MachineCommand::RunProfile(p));
                                        }
                                    }
                                }
                                "save_profile" => {
                                    if let (Some(slot), Some(p)) = (payload.slot, payload.profile) {
                                        crate::settings::save_profile_to_ram(slot, p).await;
                                        SIG_SYSTEM_EVENT.signal(SystemEvent::SaveProfile(slot));
                                    }
                                }
                                "delete_profile" => {
                                    if let Some(slot) = payload.slot {
                                        crate::settings::delete_profile_from_ram(slot).await;
                                        SIG_SYSTEM_EVENT.signal(SystemEvent::DeleteProfile(slot));
                                    }
                                }
                                _ => {
                                    defmt::warn!("API: Unknown command {}", payload.cmd);
                                }
                            }
                        } else {
                            defmt::warn!("API: Failed to parse JSON body");
                        }
                    }
                    let _ = socket
                        .write_all("HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ok\"}".as_bytes())
                        .await;
                }
            }
        }
        let _ = socket.flush().await;
        let _ = socket.close();
        let _ = embassy_time::with_timeout(Duration::from_millis(50), async {
            let mut trash = [0u8; 16];
            loop {
                if let Ok(0) | Err(_) = socket.read(&mut trash).await {
                    break;
                }
            }
        })
        .await;
        socket.abort();
    }
}

#[embassy_executor::task]
pub async fn dhcp_server_task(stack: &'static embassy_net::Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0u8; 1024];
    let mut buf = [0u8; 1024];

    let mut socket = UdpSocket::new(
        *stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    let _ = socket.bind(67);

    loop {
        if let Ok((n, _remote)) = socket.recv_from(&mut buf).await {
            if n < 240 {
                continue;
            }
            if &buf[236..240] != &[0x63, 0x82, 0x53, 0x63] {
                continue;
            }

            let xid = &buf[4..8];
            let chaddr = &buf[28..44];
            let mut msg_type = 0;

            let mut opt_ptr = 240;
            while opt_ptr < n - 2 {
                let code = buf[opt_ptr];
                if code == 255 {
                    break;
                }
                let len = buf[opt_ptr + 1] as usize;
                if code == 53 && len == 1 {
                    msg_type = buf[opt_ptr + 2];
                }
                opt_ptr += 2 + len;
            }

            if msg_type == 1 || msg_type == 3 {
                let mut reply = [0u8; 300];
                reply[0] = 2;
                reply[1] = 1;
                reply[2] = 6;
                reply[4..8].copy_from_slice(xid);
                reply[16..20].copy_from_slice(&[192, 168, 4, 2]);
                reply[20..24].copy_from_slice(&[192, 168, 4, 1]);
                reply[28..44].copy_from_slice(chaddr);
                reply[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);

                let next_type = if msg_type == 1 { 2 } else { 5 };
                reply[240..243].copy_from_slice(&[53, 1, next_type]);
                reply[243..249].copy_from_slice(&[54, 4, 192, 168, 4, 1]);
                reply[249..255].copy_from_slice(&[51, 4, 0, 0, 14, 16]);
                reply[255..261].copy_from_slice(&[1, 4, 255, 255, 255, 0]);
                reply[261..267].copy_from_slice(&[3, 4, 192, 168, 4, 1]);
                reply[267] = 255;

                let _ = socket
                    .send_to(
                        &reply,
                        embassy_net::IpEndpoint::new(
                            embassy_net::IpAddress::v4(255, 255, 255, 255),
                            68,
                        ),
                    )
                    .await;
            }
        }
    }
}

#[embassy_executor::task]
pub async fn wifi_driver_task(
    runner: cyw43::Runner<'static, Output<'static>, cyw43_pio::PioSpi<'static, PIO1, 0, DMA_CH0>>,
) {
    runner.run().await
}

#[embassy_executor::task]
pub async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) {
    runner.run().await
}

pub async fn setup_wifi(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0, DMA_CH0>,
) {
    defmt::info!("Wifi: setup_wifi started");
    // Firmware moved to reserved flash addresses to reduce binary size
    let fw = unsafe { core::slice::from_raw_parts(0x101B0000 as *const u8, 231077) };
    let clm = unsafe { core::slice::from_raw_parts(0x101EF000 as *const u8, 984) };

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    defmt::info!("Wifi: calling cyw43::new");
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;

    spawner.spawn(wifi_driver_task(runner)).unwrap();
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
    spawner.spawn(net_task(runner_alloc)).unwrap();

    spawner.spawn(wifi_server_task(stack)).unwrap();

    let mut cold_start = true;

    loop {
        let settings = SettingsManager::get().await;
        let mut success = false;

        if !settings.wifi.ssid.is_empty() {
            defmt::info!(
                "Wi-Fi: Attempting to connect to SSID: {}",
                settings.wifi.ssid.as_str()
            );

            for i in 0..10 {
                match control
                    .join(
                        settings.wifi.ssid.as_str(),
                        cyw43::JoinOptions::new(settings.wifi.password.as_bytes()),
                    )
                    .await
                {
                    Ok(_) => {
                        defmt::info!("Wi-Fi: Successfully joined network, waiting for IP...");
                        // Wait for DHCP
                        for _ in 0..200 {
                            // 20 seconds timeout
                            if let Some(config) = stack.config_v4() {
                                if !config.address.address().is_unspecified() {
                                    defmt::info!(
                                        "Wi-Fi: Connected! IP: {}",
                                        config.address.address()
                                    );
                                    success = true;
                                    break;
                                }
                            }
                            Timer::after(Duration::from_millis(100)).await;
                        }
                        if success {
                            break;
                        } else {
                            defmt::warn!("Wi-Fi: DHCP timeout (no IP assigned)");
                            let _ = control.leave().await;
                        }
                    }
                    Err(_e) => {
                        defmt::warn!("Wi-Fi: Join failed (attempt {}/10)", i + 1);
                        Timer::after(Duration::from_secs(2)).await;
                    }
                }
            }
        } else {
            defmt::info!("Wi-Fi: No SSID configured.");
        }

        if !success && cold_start {
            defmt::info!("Wi-Fi: Cold start failed. Entering AP mode 'Oximite-Setup'...");
            stack.set_config_v4(embassy_net::ConfigV4::Static(embassy_net::StaticConfigV4 {
                address: embassy_net::Ipv4Cidr::new(
                    embassy_net::Ipv4Address::new(192, 168, 4, 1),
                    24,
                ),
                gateway: None,
                dns_servers: Default::default(),
            }));

            let _ = spawner.spawn(dhcp_server_task(stack));
            control.start_ap_wpa2("Oximite-Setup", "password", 6).await;
            defmt::info!("Wi-Fi: AP Mode active at 192.168.4.1. Waiting for reconfiguration...");

            loop {
                if let Some(()) = SIG_WIFI_RECONFIG.try_take() {
                    break;
                }
                Timer::after(Duration::from_millis(500)).await;
            }

            defmt::info!("Wi-Fi: Reconfiguration signal received. Rebooting to apply changes...");
            Timer::after(Duration::from_secs(2)).await; // Ensure settings are saved to flash
            cortex_m::peripheral::SCB::sys_reset();
        } else if !success {
            defmt::info!("Wi-Fi: Reconnection failed. Retrying in 10s...");
            for _ in 0..20 {
                if let Some(()) = SIG_WIFI_RECONFIG.try_take() {
                    defmt::info!("Wi-Fi: Reconfiguration requested. Rebooting...");
                    Timer::after(Duration::from_secs(2)).await;
                    cortex_m::peripheral::SCB::sys_reset();
                }
                Timer::after(Duration::from_millis(500)).await;
            }
        } else {
            cold_start = false;
            defmt::info!("Wi-Fi: Connected and Stable.");
            loop {
                if let Some(()) = SIG_WIFI_RECONFIG.try_take() {
                    defmt::info!("Wi-Fi: Reconfiguration requested. Rebooting...");
                    Timer::after(Duration::from_secs(2)).await;
                    cortex_m::peripheral::SCB::sys_reset();
                }

                if !stack.is_link_up() {
                    defmt::warn!("Wi-Fi: Link down! Reconnecting...");
                    break;
                }
                Timer::after(Duration::from_millis(500)).await;
            }
        }
    }
}
