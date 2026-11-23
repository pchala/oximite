//! Control client. Commands and queries use HTTP on Port 80. A second thread
//! reads the one-way Postcard telemetry stream on Port 8080.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oximite_proto::model::{
    BrewProfile, MachineSettings, PidSettings, ProfileHeaderOwned, PublicSettings, WifiSettings,
};
use oximite_proto::protocol::{ApiCommand, DeviceEvent, DeviceMsg, RxFramer, PROTO_VER};

use crate::store::{ControlStore, LogTag};

const DEFAULT_TELEMETRY_PORT: u16 = 8080;
const DEFAULT_HTTP_PORT: u16 = 80;
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

enum ClientAction {
    Connect(String),
    Disconnect,
    Shutdown,
}

/// What the HTTP worker does with a response body.
enum HttpKind {
    Command,
    Ping,
    Version,
    Settings,
    Profiles,
    Profile(u8),
}

struct HttpJob {
    label: String,
    kind: HttpKind,
    method: &'static str,
    path: String,
    body: Option<String>,
}

#[derive(Clone)]
pub struct ClientHandle {
    tx: Sender<ClientAction>,
    http_tx: Sender<HttpJob>,
    store: Arc<Mutex<ControlStore>>,
    connected: Arc<AtomicBool>,
    http_port: Arc<AtomicU16>,
}

