use super::*;
use oximite_proto::model::{BrewProfileStep, WifiSettings, DEFAULT_SETTINGS};

fn empty_profiles() -> [Option<BrewProfile>; MAX_PROFILES as usize] {
    [const { None }; MAX_PROFILES as usize]
}

#[test]
fn plan_saves_orders_sections_and_covers_all_bits() {
    assert!(plan_saves(0).is_empty());
    assert_eq!(plan_saves(DIRTY_PIDS).as_slice(), &[SaveItem::Pids]);
    let mask = DIRTY_MACHINE | DIRTY_WIFI | dirty_profile(0) | dirty_profile(MAX_PROFILES - 1);
    assert_eq!(
        plan_saves(mask).as_slice(),
        &[
            SaveItem::Machine,
            SaveItem::Wifi,
            SaveItem::Profile(0),
            SaveItem::Profile(MAX_PROFILES - 1),
        ]
    );
    assert_eq!(plan_saves(u16::MAX).len(), MAX_SAVE_ITEMS);
}

#[test]
fn state_transitions_and_steaming_branches() {
    assert_eq!(dirty_profile(10), 0);
    assert!(!is_wifi_dirty(0));
    assert!(is_wifi_dirty(DIRTY_WIFI));

    let mut m = Machine::new(DEFAULT_SETTINGS, empty_profiles(), 0);
    assert_eq!(m.state(), MachineState::Idle);
    assert!(!m.valve_open());
    assert_eq!(m.profiles().len(), MAX_PROFILES as usize);
    assert_eq!(m.wifi_rev(), 1);
    assert_eq!(m.profiles_rev(), 1);
    assert!(!m.public_settings().wifi.has_password);

    // Steam -> target_temp_c uses steam_temp; Brew while steaming enters HotWater
    m.handle_command(MachineCommand::Steam, 100);
    assert_eq!(m.state(), MachineState::Steaming);
    assert_eq!(m.target_temp_c(), DEFAULT_SETTINGS.machine.steam_temp);
    assert_eq!(m.intent().pump_mode, PumpMode::Idle);
    assert!(!m.valve_open());
    m.handle_command(MachineCommand::Brew, 200);
    assert_eq!(m.state(), MachineState::HotWater);
    assert!(!m.valve_open());

    // HotWater auto-stops after MAX_HOT_WATER_MS
    m.tick(200 + MAX_HOT_WATER_MS, 0.0, 92.0);
    assert_eq!(m.state(), MachineState::Idle);

    // Steam -> Flush enters Cooling (heater off, valve closed)
    m.handle_command(MachineCommand::Steam, 400);
    m.handle_command(MachineCommand::Flush, 500);
    assert_eq!(m.state(), MachineState::Cooling);
    assert_eq!(m.intent().target_temp, 0.0);
    assert!(!m.valve_open());

    // Cooldown stops when temperature drops below stop_at
    let stop_at = DEFAULT_SETTINGS.machine.brew_temp + DEFAULT_SETTINGS.machine.temp_offset * 2.0;
    m.tick(1000, 0.0, stop_at);
    assert_eq!(m.state(), MachineState::Idle);
}

#[test]
fn sleeping_auto_wakes_and_ambient_ram_only_does_not_wake() {
    let mut m = Machine::new(DEFAULT_SETTINGS, empty_profiles(), 0);

    // Auto-sleep after sleep_timeout_min of inactivity while Idle
    let sleep_ms = (DEFAULT_SETTINGS.machine.sleep_timeout_min * 60_000.0) as u64;
    m.tick(sleep_ms, 0.0, 92.0);
    assert_eq!(m.state(), MachineState::Sleeping);
    assert_eq!(m.intent().target_temp, 0.0);

    // SetSessionTemp while sleeping updates RAM without waking
    m.handle_command(MachineCommand::SetSessionTemp(94.5), sleep_ms + 100);
    assert_eq!(m.state(), MachineState::Sleeping);
    assert_eq!(m.session_brew_temp(), 94.5);

    // Non-ambient command wakes to Idle and drops waking command
    m.handle_command(MachineCommand::Brew, sleep_ms + 200);
    assert_eq!(m.state(), MachineState::Idle);
    assert!((m.intent().target_temp - (94.5 + DEFAULT_SETTINGS.machine.temp_offset)).abs() < 1e-4);

    // TogglePower transitions Idle -> Sleeping -> Idle
    m.handle_command(MachineCommand::TogglePower, sleep_ms + 300);
    assert_eq!(m.state(), MachineState::Sleeping);
    m.handle_command(MachineCommand::TogglePower, sleep_ms + 400);
    assert_eq!(m.state(), MachineState::Idle);
}

