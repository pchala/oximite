//! Thread-safe telemetry buffer, shot capture, JSONL/CSV I/O, and event log.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Local};
use oximite_proto::model::{BrewProfile, MachineState, ProfileHeaderOwned, PublicSettings};
use oximite_proto::protocol::{DeviceEvent, TelemetryFrame, WireTick};
use serde::{Deserialize, Serialize};

pub const MAX_SAMPLES: usize = 90_000; // 30 min at 50 Hz
pub const MAX_LOGS: usize = 2_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelemetrySample {
    pub t_s: f64,
    pub frame: TelemetryFrame,
}

#[derive(Clone, Debug)]
pub struct ShotRecord {
    pub id: usize,
    pub label: String,
    pub samples: Vec<TelemetrySample>,
}

#[derive(Clone, Debug)]
pub struct LoadedTrace {
    pub name: String,
    pub samples: Vec<TelemetrySample>,
    pub offset_s: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogTag {
    Cmd,
    State,
    Step,
    Settings,
    Net,
}

impl LogTag {
    pub fn as_str(self) -> &'static str {
        match self {
            LogTag::Cmd => "CMD",
            LogTag::State => "STATE",
            LogTag::Step => "STEP",
            LogTag::Settings => "CFG",
            LogTag::Net => "NET",
        }
    }
}

#[derive(Clone, Debug)]
pub struct EventLogEntry {
    pub timestamp: DateTime<Local>,
    pub tag: LogTag,
    pub summary: String,
    pub raw: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempStats {
    pub count: usize,
    pub mean: f32,
    pub min: f32,
    pub max: f32,
    pub std_dev: f32,
}

pub fn compute_temp_stats(samples: &[&TelemetrySample]) -> Option<TempStats> {
    if samples.is_empty() {
        return None;
    }
    let mut sum = 0.0f64;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for s in samples {
        let t = s.frame.t;
        sum += f64::from(t);
        if t < min {
            min = t;
        }
        if t > max {
            max = t;
        }
    }
    let mean = sum / samples.len() as f64;
    let var: f64 = samples
        .iter()
        .map(|s| {
            let d = f64::from(s.frame.t) - mean;
            d * d
        })
        .sum::<f64>()
        / samples.len() as f64;
    Some(TempStats {
        count: samples.len(),
        mean: mean as f32,
        min,
        max,
        std_dev: var.sqrt() as f32,
    })
}

pub struct ControlStore {
    pub connected: bool,
    pub connecting: bool,
    pub endpoint: String,
    pub last_error: Option<String>,
    pub settings: Option<PublicSettings>,
    pub profiles: Vec<ProfileHeaderOwned>,
    pub loaded_profile: Option<(u8, BrewProfile)>,
    pub current_state: MachineState,
    pub session_brew_temp: f32,
    pub samples: VecDeque<TelemetrySample>,
    pub shots: Vec<ShotRecord>,
    pub comparison: [Option<LoadedTrace>; 2],
    pub logs: VecDeque<EventLogEntry>,
    pub log_seq: u64,
    pub dropped_frames: u64,
    pub rx_hz: f32,
    pub last_ping_rtt_ms: Option<f32>,
    pub latest: Option<TelemetrySample>,
    /// Directory for per-shot JSONL files. `None` disables autosave.
    pub autosave_dir: Option<PathBuf>,
    session_start: Instant,
    base_device_ms: Option<u32>,
    last_seq: Option<u32>,
    rate_window_start: Instant,
    rate_window_frames: u32,
    active_shot: Option<Vec<TelemetrySample>>,
    last_st: u8,
    wire_seq: Option<u8>,
    wire_ext: u32,
    wire_ms: u32,
}

impl Default for ControlStore {
    fn default() -> Self {
        Self::new("192.168.1.117:8080")
    }
}

impl ControlStore {
    pub fn new(endpoint: &str) -> Self {
        let now = Instant::now();
        Self {
            connected: false,
            connecting: false,
            endpoint: endpoint.to_string(),
            last_error: None,
            settings: None,
            profiles: Vec::new(),
            loaded_profile: None,
            current_state: MachineState::Idle,
            session_brew_temp: 92.0,
            samples: VecDeque::with_capacity(4096),
            shots: Vec::new(),
            comparison: [None, None],
            logs: VecDeque::with_capacity(MAX_LOGS),
            log_seq: 0,
            dropped_frames: 0,
            rx_hz: 0.0,
            last_ping_rtt_ms: None,
            latest: None,
            autosave_dir: None,
            session_start: now,
            base_device_ms: None,
            last_seq: None,
            rate_window_start: now,
            rate_window_frames: 0,
            active_shot: None,
            last_st: MachineState::Idle as u8,
            wire_seq: None,
            wire_ext: 0,
            wire_ms: 0,
        }
    }

