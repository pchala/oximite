//! CYW43 radio bring-up and network supervision on Core 1.

use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either};
use embassy_net::icmp::{
    ChecksumCapabilities, IcmpEndpoint, IcmpSocket, Icmpv4Message, Icmpv4Packet, Icmpv4Repr,
    PacketMetadata,
};
use embassy_net::{IpAddress, Ipv4Address};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::PIO1;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use static_cell::StaticCell;

use crate::backoff::{default_backoff_ms, GatewayProbeTracker, GATEWAY_PROBE_INTERVAL_S};
use crate::board::{CYW43_CLM_ADDR, CYW43_CLM_LEN, CYW43_FW_ADDR, CYW43_FW_LEN};
use crate::bus;
use crate::dhcp_task::dhcp_server_task;
use crate::diag_task::tcp_telemetry_task;
use crate::http_task::wifi_server_task;
use oximite_proto::model::WifiSettings;

const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const CONFIG_UP_TIMEOUT: Duration = Duration::from_secs(30);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);
const PING_TIMEOUT: Duration = Duration::from_secs(2);

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

async fn leave(control: &mut cyw43::Control<'static>) {
    if with_timeout(LEAVE_TIMEOUT, control.leave()).await.is_err() {
        defmt::warn!("Wi-Fi: leave timed out");
    }
}

async fn wait_wifi_flag_down(stack: &embassy_net::Stack<'static>) {
    let mut saw_up = bus::is_wifi_up();
    if let Some(mut rx) = bus::WIFI_UP_WATCH.receiver() {
        loop {
            let up = rx.changed().await;
            if up {
                saw_up = true;
            } else if saw_up || !stack.is_link_up() || !stack.is_config_up() {
                break;
            }
        }
    } else {
        while bus::is_wifi_up() || (stack.is_link_up() && stack.is_config_up()) {
            Timer::after(Duration::from_millis(500)).await;
        }
    }
}

/// Waits until either the Wi-Fi link, DHCP configuration, or supervisor state
/// indicates the active network session is down. In AP mode `cyw43` leaves
/// `LinkState::Down`, so only IP config loss is monitored.
pub async fn wait_network_down(stack: &embassy_net::Stack<'static>) {
    if bus::is_ap_mode() {
        stack.wait_config_down().await;
        return;
    }
    if !stack.is_link_up() || !stack.is_config_up() {
        return;
    }
    select3(
        stack.wait_link_down(),
        stack.wait_config_down(),
        wait_wifi_flag_down(stack),
    )
    .await;
}

async fn ping_once(
    stack: &embassy_net::Stack<'static>,
    target: Ipv4Address,
    ident: u16,
    seq_no: u16,
) -> bool {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 64];
    let mut tx_buf = [0u8; 64];
    let mut socket = IcmpSocket::new(*stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if socket.bind(IcmpEndpoint::Ident(ident)).is_err() {
        return false;
    }

    let repr = Icmpv4Repr::EchoRequest {
        ident,
        seq_no,
        data: b"oxim",
    };
    let mut pkt = [0u8; 12];
    {
        let mut frame = Icmpv4Packet::new_unchecked(&mut pkt);
        repr.emit(&mut frame, &ChecksumCapabilities::default());
    }

    if !matches!(
        with_timeout(Duration::from_millis(500), socket.send_to(&pkt, target)).await,
        Ok(Ok(()))
    ) {
        return false;
    }

    let deadline = Instant::now() + PING_TIMEOUT;
    let mut in_buf = [0u8; 64];
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        match with_timeout(remaining, socket.recv_from(&mut in_buf)).await {
            Ok(Ok((n, from))) => {
                if from != IpAddress::Ipv4(target) {
                    continue;
                }
                if let Ok(reply) = Icmpv4Packet::new_checked(&in_buf[..n]) {
                    if reply.msg_type() == Icmpv4Message::EchoReply
                        && reply.echo_ident() == ident
                        && reply.echo_seq_no() == seq_no
                    {
                        return true;
                    }
                }
            }
            _ => return false,
        }
    }
}