#[test]
fn decision_6_defers_both_ram_and_flash_until_idle() {
    let mut m = Machine::new(DEFAULT_SETTINGS, empty_profiles(), 0);
    m.handle_command(MachineCommand::Steam, 100);
    assert_eq!(m.state(), MachineState::Steaming);

    let mut new_machine = DEFAULT_SETTINGS.machine;
    new_machine.brew_temp = 95.0;
    let new_pid = PidSettings {
        kp: 15.0,
        ki: 1.5,
        kd: 25.0,
    };
    let mut wifi = WifiSettings {
        ssid: heapless::String::new(),
        password: heapless::String::new(),
    };
    let _ = wifi.ssid.push_str("EspressoLAN");
    let _ = wifi.password.push_str("secret123");

    let mut prof_name = heapless::String::new();
    let _ = prof_name.push_str("SavedMidShot");
    let mut steps = heapless::Vec::new();
    let _ = steps.push(BrewProfileStep {
        time_s: Some(10.0),
        volume: None,
        pressure: Some(9.0),
        flow: None,
    });
    let prof = BrewProfile {
        name: prof_name,
        steps,
    };

    // Issue all four distinct Ambient::RamAndFlash saves while busy
    let e1 = m.handle_command(MachineCommand::SaveMachine(new_machine), 200);
    let e2 = m.handle_command(
        MachineCommand::SavePids(new_pid, DEFAULT_SETTINGS.pump_pid),
        210,
    );
    let e3 = m.handle_command(MachineCommand::SaveWifi(wifi.clone()), 215);
    let e4 = m.handle_command(MachineCommand::SaveProfile(3, prof.clone()), 220);

    // Neither RAM nor flash dirty flags change while busy
    assert_eq!(e1.dirty_flash, 0);
    assert_eq!(e2.dirty_flash, 0);
    assert_eq!(e3.dirty_flash, 0);
    assert_eq!(e4.dirty_flash, 0);
    assert_eq!(
        m.settings().machine.brew_temp,
        DEFAULT_SETTINGS.machine.brew_temp
    );
    assert!(m.profile(3).is_none());
    assert!(m.has_deferred_writes());

    // Stopping to Idle flushes ALL coalesced sections to RAM and dirty_flash
    let e_stop = m.handle_command(MachineCommand::Stop, 300);
    assert_eq!(m.state(), MachineState::Idle);
    assert!(!m.has_deferred_writes());
    assert_eq!(m.settings().machine.brew_temp, 95.0);
    assert_eq!(m.settings().temp_pid, new_pid);
    assert_eq!(m.settings().wifi, wifi);
    assert!(m.public_settings().wifi.has_password);
    assert_eq!(m.wifi_rev(), 2);
    assert_eq!(m.profiles_rev(), 2);
    assert_eq!(m.profile(3), Some(&prof));
    assert!(is_machine_dirty(e_stop.dirty_flash));
    assert!(is_pids_dirty(e_stop.dirty_flash));
    assert!(is_wifi_dirty(e_stop.dirty_flash));
    assert!(is_profile_dirty(e_stop.dirty_flash, 3));

    // DeleteProfile while Idle applies immediately
    let e_del = m.handle_command(MachineCommand::DeleteProfile(3), 400);
    assert!(m.profile(3).is_none());
    assert!(is_profile_dirty(e_del.dirty_flash, 3));
}

#[test]
fn profile_edge_cases_direct_pump_bar_and_safety_timeouts() {
    let mut m = Machine::new(DEFAULT_SETTINGS, empty_profiles(), 0);

    // 1. Empty profile (no runnable steps) is ignored without entering Brewing or opening the valve
    let empty_prof = BrewProfile {
        name: heapless::String::try_from("Empty").unwrap(),
        steps: heapless::Vec::new(),
    };
    let eff_empty = m.handle_command(MachineCommand::RunProfile(empty_prof), 100);
    assert_eq!(m.state(), MachineState::Idle);
    assert!(!m.valve_open());
    assert!(!eff_empty.reset_volume);
    assert!(eff_empty.events.is_empty());

    // 2. Profile with a zero-time/zero-volume step skipped, followed by pressure >= DIRECT_PUMP_BAR
    //    with only a volume target, terminating via STEP_SAFETY_MS timeout
    let mut steps = heapless::Vec::new();
    let _ = steps.push(BrewProfileStep {
        time_s: Some(0.0),
        volume: Some(0.0),
        pressure: Some(4.0),
        flow: None,
    });
    let _ = steps.push(BrewProfileStep {
        time_s: None,
        volume: Some(50.0),
        pressure: Some(11.0), // >= DIRECT_PUMP_BAR (10.0)
        flow: None,
    });
    let prof = BrewProfile {
        name: heapless::String::try_from("HighP").unwrap(),
        steps,
    };
    let eff = m.handle_command(MachineCommand::RunProfile(prof), 1_000);
    assert_eq!(m.state(), MachineState::Brewing);
    assert_eq!(m.intent().step_idx, 1);
    assert_eq!(m.intent().pump_mode, PumpMode::DirectPump(11.0));
    assert!(eff.events.contains(&DeviceEvent::StepChanged {
        index: 1,
        reason: StepReason::Started,
    }));

    // Advance past STEP_SAFETY_MS without reaching 50 ml -> SafetyTimeout & Completed
    let eff_timeout = m.tick(1_000 + STEP_SAFETY_MS, 5.0, 92.0);
    assert_eq!(m.state(), MachineState::Idle);
    assert!(eff_timeout.events.contains(&DeviceEvent::StepChanged {
        index: 1,
        reason: StepReason::SafetyTimeout,
    }));
    assert!(eff_timeout.events.contains(&DeviceEvent::StepChanged {
        index: STEP_NONE,
        reason: StepReason::Completed,
    }));

    // 3. DirectPump and Steam time limits
    m.handle_command(MachineCommand::DirectPump(55.0), 200_000);
    assert_eq!(m.state(), MachineState::Pumping);
    assert_eq!(m.intent().pump_mode, PumpMode::DirectPump(55.0));
    m.tick(200_000 + MAX_PUMP_RUN_MS, 0.0, 92.0);
    assert_eq!(m.state(), MachineState::Idle);

    m.handle_command(MachineCommand::Steam, 300_000);
    assert_eq!(m.state(), MachineState::Steaming);
    let steam_limit_ms = (DEFAULT_SETTINGS.machine.steam_time_limit_s * 1000.0) as u64;
    m.tick(300_000 + steam_limit_ms, 0.0, 135.0);
    assert_eq!(m.state(), MachineState::Idle);
}
