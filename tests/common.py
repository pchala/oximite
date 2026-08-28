"""
Shared utilities, protocol definitions, HTTP client, and TCP telemetry client
for the Oximite HIL test suite.
"""

import gzip
import http.client
import json
import math
import os
import re
import socket
import struct
import threading
import time
import unittest
from typing import Any, Dict, List, Optional, Tuple, Union

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

# ==========================================
# CONFIGURATION
# ==========================================
TCP_IP = os.environ.get("OXIMITE_IP", "192.168.1.117")
HTTP_PORT = int(os.environ.get("OXIMITE_HTTP_PORT", "80"))
TCP_PORT = int(os.environ.get("OXIMITE_TCP_PORT", "8080"))
BASE_URL = f"http://{TCP_IP}:{HTTP_PORT}"

TIMESTAMP = time.strftime("%Y%m%d_%H%M_")
PLOTS_DIR = "test_plots"

# Machine State Discriminants (matches MachineState enum in src/state.rs)
STATE_IDLE = 0
STATE_BREWING = 1
STATE_STEAMING = 2
STATE_SLEEPING = 3
STATE_PUMPING = 4
STATE_COOLING = 5
STATE_HOT_WATER = 6

STATE_NAMES = {
    STATE_IDLE: "IDLE",
    STATE_BREWING: "BREWING",
    STATE_STEAMING: "STEAMING",
    STATE_SLEEPING: "SLEEPING",
    STATE_PUMPING: "PUMPING",
    STATE_COOLING: "COOLING",
    STATE_HOT_WATER: "HOT_WATER",
}

# ==========================================
# DIAGNOSTIC WIRE FORMAT (port 8080)
# ==========================================
DIAG_VER = 2
DIAG_MAGIC = 0x4449584F  # "OXID"

DIAG_HEADER_FMT = '<IBB9f'
DIAG_HEADER_FIELDS = [
    'magic', 'ver', 'frame_len',
    'temp_offset', 'brew_temp', 'steam_temp',
    'temp_kp', 'temp_ki', 'temp_kd',
    'pump_kp', 'pump_ki', 'pump_kd',
]
DIAG_HEADER_LEN = struct.calcsize(DIAG_HEADER_FMT)

DIAG_FRAME_FMT = '<BII10fBB'
DIAG_FRAME_FIELDS = [
    'ver', 'seq', 'ms',
    't', 'tt', 'ett',
    'p', 'tp',
    'fl', 'tfl', 'vol', 'hp', 'pump',
    'fc', 'st',
]
DIAG_FRAME_LEN = struct.calcsize(DIAG_FRAME_FMT)


def decode_diag_header(raw: bytes) -> Dict[str, Any]:
    """Decodes the one-time diagnostic header received upon connecting to port 8080."""
    hdr = dict(zip(DIAG_HEADER_FIELDS, struct.unpack(DIAG_HEADER_FMT, raw)))
    if hdr['magic'] != DIAG_MAGIC:
        raise ValueError(f"bad diag magic {hdr['magic']:#010x}, expected {DIAG_MAGIC:#010x}")
    if hdr['ver'] != DIAG_VER:
        raise ValueError(f"diag protocol v{hdr['ver']}, this suite speaks v{DIAG_VER}")
    if hdr['frame_len'] != DIAG_FRAME_LEN:
        raise ValueError(
            f"device frames are {hdr['frame_len']} bytes, decoder expects {DIAG_FRAME_LEN}")
    return hdr


def decode_diag_frame(raw: bytes) -> Dict[str, Any]:
    """Decodes one 55-byte binary telemetry frame from port 8080."""
    row = dict(zip(DIAG_FRAME_FIELDS, struct.unpack(DIAG_FRAME_FMT, raw)))
    if row['ver'] != DIAG_VER:
        raise ValueError(f"frame v{row['ver']} in a v{DIAG_VER} stream — reader lost alignment")
    return row


# ==========================================
# HTTP CLIENT HELPERS (port 80)
# ==========================================