    pub fn add_log(&mut self, tag: LogTag, summary: impl Into<String>, raw: impl Into<String>) {
        if self.logs.len() >= MAX_LOGS {
            self.logs.pop_front();
        }
        self.logs.push_back(EventLogEntry {
            timestamp: Local::now(),
            tag,
            summary: summary.into(),
            raw: raw.into(),
        });
        self.log_seq = self.log_seq.wrapping_add(1);
    }

    /// Samples of the brew in progress, or of the last finished brew.
    pub fn brew_samples(&self) -> Vec<TelemetrySample> {
        match (&self.active_shot, self.shots.last()) {
            (Some(active), _) => active.clone(),
            (None, Some(last)) => last.samples.clone(),
            (None, None) => Vec::new(),
        }
    }

    fn autosave_shot(&mut self, samples: &[TelemetrySample]) {
        let Some(dir) = self.autosave_dir.clone() else {
            return;
        };
        let path = dir.join(shot_file_name(Local::now()));
        let result = std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))
            .and_then(|()| export_samples_jsonl(samples, &path));
        match result {
            Ok(n) => self.add_log(
                LogTag::Settings,
                format!("Saved {n} samples to {}", path.display()),
                String::new(),
            ),
            Err(e) => self.add_log(LogTag::Settings, format!("Autosave failed: {e}"), e),
        }
    }

    pub fn clear_samples(&mut self) {
        self.samples.clear();
        self.base_device_ms = None;
        self.session_start = Instant::now();
    }

    pub fn clear_logs(&mut self) {
        self.logs.clear();
        self.log_seq = self.log_seq.wrapping_add(1);
    }

    /// Resets stream state when the Port 8080 socket opens.
    pub fn on_connected(&mut self) {
        self.base_device_ms = None;
        self.last_seq = None;
        self.wire_seq = None;
        self.add_log(LogTag::Net, "Telemetry stream connected", String::new());
    }

    /// Unwraps the 8-bit sequence and sums tick periods into device time.
    pub fn on_wire_tick(&mut self, tick: WireTick) {
        let dt_ms = u32::from(tick.dt_us) / 1000;
        let missed = match self.wire_seq {
            Some(prev) => u32::from(tick.seq.wrapping_sub(prev).wrapping_sub(1)),
            None => 0,
        };
        self.wire_seq = Some(tick.seq);
        self.wire_ext = self.wire_ext.wrapping_add(missed + 1);
        self.wire_ms = self.wire_ms.wrapping_add(dt_ms * (missed + 1));
        let frame = tick.to_frame(self.wire_ext, self.wire_ms);
        self.on_tick(frame);
    }

    pub fn on_tick(&mut self, frame: TelemetryFrame) {
        if let Some(prev) = self.last_seq {
            let expected = prev.wrapping_add(1);
            if frame.seq != expected {
                let gap = frame.seq.wrapping_sub(expected);
                if gap < 10_000 {
                    self.dropped_frames += u64::from(gap);
                }
            }
        }
        self.last_seq = Some(frame.seq);

        self.rate_window_frames += 1;
        let dt_rate = self.rate_window_start.elapsed().as_secs_f32();
        if dt_rate >= 1.0 {
            self.rx_hz = self.rate_window_frames as f32 / dt_rate;
            self.rate_window_frames = 0;
            self.rate_window_start = Instant::now();
        }

        let t_s = match self.base_device_ms {
            Some(base) => f64::from(frame.ms.wrapping_sub(base)) / 1000.0,
            None => {
                self.base_device_ms = Some(frame.ms);
                self.session_start.elapsed().as_secs_f64()
            }
        };

        let sample = TelemetrySample { t_s, frame };
        self.current_state = MachineState::try_from(frame.st).unwrap_or(MachineState::Idle);
        self.session_brew_temp = frame.sbt;
        self.latest = Some(sample.clone());

        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample.clone());

        // Auto-capture shots when entering Brewing (1) or Pumping (4)
        let is_shot_state = matches!(frame.st, 1 | 4);
        let was_shot_state = matches!(self.last_st, 1 | 4);
        if is_shot_state {
            let buf = self.active_shot.get_or_insert_with(Vec::new);
            buf.push(sample);
        } else if was_shot_state {
            if let Some(captured) = self.active_shot.take() {
                if !captured.is_empty() {
                    let id = self.shots.len() + 1;
                    let dur = captured.last().unwrap().t_s - captured.first().unwrap().t_s;
                    let vol = captured.last().unwrap().frame.vol;
                    let label = format!("Shot #{id} ({dur:.1}s, {vol:.1}ml)");
                    self.autosave_shot(&captured);
                    self.shots.push(ShotRecord {
                        id,
                        label,
                        samples: captured,
                    });
                }
            }
        }
        self.last_st = frame.st;
    }

    pub fn on_event(&mut self, ev: DeviceEvent) {
        match ev {
            DeviceEvent::StateChanged { from, to } => {
                let f = MachineState::try_from(from)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                let t = MachineState::try_from(to)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                self.add_log(LogTag::State, format!("{f} -> {t}"), String::new());
            }
            DeviceEvent::StepChanged { index, reason } => {
                self.add_log(
                    LogTag::Step,
                    format!("Step {index}: {reason:?}"),
                    String::new(),
                );
            }
            DeviceEvent::SettingsUpdated => {
                self.add_log(LogTag::Settings, "Settings updated in RAM", String::new());
            }
            DeviceEvent::ProfilesUpdated => {
                self.add_log(LogTag::Settings, "Profiles updated in RAM", String::new());
            }
            DeviceEvent::FlashSaved => {
                self.add_log(
                    LogTag::Settings,
                    "Changes persisted to flash",
                    String::new(),
                );
            }
        }
    }
}

