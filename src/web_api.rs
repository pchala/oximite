//! HTTP and line-oriented TCP interfaces to the machine.
//!
//! `wifi_server_task` serves the gzipped web UI plus a small JSON REST API on
//! port 80; `tcp_telemetry_task` streams the same telemetry at 50 Hz on port
//! 8080 and accepts the same commands as newline-delimited JSON. Both are pure
//! protocol handling -- every command is forwarded to the coordinator as a
//! `MachineCommand` rather than acted on here.

use core::str::from_utf8;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write;
use serde::{Deserialize, Serialize};

use crate::profiles::BrewProfile;
use crate::settings::{
    FlashUpdate, MachineSettings, PidSettings, Settings, WifiSettings, SIG_FLASH_UPDATE,
};
use crate::state::{
    get_session_brew_temp, get_state, get_telemetry, send_command, MachineCommand, MachineState,
    Telemetry, TELEMETRY_WATCH,
};

static INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

#[derive(Deserialize)]
struct ApiCommand<'a> {
    cmd: &'a str,
    profile: Option<BrewProfile>,
    slot: Option<u8>,
    machine: Option<MachineSettings>,
    temp_pid: Option<PidSettings>,
    press_pid: Option<PidSettings>,
    flow_pid: Option<PidSettings>,
    wifi: Option<WifiSettings>,
    power: Option<f32>,
    temp: Option<f32>,
}

#[derive(Serialize)]
struct TelemetryData {
    /// Monotonic sample counter
    seq: u32,
    /// Device uptime in milliseconds.
    ms: u32,
    t: f32,
    tt: f32,
    /// Applied temperature setpoint including the brew-flow feed-forward.
    ett: f32,
    /// Session brew temp — what the UI's +/- buttons adjust. Deliberately not
    /// derived from `tt`: `tt` is the *applied* setpoint, which is a function
    /// of the machine state (0 °C while sleeping or cooling, steam temp while
    /// steaming), so stepping from it would send nonsense back.
    sbt: f32,
    p: f32,
    tp: f32,
    /// Setpoint the pressure PID chased, after flow limiting.
    etp: f32,
    fl: f32,
    /// Active flow setpoint, 0 when the pump isn't flow-controlled.
    fll: f32,
    /// 1 while the flow PID is driving the pump duty directly.
    fc: u8,
    vol: f32,
    hp: f32,
    /// Pump triac duty, 0-100.
    pump: f32,
    st: u32,
}

#[derive(Serialize)]
struct ProfileHeader<'a> {
    slot: u8,
    name: &'a str,
}

const JSON_OK_HEADER: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";