def http_raw_request(
    method: str,
    path: str,
    body: Optional[Union[bytes, str]] = None,
    headers: Optional[Dict[str, str]] = None,
    timeout: float = 5.0,
) -> Tuple[int, Dict[str, str], bytes]:
    """
    Executes a direct HTTP/1.1 request using http.client to port 80.
    Returns (status_code, response_headers_dict, raw_response_body).
    """
    conn = http.client.HTTPConnection(TCP_IP, HTTP_PORT, timeout=timeout)
    req_headers = headers.copy() if headers else {}
    if body is not None and 'Content-Length' not in req_headers and 'content-length' not in req_headers:
        if isinstance(body, str):
            body_bytes = body.encode('utf-8')
        else:
            body_bytes = body
        req_headers['Content-Length'] = str(len(body_bytes))
    else:
        body_bytes = body.encode('utf-8') if isinstance(body, str) else body

    try:
        conn.request(method, path, body=body_bytes, headers=req_headers)
        resp = conn.getresponse()
        resp_body = resp.read()
        resp_headers = {k.lower(): v for k, v in resp.getheaders()}
        return resp.status, resp_headers, resp_body
    finally:
        conn.close()


def http_get(path: str, timeout: float = 5.0) -> Tuple[int, Any]:
    """
    Performs GET request to given path. If response is JSON, returns (status, parsed_json).
    If response is gzipped (e.g. index.html), decompresses and returns (status, decompressed_bytes).
    Otherwise returns (status, raw_bytes).
    """
    try:
        status, headers, body = http_raw_request("GET", path, timeout=timeout)
    except Exception as e:
        return 0, str(e)

    if headers.get('content-encoding') == 'gzip':
        try:
            body = gzip.decompress(body)
        except Exception:
            pass
    
    if 'application/json' in headers.get('content-type', ''):
        try:
            return status, json.loads(body.decode('utf-8'))
        except Exception:
            return status, body.decode('utf-8', errors='replace')
    return status, body


def http_post_cmd(cmd: str, payload: Optional[Dict[str, Any]] = None, timeout: float = 5.0) -> Tuple[int, Any]:
    """Sends a command to POST /api/cmd as JSON."""
    data = {"cmd": cmd}
    if payload:
        data.update(payload)
    body = json.dumps(data)
    headers = {"Content-Type": "application/json"}
    try:
        status, resp_headers, resp_body = http_raw_request("POST", "/api/cmd", body=body, headers=headers, timeout=timeout)
    except Exception as e:
        return 0, str(e)
    try:
        return status, json.loads(resp_body.decode('utf-8'))
    except Exception:
        return status, resp_body.decode('utf-8', errors='replace')


def get_telemetry_http(timeout: float = 3.0) -> Dict[str, Any]:
    """Fetches telemetry snapshot from GET /api/telemetry."""
    status, data = http_get("/api/telemetry", timeout=timeout)
    if status != 200 or not isinstance(data, dict):
        raise RuntimeError(f"Failed to fetch telemetry via HTTP: status {status}, response {data}")
    return data


