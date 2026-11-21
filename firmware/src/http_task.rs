//! HTTP web server task on TCP Port 80 driving pure `http_codec`.

use core::str::from_utf8;
use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::Write;
use oximite_proto::model::{MachineCommand, MachineState, MAX_PROFILES};
use oximite_proto::protocol::{CommandAck, ParsedCommand, UiTelemetry};
use serde::Serialize;

use crate::bus;
use crate::http_codec::{
    check_request_complete, encode_json_response, parse_http_request, HttpError, HttpRoute,
    ProfileHeader, JSON_RESP_MAX,
};

static INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

async fn read_request(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, HttpError> {
    let mut n = 0;
    loop {
        if let Some(total) = check_request_complete(&buf[..n], buf.len())? {
            return Ok(total);
        }
        match socket.read(&mut buf[n..]).await {
            Ok(0) | Err(_) => return Err(HttpError::Closed),
            Ok(r) => n += r,
        }
    }
}

async fn graceful_close(socket: &mut TcpSocket<'_>) {
    let _ = socket.flush().await;
    socket.close();
    let _ = with_timeout(Duration::from_millis(50), async {
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

async fn write_json<T: Serialize>(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; JSON_RESP_MAX],
    value: &T,
) {
    match encode_json_response(buf, value) {
        Some(len) => {
            let _ = socket.write_all(&buf[..len]).await;
        }
        None => defmt::warn!("JSON response exceeded its buffer; nothing sent"),
    }
}

async fn handle_connection(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; 4096],
) -> Result<(), HttpError> {
    let n = read_request(socket, buf).await?;
    bus::note_net_activity(Instant::now().as_secs() as u32);
    let request = from_utf8(&buf[..n]).map_err(|_| HttpError::BadRequest)?;
    let route = parse_http_request(request)?;
    let mut resp_buf = [0u8; JSON_RESP_MAX];

    match route {
        HttpRoute::Index => {
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(INDEX_HTML_GZ).await;
        }
        HttpRoute::GetTelemetry => {
            let snap = bus::get_settings_snapshot();
            let telem = bus::get_telemetry();
            let state = bus::get_state();
            let payload = UiTelemetry::from_snapshot(
                &telem,
                snap.control.machine.temp_offset,
                state == MachineState::Steaming,
                bus::get_session_brew_temp(),
                state as u8,
                bus::get_ack(),
            );
            write_json(socket, &mut resp_buf, &payload).await;
        }
        HttpRoute::GetSettings => {
            let snap = bus::get_settings_snapshot();
            write_json(socket, &mut resp_buf, &snap.public).await;
        }
        HttpRoute::GetProfiles => {
            let p_list = bus::get_profile_headers();
            let mut headers: heapless::Vec<ProfileHeader, { MAX_PROFILES as usize }> =
                heapless::Vec::new();
            for item in &p_list {
                let _ = headers.push(ProfileHeader {
                    slot: item.slot,
                    name: item.name.as_str(),
                });
            }
            write_json(socket, &mut resp_buf, &headers).await;
        }
        HttpRoute::GetProfile(slot) => {
            let p = bus::get_profile(slot).ok_or(HttpError::NotFound)?;
            write_json(socket, &mut resp_buf, &p).await;
        }
        HttpRoute::PostCmd(parsed) => {
            let cmd = match parsed {
                ParsedCommand::Machine(c) => c,
                ParsedCommand::RunSlot(slot) => {
                    let p = bus::get_profile(slot).ok_or(HttpError::NotFound)?;
                    MachineCommand::RunProfile(p)
                }
                _ => return Err(HttpError::BadRequest),
            };
            defmt::info!("API Command Received: {}", cmd.name());
            let ticket = bus::send_command(cmd, None, false).ok_or(HttpError::Busy)?;
            write_json(
                socket,
                &mut resp_buf,
                &CommandAck {
                    status: "ok",
                    ack: ticket,
                },
            )
            .await;
        }
    }

    Ok(())
}

#[embassy_executor::task(pool_size = 2)]
pub async fn wifi_server_task(stack: &'static embassy_net::Stack<'static>) {
    let mut rx_buffer = [0; 2048];
    let mut tx_buffer = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(5)));

        if socket.accept(80).await.is_err() {
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        let mut buf = [0u8; 4096];
        let conn = with_timeout(
            Duration::from_secs(10),
            handle_connection(&mut socket, &mut buf),
        );
        match select(conn, crate::wifi_task::wait_network_down(stack)).await {
            Either::First(served) => {
                if let Err(e) = served.unwrap_or(Err(HttpError::Closed)) {
                    if let Some(status) = e.status_line() {
                        let _ = socket.write_all(status.as_bytes()).await;
                    }
                }
                graceful_close(&mut socket).await;
            }
            Either::Second(()) => {
                socket.abort();
            }
        }
    }
}
