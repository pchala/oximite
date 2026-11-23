//! Pure machine state machine, operation stepper, and deferred write buffer.
//!
//! Unifies the state transition logic and step execution without any Embassy,
//! GPIO, or clock dependencies so every branch can be deterministically tested
//! and fuzzed on the host.

use oximite_proto::model::{
    default_profile, Ambient, BrewProfile, ControlSettings, MachineCommand, MachineSettings,
    MachineState, PidSettings, PublicSettings, PumpMode, Settings, WifiSettings, MAX_PROFILES,
    STEP_NONE,
};
use oximite_proto::protocol::{DeviceEvent, StepReason};

use crate::controller::ControlIntent;

pub const PUMP_POWER: f32 = 80.0;
pub const MAX_PUMP_RUN_MS: u64 = 60_000;
pub const MAX_HOT_WATER_MS: u64 = 120_000;
pub const STEP_SAFETY_MS: u64 = 120_000;
pub const DIRECT_PUMP_BAR: f32 = 10.0;

pub const DIRTY_MACHINE: u16 = 1 << 0;
pub const DIRTY_PIDS: u16 = 1 << 1;
pub const DIRTY_WIFI: u16 = 1 << 2;

pub const fn dirty_profile(slot: u8) -> u16 {
    if slot < MAX_PROFILES {
        1 << (3 + slot)
    } else {
        0
    }
}

pub const fn is_machine_dirty(mask: u16) -> bool {
    mask & DIRTY_MACHINE != 0
}

pub const fn is_pids_dirty(mask: u16) -> bool {
    mask & DIRTY_PIDS != 0
}

pub const fn is_wifi_dirty(mask: u16) -> bool {
    mask & DIRTY_WIFI != 0
}

