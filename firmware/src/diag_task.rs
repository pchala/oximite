//! Port 8080 diagnostic telemetry & control socket task.
//!
//! Downlink sends length-prefixed Postcard v3 [`DeviceMsg`] frames.
//! Uplink accepts newline-delimited JSON commands compatible with `POST /api/cmd`.

use core::str::from_utf8;
use embassy_futures::select::{select3, Either3};
use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::Write;
use oximite_proto::model::MachineCommand;
use oximite_proto::protocol::{
    encode_device_msg, parse_api_command_struct, ApiCommand, CmdError, DeviceHello, DeviceMsg,
    LineEvent, LineSplitter, ParsedCommand, TelemetryFrame, DIAG_MAGIC, DIAG_PROTO_VER,
    MAX_FRAME_LEN,
};

use crate::board::FW_VERSION;
use crate::bus;

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

async fn flush_priority(
    socket: &mut TcpSocket<'_>,
    seen_settings_rev: &mut u32,
    seen_profiles_rev: &mut u32,
) -> bool {
    while let Some((req_id, ticket)) = bus::try_pop_diag_reply() {
        let msg = DeviceMsg::CommandReply {
            req_id,
            ticket: Some(ticket),
            err: None,
        };
        if !write_msg(socket, &msg).await {
            return false;
        }
    }

    while let Some(ev) = bus::try_pop_event() {
        if !write_msg(socket, &DeviceMsg::Event(ev)).await {
            return false;
        }
    }

    let snap = bus::get_settings_snapshot();
    if snap.settings_rev != *seen_settings_rev {
        *seen_settings_rev = snap.settings_rev;
        if !write_msg(socket, &DeviceMsg::SettingsSnapshot(snap.public)).await {
            return false;
        }
    }

    let prof_rev = bus::get_profiles_rev();
    if prof_rev != *seen_profiles_rev {
        *seen_profiles_rev = prof_rev;
        let list = bus::get_profile_headers();
        if !write_msg(socket, &DeviceMsg::ProfilesList(list)).await {
            return false;
        }
    }

    true
}

async fn handle_uplink_line(
    socket: &mut TcpSocket<'_>,
    line: &[u8],
    stream_enabled: &mut bool,
    stream_divider: &mut u8,
) -> bool {
    let Ok(json) = from_utf8(line) else {
        defmt::warn!("Diag: non-UTF8 uplink line ignored");
        return true;
    };

    let Ok((payload, _)) = serde_json_core::from_str::<ApiCommand>(json) else {
        defmt::warn!("Diag: malformed JSON command");
        return true;
    };

    let req_id = payload.req_id;
    let parsed = match parse_api_command_struct(payload) {
        Ok((_, p)) => p,
        Err(err) => {
            if let Some(id) = req_id {
                return write_msg(
                    socket,
                    &DeviceMsg::CommandReply {
                        req_id: id,
                        ticket: None,
                        err: Some(err),
                    },
                )
                .await;
            }
            return true;
        }
    };

    match parsed {
        ParsedCommand::Ping => {
            if let Some(id) = req_id {
                return write_msg(
                    socket,
                    &DeviceMsg::Pong {
                        req_id: id,
                        uptime_ms: Instant::now().as_millis() as u32,
                    },
                )
                .await;
            }
            true
        }
        ParsedCommand::GetSettings => {
            let snap = bus::get_settings_snapshot();
            write_msg(socket, &DeviceMsg::SettingsSnapshot(snap.public)).await
        }
        ParsedCommand::GetProfiles => {
            let list = bus::get_profile_headers();
            write_msg(socket, &DeviceMsg::ProfilesList(list)).await
        }
        ParsedCommand::GetProfile(slot) => {
            let profile = bus::get_profile(slot);
            write_msg(socket, &DeviceMsg::ProfileData { slot, profile }).await
        }
        ParsedCommand::SetStream { enabled, divider } => {
            *stream_enabled = enabled;
            *stream_divider = divider.max(1);
            if let Some(id) = req_id {
                return write_msg(
                    socket,
                    &DeviceMsg::CommandReply {
                        req_id: id,
                        ticket: None,
                        err: None,
                    },
                )
                .await;
            }
            true
        }
        ParsedCommand::RunSlot(slot) => match bus::get_profile(slot) {
            Some(p) => queue_machine_cmd(socket, req_id, MachineCommand::RunProfile(p)).await,
            None => reply_if_req(socket, req_id, CmdError::NotFound).await,
        },
        ParsedCommand::Machine(cmd) => queue_machine_cmd(socket, req_id, cmd).await,
    }
}