/// Closes `socket` without stranding unread data in the peer's window.
///
/// `close()` only sends FIN; if the client still has bytes in flight, dropping
/// straight into `abort()` sends an RST that makes some clients discard the
/// response we just wrote. So we flush, half-close, then briefly drain whatever
/// the peer sends until it closes too, and only then abort to reclaim the
/// socket immediately instead of sitting in TIME_WAIT.
async fn graceful_close(socket: &mut TcpSocket<'_>) {
    let _ = socket.flush().await;
    socket.close();
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

async fn handle_api_command(payload: ApiCommand<'_>) {
    match payload.cmd {
        "power" => send_command(MachineCommand::TogglePower),
        "brew" => send_command(MachineCommand::Brew),
        "stop" => send_command(MachineCommand::Stop),
        "steam" => send_command(MachineCommand::Steam),
        "flush" => send_command(MachineCommand::Flush),
        "direct_pump" => {
            if let Some(p) = payload.power {
                send_command(MachineCommand::DirectPump(p));
            }
        }
        "set_session_temp" => {
            if let Some(t) = payload.temp {
                send_command(MachineCommand::SetSessionTemp(t));
            }
        }
        "save_machine" => {
            if let Some(m) = payload.machine {
                send_command(MachineCommand::SaveMachine(m));
            }
        }
        "save_pids" => {
            if let (Some(t), Some(p)) = (payload.temp_pid, payload.press_pid) {
                send_command(MachineCommand::SavePids(t, p, payload.flow_pid));
            }
        }
        "save_wifi" => {
            if let Some(w) = payload.wifi {
                defmt::info!("API: New SSID: {}", w.ssid.as_str());
                send_command(MachineCommand::SaveWifi(w));
            }
        }
        "profile" => {
            if let Some(p) = payload.profile {
                send_command(MachineCommand::RunProfile(p));
            }
        }
        "run_slot" => {
            if let Some(slot) = payload.slot {
                if let Some(p) = crate::profiles::get_profile_from_ram(slot).await {
                    send_command(MachineCommand::RunProfile(p));
                }
            }
        }
        "save_profile" => {
            if let (Some(slot), Some(p)) = (payload.slot, payload.profile) {
                crate::profiles::save_profile_to_ram(slot, p).await;
                SIG_FLASH_UPDATE.signal(FlashUpdate::SaveProfile(slot));
            }
        }
        "delete_profile" => {
            if let Some(slot) = payload.slot {
                crate::profiles::delete_profile_from_ram(slot).await;
                SIG_FLASH_UPDATE.signal(FlashUpdate::DeleteProfile(slot));
            }
        }
        _ => {
            defmt::warn!("API: Unknown command {}", payload.cmd);
        }
    }
}

async fn get_telemetry_json(a: Telemetry) -> heapless::String<384> {
    let st_val = get_state();
    let s = Settings::get().await;

    let (disp_t, disp_tt, disp_ett) =
        a.display_temps(s.machine.temp_offset, st_val == MachineState::Steaming);

    // Sensor resolution is nowhere near f32 precision, and the serializer
    // prints every digit it is given. Rounding to 2 dp roughly halves the
    // payload, which is what makes room for the diagnostic fields above.
    // `f32::round` needs std, so this rounds half away from zero by hand.
    let r2 = |v: f32| {
        let bias = if v < 0.0 { -0.5 } else { 0.5 };
        ((v * 100.0 + bias) as i32) as f32 / 100.0
    };

    let data = TelemetryData {
        seq: a.tick,
        ms: Instant::now().as_millis() as u32,
        t: r2(disp_t),
        tt: r2(disp_tt),
        ett: r2(disp_ett),
        sbt: r2(get_session_brew_temp()),
        p: r2(a.pressure_bar),
        tp: r2(a.target_bar),
        etp: r2(a.effective_target_bar),
        fl: r2(a.flow_rate_ml_s),
        fll: r2(a.flow_limit_ml_s),
        fc: a.flow_controlled as u8,
        vol: r2(a.volume_ml),
        hp: r2(a.heater_duty),
        pump: r2(a.pump_duty),
        st: st_val as u32,
    };

    let mut json_str = heapless::String::<384>::new();
    match serde_json_core::to_string::<_, 384>(&data) {
        Ok(js) => {
            let _ = json_str.push_str(js.as_str());
        }
        // Silently sending nothing would look like a dropped connection, and
        // the cause (one field wider than expected) would be invisible.
        Err(_) => defmt::warn!("Telemetry JSON exceeded buffer; sample dropped"),
    }
    json_str
}

#[embassy_executor::task]
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
        if let Ok(n) = socket.read(&mut buf).await {
            if n > 0 {
                let request = from_utf8(&buf[..n]).unwrap_or("");

                if request.starts_with("GET / ") || request.starts_with("GET /index.html") {
                    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(INDEX_HTML_GZ).await;
                } else if request.starts_with("GET /api/telemetry") {
                    let json_str = get_telemetry_json(get_telemetry()).await;
                    if !json_str.is_empty() {
                        let mut resp = heapless::String::<640>::new();
                        let _ = resp.push_str(JSON_OK_HEADER);
                        let _ = resp.push_str(json_str.as_str());
                        let _ = socket.write_all(resp.as_bytes()).await;
                    }
                } else if request.starts_with("GET /api/settings") {
                    let s = Settings::get().await;
                    if let Ok(json_str) = serde_json_core::to_string::<_, 1024>(&s) {
                        let mut resp = heapless::String::<2048>::new();
                        let _ = resp.push_str(JSON_OK_HEADER);
                        let _ = resp.push_str(json_str.as_str());
                        let _ = socket.write_all(resp.as_bytes()).await;
                    }
                } else if request.starts_with("GET /api/profiles") {
                    let p_list = crate::profiles::get_all_profiles_from_ram().await;
                    let mut headers: heapless::Vec<ProfileHeader, 10> = heapless::Vec::new();
                    for (slot, p) in p_list.iter() {
                        let _ = headers.push(ProfileHeader {
                            slot: *slot,
                            name: p.name.as_str(),
                        });
                    }

                    let mut resp_buf = [0u8; 2048];
                    let hdr = JSON_OK_HEADER.as_bytes();
                    resp_buf[..hdr.len()].copy_from_slice(hdr);
                    if let Ok(len) = serde_json_core::to_slice(&headers, &mut resp_buf[hdr.len()..])
                    {
                        let _ = socket.write_all(&resp_buf[..hdr.len() + len]).await;
                    }
                } else if request.starts_with("GET /api/profile/") {
                    if let Some(s_idx) = request.find("/api/profile/") {
                        let sub = &request[s_idx + "/api/profile/".len()..];
                        let end = sub.find(' ').unwrap_or(sub.len());
                        if let Ok(slot) = sub[..end].parse::<u8>() {
                            if let Some(p) = crate::profiles::get_profile_from_ram(slot).await {
                                let mut resp_buf = [0u8; 2048];
                                let hdr = JSON_OK_HEADER.as_bytes();
                                resp_buf[..hdr.len()].copy_from_slice(hdr);
                                if let Ok(len) =
                                    serde_json_core::to_slice(&p, &mut resp_buf[hdr.len()..])
                                {
                                    let _ = socket.write_all(&resp_buf[..hdr.len() + len]).await;
                                }
                            }
                        }
                    }
                } else if request.starts_with("POST /api/cmd") {
                    if let Some(body_start) = request.find("\r\n\r\n") {
                        let json_body = &request[(body_start + 4)..];
                        if let Ok((payload, _)) = serde_json_core::from_str::<ApiCommand>(json_body)
                        {
                            defmt::info!("API Command Received: {}", payload.cmd);
                            handle_api_command(payload).await;
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
        graceful_close(&mut socket).await;
    }
}

#[embassy_executor::task]
pub async fn tcp_telemetry_task(stack: &'static embassy_net::Stack<'static>) {
    use embassy_futures::select::{select, Either};

    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    // Taken once, outside the accept loop: the watch has a fixed number of
    // receiver slots and re-taking one per connection would exhaust them.
    let mut telemetry_rx = defmt::unwrap!(TELEMETRY_WATCH.receiver());

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);

        if socket.accept(8080).await.is_err() {
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        defmt::info!(
            "TCP Telemetry: Accepted connection from {}",
            socket.remote_endpoint()
        );

        let mut line_buf = [0u8; 1024];
        let mut line_pos = 0;

        loop {
            let mut read_buf = [0u8; 128];
            let read_fut = socket.read(&mut read_buf);
            let tick_fut = telemetry_rx.changed();

            match select(tick_fut, read_fut).await {
                Either::First(a) => {
                    let mut json_str = get_telemetry_json(a).await;
                    if !json_str.is_empty() {
                        let _ = json_str.push_str("\n");
                        if socket.write_all(json_str.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
                Either::Second(r) => {
                    match r {
                        Ok(0) => break, // Connection closed
                        Ok(n) => {
                            for &b in read_buf.iter().take(n) {
                                if b == b'\n' {
                                    if line_pos > 0 {
                                        if let Ok(json_str) = from_utf8(&line_buf[..line_pos]) {
                                            if let Ok((payload, _)) =
                                                serde_json_core::from_str::<ApiCommand>(json_str)
                                            {
                                                handle_api_command(payload).await;
                                            }
                                        }
                                        line_pos = 0;
                                    }
                                } else if b != 0 && line_pos < line_buf.len() {
                                    line_buf[line_pos] = b;
                                    line_pos += 1;
                                }
                            }
                        }
                        Err(_) => break, // Error reading
                    }
                }
            }
        }

        defmt::info!("TCP Telemetry: Connection closed");
        graceful_close(&mut socket).await;
    }
}
