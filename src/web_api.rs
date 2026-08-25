//! HTTP and line-oriented TCP interfaces to the machine.
//!
//! `wifi_server_task` serves the gzipped web UI plus a small JSON REST API on
//! port 80; `tcp_telemetry_task` streams binary diagnostic telemetry at 50 Hz
//! on port 8080 and accepts commands as newline-delimited JSON. Both are pure
//! protocol handling -- every command is forwarded to the coordinator as a
//! `MachineCommand` rather than acted on here.
//!
//! # Two telemetry channels, deliberately different
//!
//! The browser and the HIL test rig want different things, so they get
//! different payloads rather than one union of both:
//!
//! * **Port 80, `GET /api/telemetry`** -- [`UiTelemetry`] as JSON. Seven fields,
//!   ~70 bytes, offset-corrected and rounded for display. Polled at ~4 Hz by a
//!   browser that has no decoder, so human-readable JSON is the right call.
//! * **Port 8080** -- [`DiagFrame`] as postcard. Fifteen fields, 55 bytes,
//!   *raw* controller values at the full 50 Hz control rate. Binary because at
//!   50 Hz JSON float formatting dominates this path and a blocked TCP write
//!   costs a control frame.
//!
//! The diag stream opens with one [`DiagHeader`] carrying the constants a log
//! can't reconstruct (temperature offset, PID gains, tick rate), so the frames
//! themselves stay fixed-size and free of anything that never changes.

use core::str::from_utf8;
use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::Write;
use serde::{Deserialize, Serialize};

use crate::profiles::BrewProfile;
use crate::settings::{MachineSettings, PidSettings, Settings, WifiSettings};
use crate::state::{
    get_ack, get_session_brew_temp, get_state, get_telemetry, send_command, MachineCommand,
    MachineState, Telemetry, TELEMETRY_WATCH,
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

/// What the browser renders, and nothing else.
///
/// Every field here is read by `index.html`; anything it doesn't display
/// belongs on the diagnostic stream instead. Temperatures are offset-corrected
/// and everything is rounded to 2 dp, because this is a display payload rather
/// than a measurement.
#[derive(Serialize)]
struct UiTelemetry {
    /// Group-head temperature, i.e. boiler minus `temp_offset`.
    t: f32,
    /// Session brew temp — what the UI's +/- buttons adjust. Deliberately not
    /// derived from the applied setpoint, which is a function of machine state
    /// (0 °C while sleeping or cooling, steam temp while steaming), so
    /// stepping from it would send nonsense back.
    sbt: f32,
    p: f32,
    fl: f32,
    vol: f32,
    st: u32,
    /// Highest command ticket the coordinator has served, which is how a client
    /// tells a command that was merely queued from one the machine acted on.
    ack: u32,
}

/// Wire-format version for the port 8080 stream.
///
/// Bump on any change to [`DiagHeader`] or [`DiagFrame`] field order or type.
/// The client checks it on the header and on every frame, so a firmware /
/// test-suite mismatch surfaces as a clear error instead of plausible-looking
/// numbers decoded at the wrong offsets.
const DIAG_VER: u8 = 1;

/// "OXID" in little-endian byte order — lets the client confirm it is talking
/// to this protocol before it trusts any offsets.
const DIAG_MAGIC: u32 = 0x4449_584F;

/// Serialized size of [`DiagFrame`]. Constant only because the `u32`s use
/// `fixint` rather than postcard's default varints, which would shrink the
/// frame at low tick counts and grow it later — silently desynchronising a
/// client that reads fixed-size records. `postcard::to_slice` is checked
/// against this at runtime.
const DIAG_FRAME_LEN: usize = 55;

/// Serialized size of [`DiagHeader`], sent exactly once per connection.
const DIAG_HEADER_LEN: usize = 54;

/// Sent once when a client connects, before any frames.
///
/// Carries the constants needed to interpret the stream that would otherwise
/// be repeated 50 times a second or, worse, guessed by the analysis scripts.
/// The tick rate is deliberately absent: the control loop is mains-locked, so
/// its real rate is whatever the `ms` deltas say rather than a constant.
#[derive(Serialize)]
struct DiagHeader {
    #[serde(with = "postcard::fixint::le")]
    magic: u32,
    ver: u8,
    /// Size of each following record, so a client can validate its own
    /// unpacking against the firmware instead of assuming.
    frame_len: u8,
    /// Boiler-to-group offset. The frames carry raw boiler values; this is
    /// what converts them to the group-head numbers the UI shows.
    temp_offset: f32,
    brew_temp: f32,
    steam_temp: f32,
    temp_kp: f32,
    temp_ki: f32,
    temp_kd: f32,
    press_kp: f32,
    press_ki: f32,
    press_kd: f32,
    flow_kp: f32,
    flow_ki: f32,
    flow_kd: f32,
}

/// One control tick, exactly as the controller saw it.
///
/// Values are raw: no display offset, no rounding. A test analysing loop
/// behaviour wants the number the PID acted on, and `temp_offset` in the
/// header makes the display conversion recoverable anyway.
#[derive(Serialize)]
struct DiagFrame {
    ver: u8,
    /// Control-loop tick. Gaps mean the device computed a frame it never sent.
    #[serde(with = "postcard::fixint::le")]
    seq: u32,
    /// Device uptime in milliseconds.
    #[serde(with = "postcard::fixint::le")]
    ms: u32,
    /// Raw boiler temperature — *not* offset-corrected, unlike `UiTelemetry.t`.
    t: f32,
    tt: f32,
    /// Applied temperature setpoint including the brew-flow feed-forward.
    ett: f32,
    p: f32,
    tp: f32,
    /// Setpoint the pressure PID chased, after flow limiting.
    etp: f32,
    fl: f32,
    /// Active flow setpoint, 0 when the pump isn't flow-controlled.
    fll: f32,
    vol: f32,
    hp: f32,
    /// Pump triac duty, 0-100.
    pump: f32,
    /// 1 while the flow channel is the binding constraint on pump duty.
    fc: u8,
    st: u8,
}

#[derive(Serialize)]
struct ProfileHeader<'a> {
    slot: u8,
    name: &'a str,
}

