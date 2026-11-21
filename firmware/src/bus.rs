//! Lock-free inter-core and inter-task communication hub (`bus.rs`).
//!
//! All cross-core data crosses strictly through lock-free atomics, `Watch`es,
//! bounded `Channel`s, and microsecond-long critical-section copies here. No
//! cross-core async `Mutex` is used anywhere.

use core::cell::{Cell, RefCell};
use core::sync::atomic::Ordering;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use heapless::Vec;
use oximite_proto::model::{
    BrewProfile, ControlSettings, MachineCommand, MachineState, ProfileHeaderOwned, PublicSettings,
    Telemetry, WifiSettings, DEFAULT_SETTINGS, MAX_PROFILES,
};
use oximite_proto::protocol::DeviceEvent;
use portable_atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8};

use crate::controller::ControlIntent;
use crate::sensors::SensorReading;

// ==========================================
// SETTINGS & WIFI SNAPSHOTS (Core 0 writer -> lock-free readers)
// ==========================================

#[derive(Clone)]
pub struct SettingsSnapshot {
    pub control: ControlSettings,
    pub public: PublicSettings,
    pub settings_rev: u32,
}

impl Default for SettingsSnapshot {
    fn default() -> Self {
        Self {
            control: ControlSettings::default(),
            public: DEFAULT_SETTINGS.to_public(),
            settings_rev: 0,
        }
    }
}

static SETTINGS_WATCH: Watch<CriticalSectionRawMutex, SettingsSnapshot, 6> = Watch::new();
static WIFI_STORE: BlockingMutex<CriticalSectionRawMutex, RefCell<WifiSettings>> =
    BlockingMutex::new(RefCell::new(WifiSettings {
        ssid: heapless::String::new(),
        password: heapless::String::new(),
    }));
static WIFI_REV: AtomicU32 = AtomicU32::new(0);

pub fn publish_settings(
    control: ControlSettings,
    public: PublicSettings,
    wifi: &WifiSettings,
    settings_rev: u32,
    wifi_rev: u32,
) {
    WIFI_STORE.lock(|cell| {
        *cell.borrow_mut() = wifi.clone();
    });
    WIFI_REV.store(wifi_rev, Ordering::Relaxed);
    SETTINGS_WATCH.sender().send(SettingsSnapshot {
        control,
        public,
        settings_rev,
    });
}

pub fn get_settings_snapshot() -> SettingsSnapshot {
    SETTINGS_WATCH.try_get().unwrap_or_default()
}

pub fn get_wifi_settings() -> (WifiSettings, u32) {
    let rev = WIFI_REV.load(Ordering::Relaxed);
    let wifi = WIFI_STORE.lock(|cell| cell.borrow().clone());
    (wifi, rev)
}

pub fn get_wifi_rev() -> u32 {
    WIFI_REV.load(Ordering::Relaxed)
}

// ==========================================
// PROFILES STORE (Core 0 writer -> short CS snapshot readers)
// ==========================================

static PROFILE_STORE: BlockingMutex<
    CriticalSectionRawMutex,
    RefCell<[Option<BrewProfile>; MAX_PROFILES as usize]>,
> = BlockingMutex::new(RefCell::new([const { None }; MAX_PROFILES as usize]));
static PROFILE_REV: AtomicU32 = AtomicU32::new(0);

pub fn publish_profiles(profiles: &[Option<BrewProfile>; MAX_PROFILES as usize], rev: u32) {
    PROFILE_STORE.lock(|cell| {
        *cell.borrow_mut() = profiles.clone();
    });
    PROFILE_REV.store(rev, Ordering::Relaxed);
}

pub fn get_profile(slot: u8) -> Option<BrewProfile> {
    if slot >= MAX_PROFILES {
        return None;
    }
    PROFILE_STORE.lock(|cell| cell.borrow()[slot as usize].clone())
}

pub fn get_profile_headers() -> Vec<ProfileHeaderOwned, { MAX_PROFILES as usize }> {
    let mut list = Vec::new();
    PROFILE_STORE.lock(|cell| {
        let borrow = cell.borrow();
        for (i, slot) in borrow.iter().enumerate() {
            if let Some(p) = slot {
                let _ = list.push(ProfileHeaderOwned {
                    slot: i as u8,
                    name: p.name.clone(),
                });
            }
        }
    });
    list
}

