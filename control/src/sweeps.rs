//! Automated bench tuning sweeps replacing legacy Python tuning scripts.

use std::time::{Duration, Instant};

use oximite_proto::model::{BrewProfile, BrewProfileStep, MachineSettings, PidSettings};

use crate::client::ClientHandle;
use crate::store::{compute_temp_stats, TelemetrySample, TempStats};

#[derive(Clone, Debug, PartialEq)]
pub enum SweepConfig {
    TempStep {
        base_temp: f32,
        target_temp: f32,
        cooldown_s: u64,
        record_s: u64,
        pid: PidSettings,
    },
    PressureStep {
        low_bar: f32,
        high_bar: f32,
        step1_s: f32,
        step2_s: f32,
        pid: PidSettings,
    },
    PumpPowerCurve {
        step_pct: u8,
        hold_s: f32,
    },
    BoilerStability {
        duration_s: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PumpCurvePoint {
    pub power_pct: u8,
    pub mean_flow_ml_s: f32,
    pub mean_pressure_bar: f32,
}

#[derive(Clone, Debug)]
pub enum SweepResult {
    TempStep(TempStats),
    PressureStep { samples: usize, max_bar: f32 },
    PumpCurve(Vec<PumpCurvePoint>),
    Stability(TempStats),
}

enum Phase {
    TempCool {
        until: Instant,
        orig_machine: MachineSettings,
    },
    TempHeat {
        until: Instant,
        orig_machine: MachineSettings,
    },
    WaitProfileFinish {
        started: Instant,
        max_wait: Duration,
    },
    PumpHold {
        current_pct: u8,
        until: Instant,
        settle_at: Instant,
        points: Vec<PumpCurvePoint>,
    },
    StabilityHold {
        until: Instant,
    },
}

pub struct SweepRunner {
    config: SweepConfig,
    phase: Phase,
    started_at: Instant,
    sample_start_idx: usize,
    pub status: String,
    pub progress: f32,
    pub result: Option<SweepResult>,
    pub finished: bool,
}

impl SweepRunner {
    pub fn start(
        config: SweepConfig,
        client: &ClientHandle,
        current_machine: MachineSettings,
        current_sample_count: usize,
    ) -> Self {
        let now = Instant::now();
        let (phase, status, sample_start_idx) = match &config {
            SweepConfig::TempStep {
                base_temp,
                cooldown_s,
                pid,
                ..
            } => {
                let mut m = current_machine;
                m.brew_temp = *base_temp;
                let snap_pump = client
                    .store()
                    .lock()
                    .ok()
                    .and_then(|s| s.settings.as_ref().map(|x| x.pump_pid))
                    .unwrap_or(PidSettings {
                        kp: 12.0,
                        ki: 36.0,
                        kd: 0.0,
                    });
                client.send_save_pids(*pid, snap_pump);
                client.send_save_machine(m);
                client.send_session_temp(*base_temp);
                client.send_simple("stop");
                (
                    Phase::TempCool {
                        until: now + Duration::from_secs(*cooldown_s),
                        orig_machine: current_machine,
                    },
                    format!("Cooling/holding at {base_temp:.1} C..."),
                    current_sample_count,
                )
            }
            SweepConfig::PressureStep {
                low_bar,
                high_bar,
                step1_s,
                step2_s,
                pid,
            } => {
                let snap_temp = client
                    .store()
                    .lock()
                    .ok()
                    .and_then(|s| s.settings.as_ref().map(|x| x.temp_pid))
                    .unwrap_or(PidSettings {
                        kp: 8.0,
                        ki: 0.8,
                        kd: 20.0,
                    });
                client.send_save_pids(snap_temp, *pid);
                client.send_simple("stop");
                let mut steps = oximite_proto::heapless::Vec::new();
                let _ = steps.push(BrewProfileStep {
                    time_s: Some(*step1_s),
                    volume: None,
                    pressure: Some(*low_bar),
                    flow: None,
                });
                let _ = steps.push(BrewProfileStep {
                    time_s: Some(*step2_s),
                    volume: None,
                    pressure: Some(*high_bar),
                    flow: None,
                });
                let prof = BrewProfile {
                    name: oximite_proto::heapless::String::try_from("PressureSweep").unwrap(),
                    steps,
                };
                client.send_run_profile(&prof);
                (
                    Phase::WaitProfileFinish {
                        started: now,
                        max_wait: Duration::from_secs_f32(*step1_s + *step2_s + 5.0),
                    },
                    "Running pressure step profile...".into(),
                    current_sample_count,
                )
            }
            SweepConfig::PumpPowerCurve { hold_s, .. } => {
                client.send_simple("stop");
                client.send_direct_pump(0.0);
                (
                    Phase::PumpHold {
                        current_pct: 0,
                        until: now + Duration::from_secs_f32(*hold_s),
                        settle_at: now + Duration::from_secs_f32((*hold_s * 0.5).max(0.5)),
                        points: Vec::new(),
                    },
                    "Pump power step 0%...".into(),
                    current_sample_count,
                )
            }
            SweepConfig::BoilerStability { duration_s } => {
                client.send_simple("stop");
                (
                    Phase::StabilityHold {
                        until: now + Duration::from_secs(*duration_s),
                    },
                    format!("Recording stability for {duration_s}s..."),
                    current_sample_count,
                )
            }
        };

        Self {
            config,
            phase,
            started_at: now,
            sample_start_idx,
            status,
            progress: 0.0,
            result: None,
            finished: false,
        }
    }

    pub fn abort(&mut self, client: &ClientHandle) {
        if !self.finished {
            client.send_simple("stop");
            self.status = "Aborted by user".into();
            self.finished = true;
        }
    }

    pub fn tick(&mut self, client: &ClientHandle, samples: &[TelemetrySample]) {
        if self.finished {
            return;
        }
        let now = Instant::now();
        let slice = if self.sample_start_idx <= samples.len() {
            &samples[self.sample_start_idx..]
        } else {
            &[]
        };

        match (&self.config, &mut self.phase) {
            (
                SweepConfig::TempStep {
                    target_temp,
                    cooldown_s,
                    record_s,
                    ..
                },
                Phase::TempCool {
                    until,
                    orig_machine,
                },
            ) => {
                let total = (*cooldown_s + *record_s).max(1) as f32;
                self.progress = (self.started_at.elapsed().as_secs_f32() / total).min(0.99);
                if now >= *until {
                    let mut m = *orig_machine;
                    m.brew_temp = *target_temp;
                    client.send_save_machine(m);
                    client.send_session_temp(*target_temp);
                    self.sample_start_idx = samples.len();
                    self.status =
                        format!("Step to {target_temp:.1} C: recording for {record_s}s...");
                    self.phase = Phase::TempHeat {
                        until: now + Duration::from_secs(*record_s),
                        orig_machine: *orig_machine,
                    };
                }
            }
            (
                SweepConfig::TempStep {
                    cooldown_s,
                    record_s,
                    ..
                },
                Phase::TempHeat {
                    until,
                    orig_machine,
                },
            ) => {
                let total = (*cooldown_s + *record_s).max(1) as f32;
                self.progress = (self.started_at.elapsed().as_secs_f32() / total).min(0.99);
                if now >= *until {
                    let refs: Vec<&TelemetrySample> = slice.iter().collect();
                    self.result = compute_temp_stats(&refs).map(SweepResult::TempStep);
                    client.send_save_machine(*orig_machine);
                    client.send_session_temp(orig_machine.brew_temp);
                    self.progress = 1.0;
                    self.status = "Temperature step response complete.".into();
                    self.finished = true;
                }
            }
            (SweepConfig::PressureStep { .. }, Phase::WaitProfileFinish { started, max_wait }) => {
                self.progress =
                    (started.elapsed().as_secs_f32() / max_wait.as_secs_f32()).min(0.99);
                let idle_after_run = started.elapsed() > Duration::from_secs(1)
                    && slice.last().is_some_and(|s| s.frame.st == 0);
                if idle_after_run || started.elapsed() >= *max_wait {
                    client.send_simple("stop");
                    let max_bar = slice.iter().map(|s| s.frame.p).fold(0.0f32, f32::max);
                    self.result = Some(SweepResult::PressureStep {
                        samples: slice.len(),
                        max_bar,
                    });
                    self.progress = 1.0;
                    self.status = format!("Pressure step complete (peak {max_bar:.2} bar).");
                    self.finished = true;
                }
            }
            (
                SweepConfig::PumpPowerCurve { step_pct, hold_s },
                Phase::PumpHold {
                    current_pct,
                    until,
                    settle_at,
                    points,
                },
            ) => {
                self.progress = (*current_pct as f32 / 100.0).min(0.99);
                if now >= *until {
                    let settled: Vec<&TelemetrySample> =
                        slice.iter().skip(slice.len() / 2).collect();
                    let n = settled.len().max(1) as f32;
                    let mean_flow = settled.iter().map(|s| s.frame.fl).sum::<f32>() / n;
                    let mean_p = settled.iter().map(|s| s.frame.p).sum::<f32>() / n;
                    points.push(PumpCurvePoint {
                        power_pct: *current_pct,
                        mean_flow_ml_s: mean_flow,
                        mean_pressure_bar: mean_p,
                    });

                    let next = current_pct.saturating_add(*step_pct);
                    client.send_simple("stop");
                    if next > 100 || *current_pct == 100 {
                        let out = std::mem::take(points);
                        self.result = Some(SweepResult::PumpCurve(out));
                        self.progress = 1.0;
                        self.status = "Pump power curve sweep complete.".into();
                        self.finished = true;
                    } else {
                        let pct = next.min(100);
                        client.send_direct_pump(f32::from(pct));
                        *current_pct = pct;
                        self.sample_start_idx = samples.len();
                        *until = now + Duration::from_secs_f32(*hold_s);
                        *settle_at = now + Duration::from_secs_f32((*hold_s * 0.5).max(0.5));
                        self.status = format!("Pump power step {pct}%...");
                    }
                } else {
                    let _ = settle_at;
                }
            }
            (SweepConfig::BoilerStability { duration_s }, Phase::StabilityHold { until }) => {
                let total = (*duration_s).max(1) as f32;
                self.progress = (self.started_at.elapsed().as_secs_f32() / total).min(0.99);
                if now >= *until {
                    let refs: Vec<&TelemetrySample> = slice.iter().collect();
                    self.result = compute_temp_stats(&refs).map(SweepResult::Stability);
                    self.progress = 1.0;
                    self.status = "Boiler stability run complete.".into();
                    self.finished = true;
                }
            }
            _ => {}
        }
    }
}