/// Reply to an accepted command.
///
/// `ack` is the ticket the caller watches for in telemetry: the machine has
/// only obeyed the command once telemetry's `ack` reaches this value.
#[derive(Serialize)]
struct CommandAck {
    status: &'static str,
    ack: u32,
}

const JSON_OK_HEADER: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";

/// Why a request could not be served.
///
/// Each variant maps to the status line the client gets back, so a command
/// that never reached the coordinator is visible as an error instead of being
/// reported as success.
enum HttpError {
    /// Peer closed, errored, or stalled part-way through a request.
    Closed,
    /// The request, or the body it declared, is larger than the read buffer.
    TooLarge,
    /// Malformed request, or a body that is not a valid `ApiCommand`.
    BadRequest,
    NotFound,
    /// Command queue full, so the command was never queued and the machine will
    /// never run it.
    Busy,
}

impl HttpError {
    /// The response to send back, or `None` when there is no peer left to read
    /// it.
    fn status_line(&self) -> Option<&'static str> {
        match self {
            HttpError::Closed => None,
            HttpError::TooLarge => {
                Some("HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\n\r\n")
            }
            HttpError::BadRequest => Some("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n"),
            HttpError::NotFound => Some("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n"),
            HttpError::Busy => {
                Some("HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n")
            }
        }
    }
}

/// Byte offset just past the `\r\n\r\n` terminating the request head, i.e.
/// where the body starts.
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Declared body length, or `None` if the header is absent or unparseable.
fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// Reads until `buf` holds one complete request — head plus the body its
/// `Content-Length` declares — and returns that length.
///
/// A single `read` is not enough. TCP is a stream, so a POST whose body lands
/// in a second segment arrives here as a complete-looking head with an empty
/// body: the JSON parse fails and the command is dropped, but the request line
/// still routes and the client is told 200 OK. Waiting for the declared body
/// is what makes the parse failure below mean "bad JSON" rather than "not all
/// here yet".
async fn read_request(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, HttpError> {
    let mut n = 0;
    loop {
        if let Some(head) = head_end(&buf[..n]) {
            // Only the head has to be UTF-8 to find the length; the body is
            // validated once it is all here.
            let head_str = from_utf8(&buf[..head]).map_err(|_| HttpError::BadRequest)?;
            let want = head + content_length(head_str).unwrap_or(0);
            if want > buf.len() {
                return Err(HttpError::TooLarge);
            }
            if n >= want {
                // Anything past `want` belongs to a pipelined request, which
                // this server does not serve — cut it off rather than feed it
                // to the body parser.
                return Ok(want);
            }
        } else if n == buf.len() {
            return Err(HttpError::TooLarge);
        }

        match socket.read(&mut buf[n..]).await {
            Ok(0) | Err(_) => return Err(HttpError::Closed),
            Ok(r) => n += r,
        }
    }
}

/// Parses the JSON body of a request whose head is already complete.
fn parse_command(request: &str) -> Result<ApiCommand<'_>, HttpError> {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .ok_or(HttpError::BadRequest)?;
    let (payload, _) = serde_json_core::from_str::<ApiCommand>(body).map_err(|_| {
        defmt::warn!("API: Failed to parse JSON body");
        HttpError::BadRequest
    })?;
    Ok(payload)
}