pub fn get_profiles_rev() -> u32 {
    PROFILE_REV.load(Ordering::Relaxed)
}

// ==========================================
// COALESCED DIRTY FLASH MASK (Core 0 coordinator -> Core 0 storage_task)
// ==========================================

static FLASH_DIRTY: AtomicU16 = AtomicU16::new(0);
static SIG_FLASH_DIRTY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn mark_flash_dirty(mask: u16) {
    if mask != 0 {
        FLASH_DIRTY.fetch_or(mask, Ordering::AcqRel);
        SIG_FLASH_DIRTY.signal(());
    }
}

pub async fn wait_flash_dirty() {
    if FLASH_DIRTY.load(Ordering::Acquire) == 0 {
        SIG_FLASH_DIRTY.wait().await;
    }
}

pub fn take_flash_dirty() -> u16 {
    FLASH_DIRTY.swap(0, Ordering::AcqRel)
}

// ==========================================
// SENSORS & FLOW (Core 0 -> control_task / coordinator_task)
// ==========================================

static SENSOR_WATCH: Watch<CriticalSectionRawMutex, SensorReading, 4> = Watch::new();

pub fn publish_sensors(reading: SensorReading) {
    SENSOR_WATCH.sender().send(reading);
}

pub fn get_sensors() -> SensorReading {
    SENSOR_WATCH.try_get().unwrap_or_default()
}

static FLOW_RATE_ML_S: AtomicU32 = AtomicU32::new(0);
static FLOW_VOLUME_ML: AtomicU32 = AtomicU32::new(0);
static SIG_RESET_VOLUME: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn publish_flow(rate_ml_s: f32, volume_ml: f32) {
    FLOW_RATE_ML_S.store(rate_ml_s.to_bits(), Ordering::Relaxed);
    FLOW_VOLUME_ML.store(volume_ml.to_bits(), Ordering::Relaxed);
}

pub fn flow_rate_ml_s() -> f32 {
    f32::from_bits(FLOW_RATE_ML_S.load(Ordering::Relaxed))
}

pub fn shot_volume_ml() -> f32 {
    f32::from_bits(FLOW_VOLUME_ML.load(Ordering::Relaxed))
}

pub fn reset_volume() {
    FLOW_RATE_ML_S.store(0.0f32.to_bits(), Ordering::Relaxed);
    FLOW_VOLUME_ML.store(0.0f32.to_bits(), Ordering::Relaxed);
    SIG_RESET_VOLUME.signal(());
}

pub async fn wait_reset_volume() {
    SIG_RESET_VOLUME.wait().await;
}

// ==========================================
// COHERENT CONTROL INTENT & TELEMETRY
// ==========================================

static CONTROL_INTENT: Watch<CriticalSectionRawMutex, ControlIntent, 2> = Watch::new();

pub fn publish_control_intent(intent: ControlIntent) {
    CONTROL_INTENT.sender().send(intent);
}

pub fn get_control_intent() -> ControlIntent {
    CONTROL_INTENT.try_get().unwrap_or_default()
}

pub static TELEMETRY_WATCH: Watch<CriticalSectionRawMutex, Telemetry, 4> = Watch::new();

pub fn publish_telemetry(t: Telemetry) {
    TELEMETRY_WATCH.sender().send(t);
}

pub fn get_telemetry() -> Telemetry {
    TELEMETRY_WATCH.try_get().unwrap_or_default()
}

// ==========================================
// MACHINE STATE & SESSION BREW TEMP
// ==========================================

pub static MACHINE_STATE: Watch<CriticalSectionRawMutex, MachineState, 4> = Watch::new();
static CURRENT_STATE: AtomicU8 = AtomicU8::new(MachineState::Idle as u8);
static SESSION_BREW_TEMP: AtomicU32 = AtomicU32::new(0);

pub fn get_state() -> MachineState {
    MachineState::try_from(CURRENT_STATE.load(Ordering::Relaxed)).unwrap_or(MachineState::Idle)
}

pub fn set_state(state: MachineState) {
    let old_state = get_state();
    if old_state != state {
        defmt::info!("State Change: {} -> {}", old_state.as_str(), state.as_str());
        CURRENT_STATE.store(state.into(), Ordering::Relaxed);
        MACHINE_STATE.sender().send(state);
    }
}