impl ClientHandle {
    pub fn store(&self) -> &Arc<Mutex<ControlStore>> {
        &self.store
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Overrides the HTTP port. Only tests need this.
    pub fn set_http_port(&self, port: u16) {
        self.http_port.store(port, Ordering::Relaxed);
    }

    pub fn connect(&self, addr: &str) {
        if let Ok(mut st) = self.store.lock() {
            st.endpoint = addr.trim().to_string();
            st.connecting = true;
            st.last_error = None;
        }
        let _ = self.tx.send(ClientAction::Connect(addr.trim().to_string()));
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(ClientAction::Disconnect);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(ClientAction::Shutdown);
    }

    fn post_cmd(&self, label: &str, cmd: &ApiCommand<'_>) {
        match serde_json::to_string(cmd) {
            Ok(json) => {
                if let Ok(mut st) = self.store.lock() {
                    st.add_log(LogTag::Cmd, format!("tx: {label}"), json.clone());
                }
                let _ = self.http_tx.send(HttpJob {
                    label: label.to_string(),
                    kind: HttpKind::Command,
                    method: "POST",
                    path: "/api/cmd".into(),
                    body: Some(json),
                });
            }
            Err(e) => {
                if let Ok(mut st) = self.store.lock() {
                    st.last_error = Some(format!("JSON encode failed: {e}"));
                }
            }
        }
    }

    fn get(&self, label: &str, kind: HttpKind, path: String) {
        let _ = self.http_tx.send(HttpJob {
            label: label.to_string(),
            kind,
            method: "GET",
            path,
            body: None,
        });
    }

    pub fn send_simple(&self, cmd: &str) {
        self.post_cmd(cmd, &ApiCommand::simple(cmd));
    }

    pub fn send_direct_pump(&self, power: f32) {
        let mut c = ApiCommand::simple("direct_pump");
        c.power = Some(power);
        self.post_cmd(&format!("direct_pump({power:.0}%)"), &c);
    }

    pub fn send_session_temp(&self, temp: f32) {
        let mut c = ApiCommand::simple("set_session_temp");
        c.temp = Some(temp);
        self.post_cmd(&format!("set_session_temp({temp:.1})"), &c);
    }

    pub fn send_run_profile(&self, profile: &BrewProfile) {
        let mut c = ApiCommand::simple("profile");
        c.profile = Some(profile.clone());
        self.post_cmd(&format!("profile({})", profile.name.as_str()), &c);
    }

    pub fn send_run_slot(&self, slot: u8) {
        let mut c = ApiCommand::simple("run_slot");
        c.slot = Some(slot);
        self.post_cmd(&format!("run_slot({slot})"), &c);
    }

    pub fn send_save_machine(&self, machine: MachineSettings) {
        let mut c = ApiCommand::simple("save_machine");
        c.machine = Some(machine);
        self.post_cmd("save_machine", &c);
    }

    pub fn send_save_pids(&self, temp_pid: PidSettings, pump_pid: PidSettings) {
        let mut c = ApiCommand::simple("save_pids");
        c.temp_pid = Some(temp_pid);
        c.pump_pid = Some(pump_pid);
        self.post_cmd("save_pids", &c);
    }

    pub fn send_save_wifi(&self, ssid: &str, password: &str) -> Result<(), String> {
        let ssid_s =
            oximite_proto::heapless::String::try_from(ssid).map_err(|_| "SSID > 32 bytes")?;
        let pass_s = oximite_proto::heapless::String::try_from(password)
            .map_err(|_| "Password > 64 bytes")?;
        let mut c = ApiCommand::simple("save_wifi");
        c.wifi = Some(WifiSettings {
            ssid: ssid_s,
            password: pass_s,
        });
        self.post_cmd("save_wifi", &c);
        Ok(())
    }

    pub fn send_save_profile(&self, slot: u8, profile: BrewProfile) {
        let mut c = ApiCommand::simple("save_profile");
        c.slot = Some(slot);
        c.profile = Some(profile);
        self.post_cmd(&format!("save_profile({slot})"), &c);
    }

    pub fn send_delete_profile(&self, slot: u8) {
        let mut c = ApiCommand::simple("delete_profile");
        c.slot = Some(slot);
        self.post_cmd(&format!("delete_profile({slot})"), &c);
    }

    /// Measures the HTTP round trip with `GET /api/telemetry`.
    pub fn send_ping(&self) {
        self.get("ping", HttpKind::Ping, "/api/telemetry".into());
    }

    pub fn request_settings(&self) {
        self.get("get_settings", HttpKind::Settings, "/api/settings".into());
    }

    pub fn request_profiles(&self) {
        self.get("get_profiles", HttpKind::Profiles, "/api/profiles".into());
    }

    pub fn request_profile(&self, slot: u8) {
        self.get(
            &format!("get_profile({slot})"),
            HttpKind::Profile(slot),
            format!("/api/profile/{slot}"),
        );
    }
}

pub fn start_client_service(
    store: Arc<Mutex<ControlStore>>,
) -> (ClientHandle, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let (http_tx, http_rx) = mpsc::channel();
    let connected = Arc::new(AtomicBool::new(false));
    let http_port = Arc::new(AtomicU16::new(DEFAULT_HTTP_PORT));
    let handle = ClientHandle {
        tx,
        http_tx: http_tx.clone(),
        store: store.clone(),
        connected: connected.clone(),
        http_port: http_port.clone(),
    };

    let http_store = store.clone();
    thread::spawn(move || http_loop(http_rx, http_store, http_port));
    let join = thread::spawn(move || worker_loop(rx, http_tx, store, connected));
    (handle, join)
}

/// Splits `host[:port]` into a host and a telemetry port.
fn split_endpoint(addr: &str) -> (String, u16) {
    let trimmed = addr.trim();
    match trimmed.rsplit_once(':') {
        Some((host, port)) => match port.parse() {
            Ok(p) => (host.to_string(), p),
            Err(_) => (trimmed.to_string(), DEFAULT_TELEMETRY_PORT),
        },
        None => (trimmed.to_string(), DEFAULT_TELEMETRY_PORT),
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no IP found for {host}:{port}"))
}

/// Sends one HTTP/1.1 request and returns `(status, body)`.
fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let mut s = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(HTTP_TIMEOUT));
    let _ = s.set_write_timeout(Some(HTTP_TIMEOUT));
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    match s.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::ConnectionReset && !raw.is_empty() => {}
        Err(e) => return Err(e.to_string()),
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "malformed HTTP status line".to_string())?;
    Ok((status, body.to_string()))
}

fn log_cmd(store: &Arc<Mutex<ControlStore>>, text: String) {
    if let Ok(mut st) = store.lock() {
        st.add_log(LogTag::Cmd, text, String::new());
    }
}