async fn ping_gateway(
    stack: &embassy_net::Stack<'static>,
    target: Ipv4Address,
    ident: u16,
    seq_no: &mut u16,
) -> bool {
    for attempt in 0..2 {
        let seq = *seq_no;
        *seq_no = seq_no.wrapping_add(1);
        if ping_once(stack, target, ident, seq).await {
            return true;
        }
        if attempt == 0 {
            Timer::after(Duration::from_millis(500)).await;
        }
    }
    false
}

async fn monitor_session(stack: &embassy_net::Stack<'static>, seen_wifi_rev: u32, ident: u16) {
    let mut tracker = GatewayProbeTracker::new();
    let mut seq_no: u16 = 1;
    let mut next_probe_at = Instant::now() + Duration::from_secs(2);
    let mut initial_done = false;

    loop {
        Timer::after(Duration::from_millis(500)).await;
        if !stack.is_link_up() || !stack.is_config_up() || bus::get_wifi_rev() != seen_wifi_rev {
            break;
        }
        if Instant::now() < next_probe_at {
            continue;
        }
        next_probe_at = Instant::now() + Duration::from_secs(u64::from(GATEWAY_PROBE_INTERVAL_S));
        let now_s = Instant::now().as_secs() as u32;
        if initial_done
            && GatewayProbeTracker::is_recent_activity(now_s, bus::last_net_activity_s())
        {
            tracker.on_lan_activity();
            continue;
        }
        if let Some(gw) = stack.config_v4().and_then(|c| c.gateway) {
            if ping_gateway(stack, gw, ident, &mut seq_no).await {
                if !tracker.is_armed() {
                    defmt::info!("Wi-Fi: gateway ICMP probe armed");
                }
                tracker.on_probe_ok();
            } else {
                if !initial_done && !tracker.is_armed() {
                    defmt::info!(
                        "Wi-Fi: gateway did not reply to initial ICMP; silent-AP probe unarmed"
                    );
                }
                if tracker.on_probe_timeout() {
                    defmt::warn!("Wi-Fi: gateway unreachable, forcing rejoin");
                    break;
                }
            }
        }
        initial_done = true;
    }
}

async fn wait_disconnect_or_rejoin(
    stack: &embassy_net::Stack<'static>,
    seen_wifi_rev: u32,
    ident: u16,
) {
    select3(
        stack.wait_link_down(),
        stack.wait_config_down(),
        monitor_session(stack, seen_wifi_rev, ident),
    )
    .await;
}

/// Outcome of one join attempt in client mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinOutcome {
    Connected,
    JoinFailed,
    NoIp,
}

/// Returns the radio firmware and CLM blobs stored in flash.
///
/// # Safety
/// `CYW43_FW_ADDR` and `CYW43_CLM_ADDR` must point to readable flash that holds
/// blobs of `CYW43_FW_LEN` and `CYW43_CLM_LEN` bytes. The firmware blob must be
/// 4-byte aligned.
unsafe fn radio_blobs() -> (&'static cyw43::Aligned<cyw43::A4, [u8]>, &'static [u8]) {
    let fw = core::slice::from_raw_parts(CYW43_FW_ADDR as *const u8, CYW43_FW_LEN);
    let clm = core::slice::from_raw_parts(CYW43_CLM_ADDR as *const u8, CYW43_CLM_LEN);
    let fw = &*(fw as *const [u8] as *const cyw43::Aligned<cyw43::A4, [u8]>);
    (fw, clm)
}

async fn bring_up_radio(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0>,
) -> (cyw43::NetDriver<'static>, cyw43::Control<'static>) {
    // SAFETY: the addresses and lengths come from `board.rs` and describe the
    // blobs that the flash layout places there.
    let (fw, clm) = unsafe { radio_blobs() };

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) =
        cyw43::new(state, pwr, spi, fw, crate::cyw43_nvram::NVRAM).await;

    spawner.spawn(wifi_driver_task(runner).unwrap());
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;
    (net_device, control)
}

fn start_network_stack(
    spawner: Spawner,
    net_device: cyw43::NetDriver<'static>,
    net_seed: u64,
) -> &'static embassy_net::Stack<'static> {
    static STACK: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<embassy_net::StackResources<8>> = StaticCell::new();
    let mut dhcp_config = embassy_net::DhcpConfig::default();
    dhcp_config.hostname = Some("oximite".try_into().unwrap());

    let (stack_alloc, runner_alloc) = embassy_net::new(
        net_device,
        embassy_net::Config::dhcpv4(dhcp_config),
        RESOURCES.init(embassy_net::StackResources::<8>::new()),
        net_seed,
    );
    let stack = STACK.init(stack_alloc);
    spawner.spawn(net_task(runner_alloc).unwrap());
    stack
}