pub const fn is_profile_dirty(mask: u16, slot: u8) -> bool {
    mask & dirty_profile(slot) != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetTempMode {
    Brew,
    Steam,
    Off,
}

pub fn target_for(state: MachineState) -> TargetTempMode {
    match state {
        MachineState::Steaming => TargetTempMode::Steam,
        MachineState::Cooling | MachineState::Sleeping => TargetTempMode::Off,
        MachineState::Idle
        | MachineState::Brewing
        | MachineState::Pumping
        | MachineState::HotWater => TargetTempMode::Brew,
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum PendingProfileOp {
    Save(BrewProfile),
    Delete,
}

/// Per-section deferred RAM + flash writes held while the machine is busy.
///
/// Distinct sections coalesce independently without overwriting each other,
/// and both RAM and flash are updated only once the machine leaves busy states.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeferredWrites {
    pub machine: Option<MachineSettings>,
    pub pids: Option<(PidSettings, PidSettings)>,
    pub wifi: Option<WifiSettings>,
    pub profiles: [Option<PendingProfileOp>; MAX_PROFILES as usize],
}

impl DeferredWrites {
    #[cfg(test)]
    pub fn has_any(&self) -> bool {
        self.machine.is_some()
            || self.pids.is_some()
            || self.wifi.is_some()
            || self.profiles.iter().any(Option::is_some)
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ActiveOperation {
    Profile {
        profile: BrewProfile,
        step_idx: usize,
        step_started_ms: u64,
    },
    Steam {
        started_ms: u64,
        limit_ms: u64,
    },
    CooldownFlush {
        started_ms: u64,
        stop_at: f32,
    },
    DirectPump {
        power: f32,
        started_ms: u64,
    },
    HotWater {
        started_ms: u64,
    },
}

impl ActiveOperation {
    pub fn opens_valve(&self) -> bool {
        matches!(
            self,
            ActiveOperation::Profile { .. } | ActiveOperation::DirectPump { .. }
        )
    }

    pub fn brew_active(&self) -> bool {
        matches!(self, ActiveOperation::Profile { .. })
    }

    pub fn step_idx(&self) -> u8 {
        match self {
            ActiveOperation::Profile {
                profile, step_idx, ..
            } if *step_idx < profile.steps.len() => *step_idx as u8,
            _ => STEP_NONE,
        }
    }

    pub fn pump_mode(&self) -> PumpMode {
        match self {
            ActiveOperation::Profile {
                profile, step_idx, ..
            } => match profile.steps.get(*step_idx) {
                Some(step) => {
                    let pressure = step.pressure.unwrap_or(0.0);
                    let flow = step.flow.unwrap_or(0.0);
                    if pressure >= DIRECT_PUMP_BAR {
                        PumpMode::DirectPump(pressure)
                    } else {
                        PumpMode::Unified {
                            target_bar: pressure,
                            target_ml_s: flow,
                        }
                    }
                }
                None => PumpMode::Idle,
            },
            ActiveOperation::Steam { .. } => PumpMode::Idle,
            ActiveOperation::CooldownFlush { .. } | ActiveOperation::HotWater { .. } => {
                PumpMode::DirectPump(PUMP_POWER)
            }
            ActiveOperation::DirectPump { power, .. } => PumpMode::DirectPump(*power),
        }
    }

    fn advance_profile_to_runnable(&mut self, now_ms: u64) -> Option<u8> {
        let ActiveOperation::Profile {
            profile,
            step_idx,
            step_started_ms,
        } = self
        else {
            return None;
        };
        while *step_idx < profile.steps.len() {
            let s = &profile.steps[*step_idx];
            let time_s = s.time_s.unwrap_or(0.0);
            let volume = s.volume.unwrap_or(0.0);
            if time_s > 0.0 || volume > 0.0 {
                *step_started_ms = now_ms;
                return Some(*step_idx as u8);
            }
            *step_idx += 1;
        }
        None
    }

    /// Advances active operation by current timestamp and sensors.
    /// Returns `true` when the operation completed naturally.
    fn tick(
        &mut self,
        now_ms: u64,
        volume_ml: f32,
        temp_c: f32,
        out: &mut MachineStepEffects,
    ) -> bool {
        match self {
            ActiveOperation::Profile {
                profile,
                step_idx,
                step_started_ms,
            } => {
                let Some(step) = profile.steps.get(*step_idx) else {
                    out.push_event(DeviceEvent::StepChanged {
                        index: STEP_NONE,
                        reason: StepReason::Completed,
                    });
                    return true;
                };

                let time_s = step.time_s.unwrap_or(0.0);
                let volume = step.volume.unwrap_or(0.0);
                let elapsed_ms = now_ms.saturating_sub(*step_started_ms);

                let finished_reason = if time_s > 0.0 && elapsed_ms >= (time_s * 1000.0) as u64 {
                    Some(StepReason::TimeLimit)
                } else if volume > 0.0 && volume_ml >= volume {
                    Some(StepReason::VolumeLimit)
                } else if elapsed_ms >= STEP_SAFETY_MS {
                    Some(StepReason::SafetyTimeout)
                } else {
                    None
                };

                let Some(reason) = finished_reason else {
                    return false;
                };

                let finished_idx = *step_idx as u8;
                out.push_event(DeviceEvent::StepChanged {
                    index: finished_idx,
                    reason,
                });
                *step_idx += 1;

                if let Some(next_idx) = self.advance_profile_to_runnable(now_ms) {
                    out.push_event(DeviceEvent::StepChanged {
                        index: next_idx,
                        reason: StepReason::Started,
                    });
                    false
                } else {
                    out.push_event(DeviceEvent::StepChanged {
                        index: STEP_NONE,
                        reason: StepReason::Completed,
                    });
                    true
                }
            }
            ActiveOperation::Steam {
                started_ms,
                limit_ms,
            } => now_ms.saturating_sub(*started_ms) >= *limit_ms,
            ActiveOperation::CooldownFlush {
                started_ms,
                stop_at,
            } => temp_c <= *stop_at || now_ms.saturating_sub(*started_ms) >= MAX_PUMP_RUN_MS,
            ActiveOperation::DirectPump { started_ms, .. } => {
                now_ms.saturating_sub(*started_ms) >= MAX_PUMP_RUN_MS
            }
            ActiveOperation::HotWater { started_ms } => {
                now_ms.saturating_sub(*started_ms) >= MAX_HOT_WATER_MS
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MachineStepEffects {
    pub dirty_flash: u16,
    pub reset_volume: bool,
    pub settings_changed: bool,
    pub wifi_changed: bool,
    pub profiles_changed: bool,
    pub events: heapless::Vec<DeviceEvent, 8>,
}

impl MachineStepEffects {
    fn push_event(&mut self, ev: DeviceEvent) {
        let _ = self.events.push(ev);
    }
}

pub struct Machine {
    asleep: bool,
    settings: Settings,
    profiles: [Option<BrewProfile>; MAX_PROFILES as usize],
    session_brew_temp: f32,
    last_activity_ms: u64,
    op: Option<ActiveOperation>,
    deferred: DeferredWrites,
    settings_rev: u32,
    wifi_rev: u32,
    profiles_rev: u32,
}

impl Machine {
    pub fn new(
        settings: Settings,
        profiles: [Option<BrewProfile>; MAX_PROFILES as usize],
        now_ms: u64,
    ) -> Self {
        let session_brew_temp = settings.machine.brew_temp;
        Self {
            asleep: false,
            settings,
            profiles,
            session_brew_temp,
            last_activity_ms: now_ms,
            op: None,
            deferred: DeferredWrites::default(),
            settings_rev: 1,
            wifi_rev: 1,
            profiles_rev: 1,
        }
    }

    /// Derived from the active operation so state and operation cannot diverge.
    pub fn state(&self) -> MachineState {
        match &self.op {
            Some(ActiveOperation::Profile { .. }) => MachineState::Brewing,
            Some(ActiveOperation::Steam { .. }) => MachineState::Steaming,
            Some(ActiveOperation::CooldownFlush { .. }) => MachineState::Cooling,
            Some(ActiveOperation::DirectPump { .. }) => MachineState::Pumping,
            Some(ActiveOperation::HotWater { .. }) => MachineState::HotWater,
            None if self.asleep => MachineState::Sleeping,
            None => MachineState::Idle,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn control_settings(&self) -> ControlSettings {
        ControlSettings::from(&self.settings)
    }

    pub fn public_settings(&self) -> PublicSettings {
        self.settings.to_public()
    }

    pub fn profiles(&self) -> &[Option<BrewProfile>; MAX_PROFILES as usize] {
        &self.profiles
    }

    #[cfg(test)]
    pub fn profile(&self, slot: u8) -> Option<&BrewProfile> {
        self.profiles.get(slot as usize)?.as_ref()
    }

    pub fn session_brew_temp(&self) -> f32 {
        self.session_brew_temp
    }

    pub fn settings_rev(&self) -> u32 {
        self.settings_rev
    }

    pub fn wifi_rev(&self) -> u32 {
        self.wifi_rev
    }

    pub fn profiles_rev(&self) -> u32 {
        self.profiles_rev
    }

    #[cfg(test)]
    pub fn has_deferred_writes(&self) -> bool {
        self.deferred.has_any()
    }

    pub fn valve_open(&self) -> bool {
        self.op.as_ref().is_some_and(ActiveOperation::opens_valve)
    }

    pub fn target_temp_c(&self) -> f32 {
        match target_for(self.state()) {
            TargetTempMode::Brew => self.session_brew_temp + self.settings.machine.temp_offset,
            TargetTempMode::Steam => self.settings.machine.steam_temp,
            TargetTempMode::Off => 0.0,
        }
    }

    pub fn intent(&self) -> ControlIntent {
        ControlIntent {
            target_temp: self.target_temp_c(),
            pump_mode: self
                .op
                .as_ref()
                .map_or(PumpMode::Idle, ActiveOperation::pump_mode),
            brew_active: self.op.as_ref().is_some_and(ActiveOperation::brew_active),
            step_idx: self
                .op
                .as_ref()
                .map_or(STEP_NONE, ActiveOperation::step_idx),
        }
    }

    fn emit_state_change(&self, from: MachineState, out: &mut MachineStepEffects) {
        let to = self.state();
        if from != to {
            out.push_event(DeviceEvent::StateChanged {
                from: from as u8,
                to: to as u8,
            });
        }
    }

    fn start_operation(
        &mut self,
        mut op: ActiveOperation,
        now_ms: u64,
        out: &mut MachineStepEffects,
    ) {
        let from = self.state();
        let first_step = if let ActiveOperation::Profile { .. } = op {
            let Some(first_idx) = op.advance_profile_to_runnable(now_ms) else {
                return;
            };
            Some(first_idx)
        } else {
            None
        };
        out.reset_volume = true;
        self.last_activity_ms = now_ms;
        self.asleep = false;
        self.op = Some(op);
        self.emit_state_change(from, out);
        if let Some(first_idx) = first_step {
            out.push_event(DeviceEvent::StepChanged {
                index: first_idx,
                reason: StepReason::Started,
            });
        }
    }

    fn stop_to_idle(&mut self, now_ms: u64, out: &mut MachineStepEffects) {
        let from = self.state();
        self.op = None;
        self.asleep = false;
        self.last_activity_ms = now_ms;
        self.emit_state_change(from, out);
        self.flush_deferred(out);
    }

    fn flush_deferred(&mut self, out: &mut MachineStepEffects) {
        if self.state().is_busy() {
            return;
        }
        if let Some(m) = self.deferred.machine.take() {
            self.settings.machine = m;
            out.dirty_flash |= DIRTY_MACHINE;
            out.settings_changed = true;
        }
        if let Some((t, p)) = self.deferred.pids.take() {
            self.settings.temp_pid = t;
            self.settings.pump_pid = p;
            out.dirty_flash |= DIRTY_PIDS;
            out.settings_changed = true;
        }
        if let Some(w) = self.deferred.wifi.take() {
            self.settings.wifi = w;
            out.dirty_flash |= DIRTY_WIFI;
            out.settings_changed = true;
            out.wifi_changed = true;
        }
        for slot in 0..MAX_PROFILES {
            if let Some(op) = self.deferred.profiles[slot as usize].take() {
                match op {
                    PendingProfileOp::Save(p) => {
                        self.profiles[slot as usize] = Some(p);
                    }
                    PendingProfileOp::Delete => {
                        self.profiles[slot as usize] = None;
                    }
                }
                out.dirty_flash |= dirty_profile(slot);
                out.profiles_changed = true;
            }
        }
        if out.settings_changed {
            self.settings_rev = self.settings_rev.wrapping_add(1);
            out.push_event(DeviceEvent::SettingsUpdated);
        }
        if out.wifi_changed {
            self.wifi_rev = self.wifi_rev.wrapping_add(1);
        }
        if out.profiles_changed {
            self.profiles_rev = self.profiles_rev.wrapping_add(1);
            out.push_event(DeviceEvent::ProfilesUpdated);
        }
    }

    fn queue_ambient_ram_and_flash(&mut self, cmd: MachineCommand, out: &mut MachineStepEffects) {
        match cmd {
            MachineCommand::SaveMachine(m) => self.deferred.machine = Some(m),
            MachineCommand::SavePids(t, p) => self.deferred.pids = Some((t, p)),
            MachineCommand::SaveWifi(w) => self.deferred.wifi = Some(w),
            MachineCommand::SaveProfile(slot, p) => {
                if slot < MAX_PROFILES {
                    self.deferred.profiles[slot as usize] = Some(PendingProfileOp::Save(p));
                }
            }
            MachineCommand::DeleteProfile(slot) => {
                if slot < MAX_PROFILES {
                    self.deferred.profiles[slot as usize] = Some(PendingProfileOp::Delete);
                }
            }
            _ => return,
        }
        // Decision 6: defer BOTH RAM and flash updates until Idle (not busy).
        if !self.state().is_busy() {
            self.flush_deferred(out);
        }
    }

    pub fn handle_command(&mut self, cmd: MachineCommand, now_ms: u64) -> MachineStepEffects {
        let mut out = MachineStepEffects::default();
        self.last_activity_ms = now_ms;

        match cmd.ambient() {
            Ambient::No => {}
            Ambient::RamOnly => {
                if let MachineCommand::SetSessionTemp(t) = cmd {
                    self.session_brew_temp = t;
                }
                return out;
            }
            Ambient::RamAndFlash => {
                self.queue_ambient_ram_and_flash(cmd, &mut out);
                return out;
            }
        }

        if self.asleep {
            self.asleep = false;
            self.emit_state_change(MachineState::Sleeping, &mut out);
            return out;
        }

        match (self.state(), cmd) {
            (MachineState::Idle, MachineCommand::TogglePower) => {
                self.asleep = true;
                self.emit_state_change(MachineState::Idle, &mut out);
            }
            (MachineState::Idle, MachineCommand::Brew) => {
                let p = self.profiles[0].clone().unwrap_or_else(default_profile);
                self.start_operation(
                    ActiveOperation::Profile {
                        profile: p,
                        step_idx: 0,
                        step_started_ms: now_ms,
                    },
                    now_ms,
                    &mut out,
                );
            }
            (MachineState::Idle, MachineCommand::RunProfile(p)) => {
                self.start_operation(
                    ActiveOperation::Profile {
                        profile: p,
                        step_idx: 0,
                        step_started_ms: now_ms,
                    },
                    now_ms,
                    &mut out,
                );
            }
            (MachineState::Idle, MachineCommand::Steam) => {
                let limit_ms = (self.settings.machine.steam_time_limit_s.max(0.0) * 1000.0) as u64;
                self.start_operation(
                    ActiveOperation::Steam {
                        started_ms: now_ms,
                        limit_ms,
                    },
                    now_ms,
                    &mut out,
                );
            }
            (MachineState::Idle, MachineCommand::Flush) => {
                self.start_operation(
                    ActiveOperation::DirectPump {
                        power: PUMP_POWER,
                        started_ms: now_ms,
                    },
                    now_ms,
                    &mut out,
                );
            }
            (MachineState::Idle, MachineCommand::DirectPump(power)) => {
                self.start_operation(
                    ActiveOperation::DirectPump {
                        power,
                        started_ms: now_ms,
                    },
                    now_ms,
                    &mut out,
                );
            }
            (MachineState::Steaming, MachineCommand::Brew) => {
                self.start_operation(
                    ActiveOperation::HotWater { started_ms: now_ms },
                    now_ms,
                    &mut out,
                );
            }
            (MachineState::Steaming, MachineCommand::Flush) => {
                let stop_at =
                    self.settings.machine.brew_temp + self.settings.machine.temp_offset * 2.0;
                self.start_operation(
                    ActiveOperation::CooldownFlush {
                        started_ms: now_ms,
                        stop_at,
                    },
                    now_ms,
                    &mut out,
                );
            }
            (s, _) if s.is_busy() => {
                self.stop_to_idle(now_ms, &mut out);
            }
            _ => {}
        }

        out
    }

    pub fn tick(&mut self, now_ms: u64, volume_ml: f32, temp_c: f32) -> MachineStepEffects {
        let mut out = MachineStepEffects::default();

        if let Some(ref mut op) = self.op {
            if op.tick(now_ms, volume_ml, temp_c, &mut out) {
                self.stop_to_idle(now_ms, &mut out);
                return out;
            }
        }

        if self.state() == MachineState::Idle {
            let timeout_ms = (self.settings.machine.sleep_timeout_min.max(0.0) * 60_000.0) as u64;
            if now_ms.saturating_sub(self.last_activity_ms) >= timeout_ms {
                self.asleep = true;
                self.emit_state_change(MachineState::Idle, &mut out);
            }
        }

        out
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/machine.rs"]
mod tests;