fn http_loop(rx: Receiver<HttpJob>, store: Arc<Mutex<ControlStore>>, http_port: Arc<AtomicU16>) {
    while let Ok(job) = rx.recv() {
        let endpoint = match store.lock() {
            Ok(st) => st.endpoint.clone(),
            Err(_) => continue,
        };
        let (host, _) = split_endpoint(&endpoint);
        let started = Instant::now();
        let result = resolve(&host, http_port.load(Ordering::Relaxed))
            .and_then(|a| http_request(a, job.method, &job.path, job.body.as_deref()));
        let (status, body) = match result {
            Ok(r) => r,
            Err(e) => {
                log_cmd(&store, format!("ERR ({}): {e}", job.label));
                continue;
            }
        };
        if status != 200 {
            log_cmd(&store, format!("ERR ({}): HTTP {status}", job.label));
            continue;
        }
        apply_response(&store, &job, &body, started.elapsed());
    }
}

fn apply_response(store: &Arc<Mutex<ControlStore>>, job: &HttpJob, body: &str, rtt: Duration) {
    let Ok(mut st) = store.lock() else {
        return;
    };
    match job.kind {
        HttpKind::Command => st.add_log(
            LogTag::Cmd,
            format!("ack ({}): {body}", job.label),
            String::new(),
        ),
        HttpKind::Ping => {
            let ms = rtt.as_secs_f32() * 1000.0;
            st.last_ping_rtt_ms = Some(ms);
            st.add_log(LogTag::Net, format!("HTTP rtt={ms:.1}ms"), String::new());
        }
        HttpKind::Version => match serde_json::from_str::<serde_json::Value>(body) {
            Ok(v) if v["proto"].as_u64() == Some(u64::from(PROTO_VER)) => {}
            Ok(v) => st.add_log(
                LogTag::Net,
                format!("Protocol mismatch: device {} app {PROTO_VER}", v["proto"]),
                String::new(),
            ),
            Err(e) => st.add_log(LogTag::Net, format!("Bad version JSON: {e}"), String::new()),
        },
        HttpKind::Settings => match serde_json::from_str::<PublicSettings>(body) {
            Ok(s) => st.settings = Some(s),
            Err(e) => st.add_log(
                LogTag::Net,
                format!("Bad settings JSON: {e}"),
                String::new(),
            ),
        },
        HttpKind::Profiles => match serde_json::from_str::<Vec<ProfileHeaderOwned>>(body) {
            Ok(list) => st.profiles = list,
            Err(e) => st.add_log(
                LogTag::Net,
                format!("Bad profiles JSON: {e}"),
                String::new(),
            ),
        },
        HttpKind::Profile(slot) => match serde_json::from_str::<BrewProfile>(body) {
            Ok(p) => st.loaded_profile = Some((slot, p)),
            Err(e) => st.add_log(LogTag::Net, format!("Bad profile JSON: {e}"), String::new()),
        },
    }
}

fn disconnect_stream(
    stream: &mut Option<TcpStream>,
    connected: &AtomicBool,
    store: &Arc<Mutex<ControlStore>>,
    reason: Option<String>,
) {
    if let Some(s) = stream.take() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    connected.store(false, Ordering::Relaxed);
    if let Ok(mut st) = store.lock() {
        st.connected = false;
        st.connecting = false;
        if let Some(err) = reason {
            st.last_error = Some(err.clone());
            st.add_log(LogTag::Net, err, String::new());
        }
    }
}

fn queue_get(http_tx: &Sender<HttpJob>, label: &str, kind: HttpKind, path: &str) {
    let _ = http_tx.send(HttpJob {
        label: label.to_string(),
        kind,
        method: "GET",
        path: path.to_string(),
        body: None,
    });
}

fn refetch_version(http_tx: &Sender<HttpJob>) {
    queue_get(http_tx, "get_version", HttpKind::Version, "/api/version");
}

fn refetch_settings(http_tx: &Sender<HttpJob>) {
    queue_get(http_tx, "get_settings", HttpKind::Settings, "/api/settings");
}

fn refetch_profiles(http_tx: &Sender<HttpJob>) {
    queue_get(http_tx, "get_profiles", HttpKind::Profiles, "/api/profiles");
}

