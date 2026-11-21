//! Background Port 8080 TCP client sending JSON-line `ApiCommand`s and
//! receiving length-prefixed Postcard v3 `DeviceMsg` envelopes.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oximite_proto::model::{BrewProfile, MachineSettings, PidSettings, WifiSettings};
use oximite_proto::protocol::{ApiCommand, DeviceMsg, RxFramer, DIAG_MAGIC, DIAG_PROTO_VER};

use crate::store::{ControlStore, LogTag};

enum ClientAction {
    Connect(String),
    Disconnect,
    SendJson(String),
    Shutdown,
}

#[derive(Clone)]
pub struct ClientHandle {
    tx: Sender<ClientAction>,
    store: Arc<Mutex<ControlStore>>,
    connected: Arc<AtomicBool>,
    next_req_id: Arc<AtomicU32>,
}

impl ClientHandle {
    pub fn store(&self) -> &Arc<Mutex<ControlStore>> {
        &self.store
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
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

    fn alloc_req_id(&self) -> u32 {
        let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            self.next_req_id.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        }
    }

    fn send_api(&self, label: &str, mut cmd: ApiCommand<'_>) -> u32 {
        let req_id = self.alloc_req_id();
        cmd.req_id = Some(req_id);
        match serde_json::to_string(&cmd) {
            Ok(json) => {
                if let Ok(mut st) = self.store.lock() {
                    st.pending_cmds.insert(req_id, label.to_string());
                    st.add_log(LogTag::Cmd, format!("tx #{req_id}: {label}"), json.clone());
                }
                let _ = self.tx.send(ClientAction::SendJson(json));
            }
            Err(e) => {
                if let Ok(mut st) = self.store.lock() {
                    st.last_error = Some(format!("JSON encode failed: {e}"));
                }
            }
        }
        req_id
    }

    pub fn send_simple(&self, cmd: &str) -> u32 {
        self.send_api(cmd, ApiCommand::simple(cmd))
    }

    pub fn send_direct_pump(&self, power: f32) -> u32 {
        let mut c = ApiCommand::simple("direct_pump");
        c.power = Some(power);
        self.send_api(&format!("direct_pump({power:.0}%)"), c)
    }

    pub fn send_session_temp(&self, temp: f32) -> u32 {
        let mut c = ApiCommand::simple("set_session_temp");
        c.temp = Some(temp);
        self.send_api(&format!("set_session_temp({temp:.1})"), c)
    }

    pub fn send_run_profile(&self, profile: &BrewProfile) -> u32 {
        let mut c = ApiCommand::simple("profile");
        c.profile = Some(profile.clone());
        self.send_api(&format!("profile({})", profile.name.as_str()), c)
    }

    pub fn send_run_slot(&self, slot: u8) -> u32 {
        let mut c = ApiCommand::simple("run_slot");
        c.slot = Some(slot);
        self.send_api(&format!("run_slot({slot})"), c)
    }

    pub fn send_save_machine(&self, machine: MachineSettings) -> u32 {
        let mut c = ApiCommand::simple("save_machine");
        c.machine = Some(machine);
        self.send_api("save_machine", c)
    }

    pub fn send_save_pids(&self, temp_pid: PidSettings, pump_pid: PidSettings) -> u32 {
        let mut c = ApiCommand::simple("save_pids");
        c.temp_pid = Some(temp_pid);
        c.pump_pid = Some(pump_pid);
        self.send_api("save_pids", c)
    }

    pub fn send_save_wifi(&self, ssid: &str, password: &str) -> Result<u32, String> {
        let ssid_s =
            oximite_proto::heapless::String::try_from(ssid).map_err(|_| "SSID > 32 bytes")?;
        let pass_s = oximite_proto::heapless::String::try_from(password)
            .map_err(|_| "Password > 64 bytes")?;
        let mut c = ApiCommand::simple("save_wifi");
        c.wifi = Some(WifiSettings {
            ssid: ssid_s,
            password: pass_s,
        });
        Ok(self.send_api("save_wifi", c))
    }

    pub fn send_save_profile(&self, slot: u8, profile: BrewProfile) -> u32 {
        let mut c = ApiCommand::simple("save_profile");
        c.slot = Some(slot);
        c.profile = Some(profile);
        self.send_api(&format!("save_profile({slot})"), c)
    }

    pub fn send_delete_profile(&self, slot: u8) -> u32 {
        let mut c = ApiCommand::simple("delete_profile");
        c.slot = Some(slot);
        self.send_api(&format!("delete_profile({slot})"), c)
    }