/// Slot number from a `GET /api/profile/{slot}` request line.
fn parse_slot(request: &str) -> Result<u8, HttpError> {
    let rest = request
        .strip_prefix("GET /api/profile/")
        .ok_or(HttpError::NotFound)?;
    let end = rest.find(' ').unwrap_or(rest.len());
    rest[..end].parse::<u8>().map_err(|_| HttpError::NotFound)
}

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

/// Maps one API command onto a [`MachineCommand`], or says why it cannot be.
///
/// Every field a command needs is mandatory, so a caller is never handed a
/// success for a command that could not be built and therefore never ran.
async fn build_command(payload: ApiCommand<'_>) -> Result<MachineCommand, HttpError> {
    Ok(match payload.cmd {
        "power" => MachineCommand::TogglePower,
        "brew" => MachineCommand::Brew,
        "stop" => MachineCommand::Stop,
        "steam" => MachineCommand::Steam,
        "flush" => MachineCommand::Flush,
        "direct_pump" => {
            MachineCommand::DirectPump(payload.power.ok_or(HttpError::BadRequest)?)
        }
        "set_session_temp" => {
            MachineCommand::SetSessionTemp(payload.temp.ok_or(HttpError::BadRequest)?)
        }
        "save_machine" => MachineCommand::SaveMachine(payload.machine.ok_or(HttpError::BadRequest)?),
        "save_pids" => MachineCommand::SavePids(
            payload.temp_pid.ok_or(HttpError::BadRequest)?,
            payload.press_pid.ok_or(HttpError::BadRequest)?,
            payload.flow_pid,
        ),
        "save_wifi" => {
            let w = payload.wifi.ok_or(HttpError::BadRequest)?;
            defmt::info!("API: New SSID: {}", w.ssid.as_str());
            MachineCommand::SaveWifi(w)
        }
        "profile" => MachineCommand::RunProfile(payload.profile.ok_or(HttpError::BadRequest)?),
        "run_slot" => {
            let slot = payload.slot.ok_or(HttpError::BadRequest)?;
            let p = crate::profiles::get_profile_from_ram(slot)
                .await
                .ok_or(HttpError::NotFound)?;
            MachineCommand::RunProfile(p)
        }
        "save_profile" => MachineCommand::SaveProfile(
            payload.slot.ok_or(HttpError::BadRequest)?,
            payload.profile.ok_or(HttpError::BadRequest)?,
        ),
        "delete_profile" => {
            MachineCommand::DeleteProfile(payload.slot.ok_or(HttpError::BadRequest)?)
        }
        other => {
            defmt::warn!("API: Unknown command {}", other);
            return Err(HttpError::BadRequest);
        }
    })
}

/// Runs one command from the diag socket.
///
/// That socket is a one-way binary stream with no reply channel, so a command
/// it cannot run is logged — the test rig reads the log.
async fn dispatch_diag_command(payload: ApiCommand<'_>) {
    match build_command(payload).await {
        Ok(cmd) => {
            if send_command(cmd).is_none() {
                defmt::warn!("Diag: command dropped, queue full");
            }
        }
        Err(_) => defmt::warn!("Diag: command rejected"),
    }
}