fn handle_device_msg(msg: DeviceMsg, store: &Arc<Mutex<ControlStore>>, http_tx: &Sender<HttpJob>) {
    match msg {
        DeviceMsg::Tick(tick) => {
            if let Ok(mut st) = store.lock() {
                st.on_wire_tick(tick);
            }
        }
        DeviceMsg::Event(ev) => {
            if let Ok(mut st) = store.lock() {
                st.on_event(ev);
            }
            match ev {
                DeviceEvent::SettingsUpdated => refetch_settings(http_tx),
                DeviceEvent::ProfilesUpdated => refetch_profiles(http_tx),
                _ => {}
            }
        }
    }
}

fn worker_loop(
    rx: Receiver<ClientAction>,
    http_tx: Sender<HttpJob>,
    store: Arc<Mutex<ControlStore>>,
    connected: Arc<AtomicBool>,
) {
    let mut target_addr: Option<String> = None;
    let mut stream: Option<TcpStream> = None;
    let mut framer = RxFramer::<4096>::new();
    let mut next_retry = Instant::now();
    let mut buf = [0u8; 2048];

    loop {
        while let Ok(action) = rx.try_recv() {
            match action {
                ClientAction::Shutdown => return,
                ClientAction::Disconnect => {
                    target_addr = None;
                    disconnect_stream(&mut stream, &connected, &store, Some("Disconnected".into()));
                }
                ClientAction::Connect(addr) => {
                    disconnect_stream(&mut stream, &connected, &store, None);
                    target_addr = Some(addr);
                    next_retry = Instant::now();
                }
            }
        }

        if stream.is_none() {
            if let Some(ref addr) = target_addr {
                if Instant::now() >= next_retry {
                    let (host, port) = split_endpoint(addr);
                    match resolve(&host, port).and_then(|sa| {
                        TcpStream::connect_timeout(&sa, Duration::from_secs(2))
                            .map_err(|e| format!("connect {sa}: {e}"))
                    }) {
                        Ok(s) => {
                            let _ = s.set_nodelay(true);
                            let _ = s.set_nonblocking(true);
                            framer.clear();
                            stream = Some(s);
                            connected.store(true, Ordering::Relaxed);
                            if let Ok(mut st) = store.lock() {
                                st.connected = true;
                                st.connecting = false;
                                st.last_error = None;
                                st.on_connected();
                            }
                            refetch_version(&http_tx);
                            refetch_settings(&http_tx);
                            refetch_profiles(&http_tx);
                        }
                        Err(err) => {
                            if let Ok(mut st) = store.lock() {
                                st.connected = false;
                                st.connecting = true;
                                st.last_error = Some(err);
                            }
                            next_retry = Instant::now() + Duration::from_secs(2);
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        let mut progressed = false;
        if let Some(ref mut s) = stream {
            match s.read(&mut buf) {
                Ok(0) => {
                    disconnect_stream(
                        &mut stream,
                        &connected,
                        &store,
                        Some("Peer closed connection".into()),
                    );
                    next_retry = Instant::now() + Duration::from_secs(1);
                }
                Ok(n) => {
                    progressed = true;
                    let mut at = 0;
                    while at < n {
                        let pushed = framer.accept(&buf[at..n]);
                        if pushed == 0 {
                            framer.clear();
                            break;
                        }
                        at += pushed;
                        loop {
                            match framer.next_frame() {
                                Ok(Some(msg)) => handle_device_msg(msg, &store, &http_tx),
                                Ok(None) => break,
                                Err(e) => {
                                    if let Ok(mut st) = store.lock() {
                                        st.add_log(
                                            LogTag::Net,
                                            format!("Frame error: {e:?}"),
                                            String::new(),
                                        );
                                    }
                                    framer.clear();
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => {
                    disconnect_stream(
                        &mut stream,
                        &connected,
                        &store,
                        Some(format!("Read error: {e}")),
                    );
                    next_retry = Instant::now() + Duration::from_secs(1);
                }
            }
        }

        if !progressed {
            thread::sleep(Duration::from_millis(5));
        }
    }
}