    pub fn send_ping(&self) -> u32 {
        let req_id = self.alloc_req_id();
        if let Ok(mut st) = self.store.lock() {
            st.note_ping_sent(req_id);
        }
        let c = ApiCommand::simple("ping").with_req_id(req_id);
        if let Ok(json) = serde_json::to_string(&c) {
            let _ = self.tx.send(ClientAction::SendJson(json));
        }
        req_id
    }

    pub fn request_settings(&self) -> u32 {
        self.send_api("get_settings", ApiCommand::simple("get_settings"))
    }

    pub fn request_profiles(&self) -> u32 {
        self.send_api("get_profiles", ApiCommand::simple("get_profiles"))
    }

    pub fn request_profile(&self, slot: u8) -> u32 {
        let mut c = ApiCommand::simple("get_profile");
        c.slot = Some(slot);
        self.send_api(&format!("get_profile({slot})"), c)
    }

    pub fn set_stream(&self, enabled: bool, divider: u8) -> u32 {
        let mut c = ApiCommand::simple("set_stream");
        c.enabled = Some(enabled);
        c.divider = Some(divider);
        self.send_api(&format!("set_stream({enabled}, /{divider})"), c)
    }
}

pub fn start_client_service(
    store: Arc<Mutex<ControlStore>>,
) -> (ClientHandle, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let connected = Arc::new(AtomicBool::new(false));
    let handle = ClientHandle {
        tx,
        store: store.clone(),
        connected: connected.clone(),
        next_req_id: Arc::new(AtomicU32::new(1)),
    };

    let join = thread::spawn(move || worker_loop(rx, store, connected));
    (handle, join)
}

fn normalize_addr(addr: &str) -> Result<SocketAddr, String> {
    let trimmed = addr.trim();
    let with_port = if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:8080")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("resolve {with_port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no IP found for {with_port}"))
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

fn handle_device_msg(msg: DeviceMsg, store: &Arc<Mutex<ControlStore>>) {
    let Ok(mut st) = store.lock() else {
        return;
    };
    match msg {
        DeviceMsg::Hello(h) => {
            if h.magic != DIAG_MAGIC || h.proto_ver != DIAG_PROTO_VER {
                st.last_error = Some(format!(
                    "Protocol mismatch (magic={:#x}, ver={})",
                    h.magic, h.proto_ver
                ));
            } else {
                st.on_hello(h);
            }
        }
        DeviceMsg::Tick(f) => st.on_tick(f),
        DeviceMsg::CommandReply {
            req_id,
            ticket,
            err,
        } => {
            let label = st.pending_cmds.remove(&req_id).unwrap_or_default();
            match err {
                None => st.add_log(
                    LogTag::Cmd,
                    format!("ack #{req_id} ({label}) ticket={ticket:?}"),
                    String::new(),
                ),
                Some(e) => st.add_log(
                    LogTag::Cmd,
                    format!("ERR #{req_id} ({label}): {}", e.as_str()),
                    String::new(),
                ),
            }
        }
        DeviceMsg::SettingsSnapshot(s) => {
            st.settings = Some(s);
            st.add_log(
                LogTag::Settings,
                "Settings snapshot received",
                String::new(),
            );
        }
        DeviceMsg::ProfilesList(list) => {
            st.profiles = list.into_iter().collect();
        }
        DeviceMsg::ProfileData { slot, profile } => {
            if let Some(p) = profile {
                st.loaded_profile = Some((slot, p));
            }
        }
        DeviceMsg::Event(ev) => st.on_event(ev),
        DeviceMsg::Pong { req_id, uptime_ms } => st.on_pong(req_id, uptime_ms),
    }
}

fn worker_loop(
    rx: Receiver<ClientAction>,
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
                ClientAction::SendJson(line) => {
                    if let Some(ref mut s) = stream {
                        let msg = format!("{line}\n");
                        let _ = s.set_nonblocking(false);
                        let res = s.write_all(msg.as_bytes()).and_then(|()| s.flush());
                        let _ = s.set_nonblocking(true);
                        if let Err(e) = res {
                            disconnect_stream(
                                &mut stream,
                                &connected,
                                &store,
                                Some(format!("Write error: {e}")),
                            );
                        }
                    }
                }
            }
        }

        if stream.is_none() {
            if let Some(ref addr) = target_addr {
                if Instant::now() >= next_retry {
                    match normalize_addr(addr).and_then(|sa| {
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
                            }
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
                                Ok(Some(msg)) => handle_device_msg(msg, &store),
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