pub fn export_samples_jsonl(samples: &[TelemetrySample], path: &Path) -> Result<usize, String> {
    let file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut w = BufWriter::new(file);
    for s in samples {
        serde_json::to_writer(&mut w, s).map_err(|e| e.to_string())?;
        w.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(samples.len())
}

pub fn export_samples_csv(samples: &[TelemetrySample], path: &Path) -> Result<usize, String> {
    let file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut w = BufWriter::new(file);
    writeln!(w, "t_s,seq,ms,t,tt,p,tp,fl,tfl,vol,hp,pump,sbt,fc,st,step")
        .map_err(|e| e.to_string())?;
    for s in samples {
        let f = &s.frame;
        writeln!(
            w,
            "{:.4},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2},{:.2},{},{},{}",
            s.t_s,
            f.seq,
            f.ms,
            f.t,
            f.tt,
            f.p,
            f.tp,
            f.fl,
            f.tfl,
            f.vol,
            f.hp,
            f.pump,
            f.sbt,
            f.fc,
            f.st,
            f.step
        )
        .map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(samples.len())
}

/// Names a shot file `year_month_day_hh_mm.jsonl`.
pub fn shot_file_name(now: DateTime<Local>) -> String {
    now.format("%Y_%m_%d_%H_%M.jsonl").to_string()
}

pub fn import_samples_jsonl(path: &Path) -> Result<LoadedTrace, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut samples = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(s) = serde_json::from_str::<TelemetrySample>(&line) {
            samples.push(s);
        }
    }
    if samples.is_empty() {
        return Err("No valid TelemetrySample rows found".into());
    }
    let t0 = samples[0].t_s;
    for s in &mut samples {
        s.t_s -= t0;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "trace.jsonl".into());
    Ok(LoadedTrace {
        name,
        samples,
        offset_s: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_frame(seq: u32, ms: u32, t: f32, st: u8, vol: f32) -> TelemetryFrame {
        TelemetryFrame {
            seq,
            ms,
            t,
            tt: 92.0,
            p: 9.0,
            tp: 9.0,
            fl: 2.0,
            tfl: 2.5,
            vol,
            hp: 20.0,
            pump: 50.0,
            sbt: 92.0,
            fc: 0,
            st,
            step: 0,
        }
    }

    #[test]
    fn captures_shots_and_counts_dropped_frames() {
        let mut store = ControlStore::new("127.0.0.1:8080");
        store.on_tick(mk_frame(10, 1000, 92.0, 0, 0.0));
        store.on_tick(mk_frame(11, 1020, 92.5, 1, 1.0));
        // Skip seq 12 -> 1 dropped frame
        store.on_tick(mk_frame(13, 1060, 93.0, 1, 3.0));
        store.on_tick(mk_frame(14, 1080, 92.5, 0, 3.0));

        assert_eq!(store.dropped_frames, 1);
        assert_eq!(store.shots.len(), 1);
        assert_eq!(store.shots[0].samples.len(), 2);
    }

    #[test]
    fn shot_file_name_format() {
        use chrono::TimeZone;
        let t = Local.with_ymd_and_hms(2026, 3, 7, 9, 5, 0).unwrap();
        assert_eq!(shot_file_name(t), "2026_03_07_09_05.jsonl");
    }

    #[test]
    fn brew_samples_follow_active_then_last_shot() {
        let mut store = ControlStore::new("127.0.0.1:8080");
        assert!(store.brew_samples().is_empty());
        store.on_tick(mk_frame(1, 1000, 92.0, 0, 0.0));
        assert!(store.brew_samples().is_empty());
        store.on_tick(mk_frame(2, 1020, 92.0, 1, 1.0));
        store.on_tick(mk_frame(3, 1040, 92.0, 1, 2.0));
        assert_eq!(store.brew_samples().len(), 2);
        store.on_tick(mk_frame(4, 1060, 92.0, 0, 2.0));
        assert_eq!(store.brew_samples().len(), 2);
        store.on_tick(mk_frame(5, 1080, 92.0, 0, 0.0));
        assert_eq!(store.brew_samples().len(), 2);
    }

    #[test]
    fn brew_end_autosaves_jsonl() {
        let dir = std::env::temp_dir().join("oximite_autosave_test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = ControlStore::new("127.0.0.1:8080");
        store.autosave_dir = Some(dir.clone());
        store.on_tick(mk_frame(1, 1000, 92.0, 1, 1.0));
        store.on_tick(mk_frame(2, 1020, 92.0, 0, 1.0));
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().to_string_lossy().into_owned();
        assert!(name.ends_with(".jsonl") && name.len() == "2026_03_07_09_05.jsonl".len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonl_roundtrip_and_stats() {
        let samples = vec![
            TelemetrySample {
                t_s: 10.0,
                frame: mk_frame(1, 100, 92.0, 0, 0.0),
            },
            TelemetrySample {
                t_s: 10.5,
                frame: mk_frame(2, 600, 94.0, 0, 0.0),
            },
        ];
        let tmp = std::env::temp_dir().join("oximite_test_trace.jsonl");
        assert_eq!(export_samples_jsonl(&samples, &tmp).unwrap(), 2);
        let loaded = import_samples_jsonl(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(loaded.samples[0].t_s, 0.0);
        assert!((loaded.samples[1].t_s - 0.5).abs() < 1e-6);

        let refs: Vec<&TelemetrySample> = samples.iter().collect();
        let st = compute_temp_stats(&refs).unwrap();
        assert!((st.mean - 93.0).abs() < 1e-4);
        assert!((st.std_dev - 1.0).abs() < 1e-4);
    }
}