/// Builds the display payload for one telemetry snapshot.
async fn ui_telemetry(a: Telemetry) -> UiTelemetry {
    let st_val = get_state();
    let s = Settings::get().await;

    let disp_t = a.display_temp(s.machine.temp_offset, st_val == MachineState::Steaming);

    // Sensor resolution is nowhere near f32 precision and the serializer
    // prints every digit it is given, so 2 dp roughly halves the payload with
    // no visible loss. `f32::round` needs std, so this rounds half away from
    // zero by hand.
    let r2 = |v: f32| {
        let bias = if v < 0.0 { -0.5 } else { 0.5 };
        ((v * 100.0 + bias) as i32) as f32 / 100.0
    };

    UiTelemetry {
        t: r2(disp_t),
        sbt: r2(get_session_brew_temp()),
        p: r2(a.pressure_bar),
        fl: r2(a.flow_rate_ml_s),
        vol: r2(a.volume_ml),
        st: st_val as u32,
        ack: get_ack(),
    }
}

/// Serializes `value` into `buf`, requiring it to occupy exactly `expected`
/// bytes.
///
/// Both diagnostic records are fixed-size by contract: the client reads them
/// as fixed-width records, so a short encoding would slide every following
/// field by the difference. A mismatch can only mean the struct changed
/// without its `*_LEN`/`DIAG_VER` being updated with it, so the record is
/// dropped and the cause logged rather than sent.
fn encode_fixed<T: Serialize>(
    value: &T,
    buf: &mut [u8],
    expected: usize,
    what: &str,
) -> Option<usize> {
    match postcard::to_slice(value, buf) {
        Ok(used) if used.len() == expected => Some(used.len()),
        Ok(used) => {
            defmt::error!(
                "Diag {} is {} bytes, expected {} — update its LEN const and DIAG_VER",
                what,
                used.len(),
                expected
            );
            None
        }
        Err(_) => {
            defmt::error!("Diag {} did not fit its buffer; dropped", what);
            None
        }
    }
}

/// Packs one control tick into the fixed-size diagnostic record.
fn encode_diag_frame(a: &Telemetry, buf: &mut [u8; DIAG_FRAME_LEN]) -> Option<usize> {
    let frame = DiagFrame {
        ver: DIAG_VER,
        seq: a.tick,
        ms: Instant::now().as_millis() as u32,
        t: a.temp_c,
        tt: a.target_temp,
        ett: a.effective_target_temp,
        p: a.pressure_bar,
        tp: a.target_bar,
        etp: a.effective_target_bar,
        fl: a.flow_rate_ml_s,
        fll: a.flow_limit_ml_s,
        vol: a.volume_ml,
        hp: a.heater_duty,
        pump: a.pump_duty,
        fc: a.flow_controlled as u8,
        st: get_state() as u8,
    };

    encode_fixed(&frame, buf, DIAG_FRAME_LEN, "frame")
}

/// Serialized size of the largest JSON response (`GET /api/settings`), header
/// included. One capacity for every route, rather than a `String`/byte-buffer
/// pair per route each needing its own two numbers kept in step.
const JSON_RESP_MAX: usize = 2048;

/// Writes a 200 OK JSON response whose body is `value` serialized.
///
/// `buf` is owned by the caller so that all routes share a single buffer: as a
/// local here it would be one 2 KiB slot per monomorphization in the server
/// task's future.
async fn write_json<T: Serialize>(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; JSON_RESP_MAX],
    value: &T,
) {
    let hdr = JSON_OK_HEADER.as_bytes();
    buf[..hdr.len()].copy_from_slice(hdr);
    match serde_json_core::to_slice(value, &mut buf[hdr.len()..]) {
        Ok(len) => {
            let _ = socket.write_all(&buf[..hdr.len() + len]).await;
        }
        // Silently sending nothing would look like a dropped connection, and
        // the cause (a payload wider than the buffer) would be invisible.
        Err(_) => defmt::warn!("JSON response exceeded its buffer; nothing sent"),
    }
}

/// Reads one request and serves it, reporting why if it could not be served.
async fn handle_connection(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; 4096],
) -> Result<(), HttpError> {
    let n = read_request(socket, buf).await?;
    let request = from_utf8(&buf[..n]).map_err(|_| HttpError::BadRequest)?;
    serve(socket, request).await
}