fn spawn_network_services(spawner: Spawner, stack: &'static embassy_net::Stack<'static>) {
    spawner.spawn(wifi_server_task(stack).unwrap());
    spawner.spawn(wifi_server_task(stack).unwrap());
    spawner.spawn(tcp_telemetry_task(stack).unwrap());
}

async fn run_ap_mode(
    spawner: Spawner,
    stack: &'static embassy_net::Stack<'static>,
    mut control: cyw43::Control<'static>,
) -> ! {
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
    bus::set_wifi_up(true);
    loop {
        core::future::pending::<()>().await;
    }
}

async fn try_join(
    control: &mut cyw43::Control<'static>,
    stack: &embassy_net::Stack<'static>,
    wifi: &WifiSettings,
) -> JoinOutcome {
    defmt::info!("Wi-Fi: Joining SSID: {}", wifi.ssid.as_str());
    let joined = matches!(
        with_timeout(
            JOIN_TIMEOUT,
            control.join(
                wifi.ssid.as_str(),
                cyw43::JoinOptions::new(wifi.password.as_bytes()),
            )
        )
        .await,
        Ok(Ok(()))
    );
    if !joined {
        return JoinOutcome::JoinFailed;
    }

    let config_up = async {
        stack.wait_link_up().await;
        matches!(
            select(stack.wait_config_up(), stack.wait_link_down()).await,
            Either::First(())
        )
    };
    let has_ip = with_timeout(CONFIG_UP_TIMEOUT, config_up)
        .await
        .unwrap_or(false);
    if has_ip {
        JoinOutcome::Connected
    } else {
        JoinOutcome::NoIp
    }
}

async fn run_client_mode(
    stack: &'static embassy_net::Stack<'static>,
    mut control: cyw43::Control<'static>,
    net_seed: u64,
) -> ! {
    defmt::info!("Wi-Fi: Booting in Client Mode");
    bus::set_wifi_up(false);
    let probe_ident = (net_seed as u16) | 1;
    let mut attempt: u32 = 0;

    loop {
        let (wifi, seen_rev) = bus::get_wifi_settings();
        if wifi.ssid.is_empty() {
            Timer::after(Duration::from_secs(1)).await;
            continue;
        }

        match try_join(&mut control, stack, &wifi).await {
            JoinOutcome::Connected => {
                defmt::info!("Wi-Fi: Connected to SSID");
                attempt = 0;
                bus::set_wifi_up(true);
                wait_disconnect_or_rejoin(stack, seen_rev, probe_ident).await;
                bus::set_wifi_up(false);
                defmt::warn!(
                    "Wi-Fi: Link/config lost, gateway unreachable, or credentials updated"
                );
            }
            JoinOutcome::NoIp => defmt::warn!("Wi-Fi: DHCP/config timeout"),
            JoinOutcome::JoinFailed => defmt::warn!("Wi-Fi: Join failed"),
        }

        // Tear down any stale association so the next join never panics on IOCTL error -5.
        leave(&mut control).await;
        attempt = attempt.saturating_add(1);
        let delay_ms = default_backoff_ms(attempt, Instant::now().as_ticks());
        Timer::after(Duration::from_millis(delay_ms)).await;
    }
}

#[embassy_executor::task]
pub async fn wifi_init_task(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: cyw43_pio::PioSpi<'static, PIO1, 0>,
    force_ap: bool,
    net_seed: u64,
) {
    defmt::info!("Wifi: init task started");
    let (net_device, control) = bring_up_radio(spawner, pwr, spi).await;
    let stack = start_network_stack(spawner, net_device, net_seed);

    let (initial_wifi, _) = bus::get_wifi_settings();
    let is_ap = force_ap || initial_wifi.ssid.is_empty();
    bus::set_ap_mode(is_ap);

    spawn_network_services(spawner, stack);

    if is_ap {
        run_ap_mode(spawner, stack, control).await
    } else {
        run_client_mode(stack, control, net_seed).await
    }
}