def wait_for_ack(ticket: int, timeout: float = 5.0) -> bool:
    """Polls telemetry until the coordinator reports having served `ticket`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if get_telemetry_http(timeout=2.0).get("ack", -1) >= ticket:
                return True
        except Exception:
            pass
        time.sleep(0.1)
    return False


def get_settings_http(timeout: float = 3.0) -> Dict[str, Any]:
    """Fetches machine settings from GET /api/settings."""
    status, data = http_get("/api/settings", timeout=timeout)
    if status != 200 or not isinstance(data, dict):
        raise RuntimeError(f"Failed to fetch settings via HTTP: status {status}, response {data}")
    return data


def get_profiles_http(timeout: float = 3.0) -> List[Dict[str, Any]]:
    """Fetches profile list from GET /api/profiles."""
    status, data = http_get("/api/profiles", timeout=timeout)
    if status != 200 or not isinstance(data, list):
        raise RuntimeError(f"Failed to fetch profiles list via HTTP: status {status}, response {data}")
    return data


def get_profile_http(slot: int, timeout: float = 3.0) -> Tuple[int, Any]:
    """Fetches profile from GET /api/profile/{slot}."""
    status, data = http_get(f"/api/profile/{slot}", timeout=timeout)
    if status == 200 and isinstance(data, dict):
        return 200, data
    return status, {}


# ==========================================
# TCP DIAGNOSTIC STREAM CLIENT (port 8080)
# ==========================================

class TcpDiagClient:
    """Manages the 50 Hz binary telemetry stream & command socket on port 8080."""

    def __init__(self, ip: str = TCP_IP, port: int = TCP_PORT):
        self.ip = ip
        self.port = port
        self.sock: Optional[socket.socket] = None
        self.read_thread: Optional[threading.Thread] = None
        self.running = False
        self.diag_header: Optional[Dict[str, Any]] = None
        self.telemetry_history: List[Dict[str, Any]] = []
        self.current_state: int = STATE_IDLE
        self.parse_errors: int = 0
        self.connected_event = threading.Event()

    def start(self, timeout: float = 5.0):
        self.running = True
        self.connected_event.clear()
        try:
            self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.sock.settimeout(timeout)
            self.sock.connect((self.ip, self.port))
            self.read_thread = threading.Thread(target=self._read_loop, daemon=True)
            self.read_thread.start()
            # Wait for header
            self.connected_event.wait(timeout=timeout)
        except Exception as e:
            self.running = False
            if self.sock:
                try:
                    self.sock.close()
                except Exception:
                    pass
            raise ConnectionError(f"Failed to connect to diagnostic stream at {self.ip}:{self.port}: {e}")

    def stop(self):
        self.running = False
        if self.sock:
            try:
                self.sock.shutdown(socket.SHUT_RDWR)
            except Exception:
                pass
            try:
                self.sock.close()
            except Exception:
                pass
        if self.read_thread and self.read_thread.is_alive():
            self.read_thread.join(timeout=2.0)

    def _recv_exactly(self, n: int) -> Optional[bytes]:
        buf = bytearray()
        while len(buf) < n and self.running:
            try:
                chunk = self.sock.recv(n - len(buf))
            except socket.timeout:
                continue
            except OSError:
                return None
            if not chunk:
                return None
            buf += chunk
        return bytes(buf) if len(buf) == n else None

    def _read_loop(self):
        raw_hdr = self._recv_exactly(DIAG_HEADER_LEN)
        if raw_hdr is None:
            return
        try:
            self.diag_header = decode_diag_header(raw_hdr)
            self.connected_event.set()
        except ValueError as e:
            print(f"Diagnostic stream rejected: {e}")
            return

        while self.running:
            raw = self._recv_exactly(DIAG_FRAME_LEN)
            if raw is None:
                break
            try:
                row = decode_diag_frame(raw)
            except (ValueError, struct.error):
                self.parse_errors += 1
                continue
            self.telemetry_history.append(row)
            self.current_state = row['st']

    def send_command(self, cmd_dict: Dict[str, Any]):
        if self.sock and self.running:
            msg = json.dumps(cmd_dict) + "\n"
            try:
                self.sock.sendall(msg.encode('utf-8'))
            except Exception as e:
                print(f"Failed to send TCP command: {e}")

    def snapshot_history(self) -> List[Dict[str, Any]]:
        return list(self.telemetry_history)

    def clear_history(self):
        self.telemetry_history.clear()


# ==========================================
# PLOTTING & LOGGING HELPERS
# ==========================================

def save_telemetry_json(history: List[Dict[str, Any]], safe_title: str, diag_header: Optional[Dict[str, Any]] = None):
    os.makedirs(PLOTS_DIR, exist_ok=True)
    telemetry_filepath = os.path.join(PLOTS_DIR, f"{TIMESTAMP}{safe_title}.json")
    try:
        with open(telemetry_filepath, 'w') as f:
            if diag_header:
                json.dump({'hdr': diag_header}, f)
                f.write("\n")
            for rep in history:
                json.dump(rep, f)
                f.write("\n")
        print(f"Saved telemetry: {telemetry_filepath}")
    except Exception as e:
        print(f"Failed to save telemetry JSON: {e}")


def plot_results(history: List[Dict[str, Any]], title: str, diag_header: Optional[Dict[str, Any]] = None):
    if not history:
        print(f"No data collected for {title}")
        return

    os.makedirs(PLOTS_DIR, exist_ok=True)
    t0 = history[0]['ms']
    x = [(d['ms'] - t0) / 1000.0 for d in history]

    def col(k):
        return [d.get(k, 0) for d in history]

    p, tp = col('p'), col('tp')
    vol, fl, tfl = col('vol'), col('fl'), col('tfl')
    hp, pump = col('hp'), col('pump')
    t, tt, ett = col('t'), col('tt'), col('ett')

    fig, (ax_temp, ax_press, ax_flow) = plt.subplots(3, 1, figsize=(12, 12), sharex=True)

    # --- Top Panel: Temperature ---
    ax_temp.set_title(title.replace('_', ' '), fontweight='bold', fontsize=14)
    ax_temp.set_ylabel("Temperature (°C)", color='tab:red', fontweight='bold')
    line_tt = ax_temp.plot(x, tt, label="Target Boiler Temp", linestyle="--", color='grey')
    line_ett = ax_temp.plot(x, ett, label="Applied Target (+flow FF)", linestyle=":", color='tab:green', linewidth=2)
    line_t = ax_temp.plot(x, t, label="Boiler Temp", color='tab:red', linewidth=2)

    ax_hp = ax_temp.twinx()
    ax_hp.set_ylabel("Heater Power (%)", color='tab:orange', fontweight='bold')
    line_hp = ax_hp.plot(x, hp, label="Heater Power", color='tab:orange', alpha=0.5)
    ax_hp.set_ylim(-5, 105)

    lines_top = line_tt + line_ett + line_t + line_hp
    labels_top = [l.get_label() for l in lines_top]
    ax_temp.legend(lines_top, labels_top, loc='upper left')
    ax_temp.grid(True, alpha=0.3)

    # --- Middle Panel: Pressure vs Duty ---
    color_p = 'tab:blue'
    ax_press.set_ylabel("Pressure (Bar)", color=color_p, fontweight='bold')
    line1 = ax_press.plot(x, tp, label="Profile Target", linestyle="--", color='grey')
    line2 = ax_press.plot(x, p, label="Actual Pressure", color=color_p, linewidth=2)
    ax_press.tick_params(axis='y', labelcolor=color_p)
    ax_press.set_ylim(bottom=0)

    ax_pump = ax_press.twinx()
    ax_pump.set_ylabel("Pump Power (%)", color='tab:brown', fontweight='bold')
    line_pump = ax_pump.plot(x, pump, label="Pump Power", color='tab:brown', alpha=0.6)
    ax_pump.set_ylim(-5, 105)

    lines_mid = line1 + line2 + line_pump
    ax_press.legend(lines_mid, [l.get_label() for l in lines_mid], loc='upper left')
    ax_press.grid(True, alpha=0.3)

    # --- Bottom Panel: Flow vs Setpoint & Volume ---
    color_f = 'tab:purple'
    ax_flow.set_xlabel("Time (Seconds)", fontweight='bold')
    ax_flow.set_ylabel("Flow Rate (ml/s)", color=color_f, fontweight='bold')
    line3 = ax_flow.plot(x, fl, label="Flow Rate", color=color_f, linewidth=2)
    line_tfl = ax_flow.plot(x, tfl, label="Flow Setpoint", linestyle="--", color='grey')
    ax_flow.tick_params(axis='y', labelcolor=color_f)
    ax_flow.set_ylim(bottom=0)

    ax_vol = ax_flow.twinx()
    ax_vol.set_ylabel("Volume (ml)", color='tab:cyan', fontweight='bold')
    line_vol = ax_vol.plot(x, vol, label="Volume", color='tab:cyan', alpha=0.7)

    lines_bot = line3 + line_tfl + line_vol
    ax_flow.legend(lines_bot, [l.get_label() for l in lines_bot], loc='upper left')
    ax_flow.grid(True, alpha=0.3)

    fig.tight_layout()

    safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
    filepath = os.path.join(PLOTS_DIR, f"{TIMESTAMP}{safe_title}.png")
    plt.savefig(filepath, dpi=150)
    plt.close(fig)
    print(f"Saved plot: {filepath}")

    save_telemetry_json(history, safe_title, diag_header)


def plot_stability_results(history: List[Dict[str, Any]], title: str, diag_header: Optional[Dict[str, Any]] = None):
    if not history:
        print(f"No data collected for {title}")
        return

    os.makedirs(PLOTS_DIR, exist_ok=True)
    temps = [d['t'] for d in history]
    targets = [d['tt'] for d in history]

    t0 = history[0]['ms']
    times = [(d['ms'] - t0) / 60000.0 for d in history]

    mean_t = sum(temps) / len(temps)
    min_t = min(temps)
    max_t = max(temps)
    variance = sum((x - mean_t) ** 2 for x in temps) / len(temps)
    std_dev = math.sqrt(variance)

    fig, ax = plt.subplots(figsize=(12, 7))
    ax.plot(times, targets, label="Target Boiler Temp (°C)", linestyle="--", color='grey', alpha=0.7)
    ax.plot(times, temps, label="Boiler Temp (°C)", color='tab:red', linewidth=1.5)

    ax.set_title(f"Boiler Temperature Stability", fontweight='bold', fontsize=16)
    ax.set_xlabel("Time (Minutes)", fontweight='bold')
    ax.set_ylabel("Temperature (°C)", fontweight='bold')
    ax.grid(True, alpha=0.3)
    ax.legend(loc='upper right')

    stats_text = (
        f"Statistics:\nMean: {mean_t:.2f}°C\nMin:  {min_t:.2f}°C\nMax:  {max_t:.2f}°C\nStdDev: {std_dev:.3f}°C"
    )
    props = dict(boxstyle='round', facecolor='wheat', alpha=0.5)
    ax.text(0.02, 0.95, stats_text, transform=ax.transAxes, fontsize=12, verticalalignment='top', bbox=props, family='monospace')

    fig.tight_layout()
    safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
    filepath = os.path.join(PLOTS_DIR, f"{TIMESTAMP}{safe_title}.png")
    plt.savefig(filepath, dpi=150)
    plt.close(fig)
    print(f"Saved stability plot: {filepath}")

    save_telemetry_json(history, safe_title, diag_header)


def plot_step_response(history: List[Dict[str, Any]], title: str):
    if not history:
        print(f"No data collected for {title}")
        return

    os.makedirs(PLOTS_DIR, exist_ok=True)
    temps = [d['t'] for d in history]
    targets = [d['tt'] for d in history]
    t0 = history[0]['ms']
    times = [(d['ms'] - t0) / 1000.0 for d in history]

    cutoff = times[-1] - 60.0
    last_60_idx = next((i for i, tv in enumerate(times) if tv >= cutoff), 0)
    times_60 = times[last_60_idx:]
    temps_60 = temps[last_60_idx:]
    targets_60 = targets[last_60_idx:]

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(10, 10))

    ax1.plot(times, targets, label="Target Boiler Temp (°C)", linestyle="--", color='grey', alpha=0.7)
    ax1.plot(times, temps, label="Boiler Temp (°C)", color='tab:red', linewidth=2)
    ax1.set_title(f"Step Response: {title.replace('_', ' ')}", fontweight='bold', fontsize=14)
    ax1.set_xlabel("Time (Seconds)", fontweight='bold')
    ax1.set_ylabel("Temperature (°C)", fontweight='bold')
    ax1.grid(True, alpha=0.3)
    ax1.legend(loc='lower right')

    ax2.plot(times_60, targets_60, label="Target Boiler Temp (°C)", linestyle="--", color='grey', alpha=0.7)
    ax2.plot(times_60, temps_60, label="Boiler Temp (°C)", color='tab:red', linewidth=2)
    ax2.set_title("Zoomed View: Last 60 Seconds", fontweight='bold', fontsize=12)
    ax2.set_xlabel("Time (Seconds)", fontweight='bold')
    ax2.set_ylabel("Temperature (°C)", fontweight='bold')
    ax2.grid(True, alpha=0.5)

    if len(targets_60) > 0:
        target_val = targets_60[-1]
        y_min = min(target_val - 2.0, min(temps_60) - 0.5)
        y_max = max(target_val + 2.0, max(temps_60) + 0.5)
        ax2.set_ylim(y_min, y_max)

    ax2.legend(loc='lower right')
    fig.tight_layout()

    safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
    filepath = os.path.join(PLOTS_DIR, f"{TIMESTAMP}{safe_title}.png")
    plt.savefig(filepath, dpi=150)
    plt.close(fig)
    print(f"Saved step response plot: {filepath}")


def plot_pressure_step_response(history: List[Dict[str, Any]], title: str):
    if not history:
        print(f"No data collected for {title}")
        return

    os.makedirs(PLOTS_DIR, exist_ok=True)
    pressures = [d.get('p', 0) for d in history]
    targets = [d.get('tp', 0) for d in history]
    t0 = history[0]['ms']
    times = [(d['ms'] - t0) / 1000.0 for d in history]

    fig, ax = plt.subplots(figsize=(10, 5))
    ax.plot(times, targets, label="Target Pressure (Bar)", linestyle="--", color='grey', alpha=0.7)
    ax.plot(times, pressures, label="Actual Pressure (Bar)", color='tab:blue', linewidth=2)
    ax.set_title(f"Step Response: {title.replace('_', ' ')}", fontweight='bold', fontsize=14)
    ax.set_xlabel("Time (Seconds)", fontweight='bold')
    ax.set_ylabel("Pressure (Bar)", fontweight='bold')
    ax.grid(True, alpha=0.3)
    ax.legend(loc='lower right')
    ax.set_ylim(bottom=0)

    fig.tight_layout()
    safe_title = re.sub(r'[^a-zA-Z0-9_\-]', '_', title)
    filepath = os.path.join(PLOTS_DIR, f"{TIMESTAMP}{safe_title}.png")
    plt.savefig(filepath, dpi=150)
    plt.close(fig)
    print(f"Saved pressure step response plot: {filepath}")


# ==========================================
# BASE TEST CASE CLASS
# ==========================================

class OximiteTestCase(unittest.TestCase):
    """Base class for tests with shared helpers."""

    diag_client: Optional[TcpDiagClient] = None

    @classmethod
    def setUpClass(cls):
        os.makedirs(PLOTS_DIR, exist_ok=True)

    def wait_for_state_http(self, target_state: Union[int, List[int], Tuple[int, ...]], timeout: float = 10.0, poll_interval: float = 0.1) -> bool:
        """Polls GET /api/telemetry until machine reaches target_state (int or list/tuple of valid target states)."""
        targets = [target_state] if isinstance(target_state, int) else list(target_state)
        start = time.time()
        while time.time() - start < timeout:
            try:
                telem = get_telemetry_http(timeout=1.5)
                if telem.get('st') in targets:
                    return True
            except Exception:
                pass
            time.sleep(poll_interval)
        return False

    def ensure_idle_state(self, timeout: float = 5.0) -> bool:
        """Sends stop command and waits for state to become IDLE (0)."""
        http_post_cmd("stop")
        return self.wait_for_state_http(STATE_IDLE, timeout=timeout)