/// Routes one complete request. Every path either writes a response or returns
/// the error whose status line the caller sends.
async fn serve(socket: &mut TcpSocket<'_>, request: &str) -> Result<(), HttpError> {
    let mut buf = [0u8; JSON_RESP_MAX];

    if request.starts_with("GET / ") || request.starts_with("GET /index.html") {
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(headers.as_bytes()).await;
        let _ = socket.write_all(INDEX_HTML_GZ).await;
    } else if request.starts_with("GET /api/telemetry") {
        write_json(socket, &mut buf, &ui_telemetry(get_telemetry()).await).await;
    } else if request.starts_with("GET /api/settings") {
        write_json(socket, &mut buf, &Settings::get().await).await;
    } else if request.starts_with("GET /api/profiles") {
        let p_list = crate::profiles::get_all_profiles_from_ram().await;
        let mut headers: heapless::Vec<ProfileHeader, { crate::profiles::MAX_PROFILES as usize }> =
            heapless::Vec::new();
        for (slot, p) in p_list.iter() {
            let _ = headers.push(ProfileHeader {
                slot: *slot,
                name: p.name.as_str(),
            });
        }
        write_json(socket, &mut buf, &headers).await;
    } else if request.starts_with("GET /api/profile/") {
        let slot = parse_slot(request)?;
        let p = crate::profiles::get_profile_from_ram(slot)
            .await
            .ok_or(HttpError::NotFound)?;
        write_json(socket, &mut buf, &p).await;
    } else if request.starts_with("POST /api/cmd") {
        let payload = parse_command(request)?;
        defmt::info!("API Command Received: {}", payload.cmd);
        let cmd = build_command(payload).await?;
        let ticket = send_command(cmd).ok_or(HttpError::Busy)?;
        write_json(
            socket,
            &mut buf,
            &CommandAck {
                status: "ok",
                ack: ticket,
            },
        )
        .await;
    } else {
        return Err(HttpError::NotFound);
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
        // The socket timeout only bounds a single idle gap, so it cannot stop
        // a client trickling bytes forever. This bounds the whole exchange.
        let served = with_timeout(
            Duration::from_secs(10),
            handle_connection(&mut socket, &mut buf),
        )
        .await
        .unwrap_or(Err(HttpError::Closed));

        if let Err(e) = served {
            if let Some(status) = e.status_line() {
                let _ = socket.write_all(status.as_bytes()).await;
            }
        }

        graceful_close(&mut socket).await;
    }
}

/// Builds the one-shot connection header from current settings.
async fn encode_diag_header(buf: &mut [u8; DIAG_HEADER_LEN]) -> Option<usize> {
    let s = Settings::get().await;
    let header = DiagHeader {
        magic: DIAG_MAGIC,
        ver: DIAG_VER,
        frame_len: DIAG_FRAME_LEN as u8,
        temp_offset: s.machine.temp_offset,
        brew_temp: s.machine.brew_temp,
        steam_temp: s.machine.steam_temp,
        temp_kp: s.temp_pid.kp,
        temp_ki: s.temp_pid.ki,
        temp_kd: s.temp_pid.kd,
        press_kp: s.press_pid.kp,
        press_ki: s.press_pid.ki,
        press_kd: s.press_pid.kd,
        flow_kp: s.flow_pid.kp,
        flow_ki: s.flow_pid.ki,
        flow_kd: s.flow_pid.kd,
    };

    encode_fixed(&header, buf, DIAG_HEADER_LEN, "header")
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

        // The header must land before any frame, or the client cannot tell
        // where records begin. A failure here is fatal to the connection
        // rather than something to stream past.
        let mut header_buf = [0u8; DIAG_HEADER_LEN];
        match encode_diag_header(&mut header_buf).await {
            Some(n) if socket.write_all(&header_buf[..n]).await.is_ok() => {}
            _ => {
                graceful_close(&mut socket).await;
                continue;
            }
        }

        let mut line_buf = [0u8; 1024];
        let mut line_pos = 0;
        let mut frame_buf = [0u8; DIAG_FRAME_LEN];

        loop {
            let mut read_buf = [0u8; 128];
            let read_fut = socket.read(&mut read_buf);
            let tick_fut = telemetry_rx.changed();

            match select(tick_fut, read_fut).await {
                Either::First(a) => {
                    // Commands arrive as JSON lines; only the outbound
                    // direction is binary, so this socket stays bidirectional
                    // without any framing ambiguity.
                    if let Some(n) = encode_diag_frame(&a, &mut frame_buf) {
                        if socket.write_all(&frame_buf[..n]).await.is_err() {
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
                                                dispatch_diag_command(payload).await;
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