pub fn get_session_brew_temp() -> f32 {
    f32::from_bits(SESSION_BREW_TEMP.load(Ordering::Relaxed))
}

pub fn set_session_brew_temp(temp: f32) {
    SESSION_BREW_TEMP.store(temp.to_bits(), Ordering::Relaxed);
}

// ==========================================
// COMMAND QUEUE & PORT 8080 DIAGNOSTIC REPLIES / EVENTS
// ==========================================

pub struct QueuedCommand {
    pub ticket: u32,
    pub req_id: Option<u32>,
    pub reply_diag: bool,
    pub cmd: MachineCommand,
}

static COMMAND_QUEUE: Channel<CriticalSectionRawMutex, QueuedCommand, 8> = Channel::new();
static NEXT_TICKET: BlockingMutex<CriticalSectionRawMutex, Cell<u32>> =
    BlockingMutex::new(Cell::new(0));
static SERVED: AtomicU32 = AtomicU32::new(0);

#[must_use = "a dropped ticket cannot be confirmed; use `let _ =` if the caller does not need one"]
pub fn send_command(cmd: MachineCommand, req_id: Option<u32>, reply_diag: bool) -> Option<u32> {
    NEXT_TICKET.lock(|t| {
        let ticket = t.get().wrapping_add(1);
        let q = QueuedCommand {
            ticket,
            req_id,
            reply_diag,
            cmd,
        };
        if COMMAND_QUEUE.try_send(q).is_err() {
            defmt::warn!("Command queue full — command dropped");
            return None;
        }
        t.set(ticket);
        Some(ticket)
    })
}

pub fn mark_served() {
    SERVED.fetch_add(1, Ordering::Relaxed);
}

pub fn get_ack() -> u32 {
    SERVED.load(Ordering::Relaxed)
}

pub async fn next_command() -> QueuedCommand {
    COMMAND_QUEUE.receive().await
}

static DIAG_REPLY_QUEUE: Channel<CriticalSectionRawMutex, (u32, u32), 8> = Channel::new();

pub fn push_diag_reply(req_id: u32, ticket: u32) {
    let _ = DIAG_REPLY_QUEUE.try_send((req_id, ticket));
}

pub fn try_pop_diag_reply() -> Option<(u32, u32)> {
    DIAG_REPLY_QUEUE.try_receive().ok()
}

pub fn clear_diag_queues() {
    while DIAG_REPLY_QUEUE.try_receive().is_ok() {}
    while EVENT_QUEUE.try_receive().is_ok() {}
}

static EVENT_QUEUE: Channel<CriticalSectionRawMutex, DeviceEvent, 16> = Channel::new();

pub fn push_event(ev: DeviceEvent) {
    let _ = EVENT_QUEUE.try_send(ev);
}

pub fn try_pop_event() -> Option<DeviceEvent> {
    EVENT_QUEUE.try_receive().ok()
}

// ==========================================
// NETWORK SUPERVISION STATE
// ==========================================

static NET_AP_MODE: AtomicBool = AtomicBool::new(false);
static WIFI_UP: AtomicBool = AtomicBool::new(false);
static LAST_NET_ACTIVITY_S: AtomicU32 = AtomicU32::new(0);
pub static WIFI_UP_WATCH: Watch<CriticalSectionRawMutex, bool, 4> = Watch::new();

pub fn set_ap_mode(ap: bool) {
    NET_AP_MODE.store(ap, Ordering::Relaxed);
}

pub fn is_ap_mode() -> bool {
    NET_AP_MODE.load(Ordering::Relaxed)
}

pub fn set_wifi_up(up: bool) {
    WIFI_UP.store(up, Ordering::Relaxed);
    WIFI_UP_WATCH.sender().send(up);
}

pub fn is_wifi_up() -> bool {
    WIFI_UP.load(Ordering::Relaxed)
}

pub fn note_net_activity(uptime_s: u32) {
    LAST_NET_ACTIVITY_S.store(uptime_s.max(1), Ordering::Relaxed);
}

pub fn last_net_activity_s() -> u32 {
    LAST_NET_ACTIVITY_S.load(Ordering::Relaxed)
}