async fn reply_if_req(socket: &mut TcpSocket<'_>, req_id: Option<u32>, err: CmdError) -> bool {
    if let Some(id) = req_id {
        write_msg(
            socket,
            &DeviceMsg::CommandReply {
                req_id: id,
                ticket: None,
                err: Some(err),
            },
        )
        .await
    } else {
        true
    }
}

async fn queue_machine_cmd(
    socket: &mut TcpSocket<'_>,
    req_id: Option<u32>,
    cmd: MachineCommand,
) -> bool {
    if bus::send_command(cmd, req_id, req_id.is_some()).is_none() {
        return reply_if_req(socket, req_id, CmdError::Busy).await;
    }
    true
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

#[embassy_executor::task]
pub async fn tcp_telemetry_task(stack: &'static embassy_net::Stack<'static>) {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 2048];
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

        bus::clear_diag_queues();
        let snap = bus::get_settings_snapshot();
        let mut seen_settings_rev = snap.settings_rev;
        let mut seen_profiles_rev = bus::get_profiles_rev();

        let mut fw_version = heapless::String::new();
        let _ = fw_version.push_str(FW_VERSION);

        let hello = DeviceMsg::Hello(DeviceHello {
            magic: DIAG_MAGIC,
            proto_ver: DIAG_PROTO_VER,
            fw_version,
            settings: snap.public,
            profiles: bus::get_profile_headers(),
            state: bus::get_state() as u8,
            sbt: bus::get_session_brew_temp(),
        });

        if !write_msg(&mut socket, &hello).await {
            graceful_close(&mut socket).await;
            continue;
        }

        let mut splitter = LineSplitter::<1024>::new();
        let mut stream_enabled = true;
        let mut stream_divider: u8 = 1;
        let mut div_counter: u32 = 0;
        let mut network_lost = false;

        loop {
            if !flush_priority(&mut socket, &mut seen_settings_rev, &mut seen_profiles_rev).await {
                break;
            }

            let mut read_buf = [0u8; 128];
            let read_fut = socket.read(&mut read_buf);
            let tick_fut = telemetry_rx.changed();
            let net_down_fut = crate::wifi_task::wait_network_down(stack);

            match select3(tick_fut, read_fut, net_down_fut).await {
                Either3::First(t) => {
                    if !flush_priority(&mut socket, &mut seen_settings_rev, &mut seen_profiles_rev)
                        .await
                    {
                        break;
                    }
                    if stream_enabled {
                        div_counter = div_counter.wrapping_add(1);
                        if div_counter.is_multiple_of(u32::from(stream_divider)) {
                            let intent = bus::get_control_intent();
                            let frame = TelemetryFrame {
                                seq: t.tick,
                                ms: Instant::now().as_millis() as u32,
                                t: t.temp_c,
                                tt: t.target_temp,
                                ett: t.effective_target_temp,
                                p: t.pressure_bar,
                                tp: t.target_bar,
                                fl: t.flow_rate_ml_s,
                                tfl: t.target_ml_s,
                                vol: t.volume_ml,
                                hp: t.heater_duty,
                                pump: t.pump_duty,
                                ac_us: t.ac_period_us,
                                sbt: bus::get_session_brew_temp(),
                                fc: t.flow_controlled as u8,
                                st: bus::get_state() as u8,
                                step: intent.step_idx,
                                ack: bus::get_ack(),
                            };
                            if !write_msg(&mut socket, &DeviceMsg::Tick(frame)).await {
                                break;
                            }
                        }
                    }
                }
                Either3::Second(Ok(0) | Err(_)) => break,
                Either3::Second(Ok(n)) => {
                    bus::note_net_activity(Instant::now().as_secs() as u32);
                    let mut ok = true;
                    for &b in &read_buf[..n] {
                        if let Some(ev) = splitter.push_byte(b) {
                            match ev {
                                LineEvent::Line(line) => {
                                    if !handle_uplink_line(
                                        &mut socket,
                                        line,
                                        &mut stream_enabled,
                                        &mut stream_divider,
                                    )
                                    .await
                                    {
                                        ok = false;
                                        break;
                                    }
                                }
                                LineEvent::Overflow => {
                                    defmt::warn!("Diag: uplink line overflow (>1024 bytes)");
                                }
                            }
                        }
                    }
                    if !ok {
                        break;
                    }
                }
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
