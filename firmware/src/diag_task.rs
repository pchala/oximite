//! Port 8080 one-way telemetry socket task.
//!
//! Sends length-prefixed Postcard [`DeviceMsg`] frames. The task ignores all
//! bytes that the peer sends. Commands and queries use HTTP on Port 80.

use embassy_futures::select::{select3, Either3};
use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration, Timer};
use embedded_io_async::Write;
use oximite_proto::protocol::{encode_device_msg, DeviceMsg, WireTick, MAX_FRAME_LEN};

use crate::bus;
use crate::tcp::graceful_close;

async fn write_msg(socket: &mut TcpSocket<'_>, msg: &DeviceMsg) -> bool {
    let mut buf = [0u8; MAX_FRAME_LEN + 2];
    let Some(n) = encode_device_msg(msg, &mut buf) else {
        defmt::error!("Diag: failed to encode DeviceMsg");
        return true;
    };
    matches!(
        with_timeout(Duration::from_millis(250), socket.write_all(&buf[..n])).await,
        Ok(Ok(()))
    )
}

async fn flush_events(socket: &mut TcpSocket<'_>) -> bool {
    while let Some(ev) = bus::try_pop_event() {
        if !write_msg(socket, &DeviceMsg::Event(ev)).await {
            return false;
        }
    }
    true
}

#[embassy_executor::task]
pub async fn tcp_telemetry_task(stack: &'static embassy_net::Stack<'static>) {
    let mut rx_buffer = [0; 64];
    let mut tx_buffer = [0; 1024];
    let mut telemetry_rx = defmt::unwrap!(bus::TELEMETRY_WATCH.receiver());

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_keep_alive(Some(Duration::from_secs(15)));
        socket.set_timeout(Some(Duration::from_secs(45)));
        if socket.accept(8080).await.is_err() {
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        defmt::info!(
            "TCP Telemetry: Accepted connection from {}",
            socket.remote_endpoint()
        );

        bus::clear_events();

        let mut network_lost = false;

        loop {
            if !flush_events(&mut socket).await {
                break;
            }

            let mut discard = [0u8; 64];
            let read_fut = socket.read(&mut discard);
            let tick_fut = telemetry_rx.changed();
            let net_down_fut = crate::wifi_task::wait_network_down(stack);

            match select3(tick_fut, read_fut, net_down_fut).await {
                Either3::First(t) => {
                    if !flush_events(&mut socket).await {
                        break;
                    }
                    let tick = WireTick::from_telemetry(
                        &t,
                        bus::get_session_brew_temp(),
                        bus::get_state() as u8,
                        bus::get_control_intent().step_idx,
                    );
                    if !write_msg(&mut socket, &DeviceMsg::Tick(tick)).await {
                        break;
                    }
                }
                Either3::Second(Ok(0) | Err(_)) => break,
                Either3::Second(Ok(_)) => {}
                Either3::Third(()) => {
                    defmt::warn!("TCP Telemetry: Network down, aborting socket");
                    network_lost = true;
                    break;
                }
            }
        }

        if network_lost {
            socket.abort();
        } else {
            defmt::info!("TCP Telemetry: Connection closed");
            graceful_close(&mut socket).await;
        }
    }
}
