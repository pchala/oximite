use super::*;

const IDLE: u16 = 3686;
const POWER: u16 = 3080;
const STEAM: u16 = 2260;
const FLUSH: u16 = 1440;
const BREW: u16 = 614;

fn feed(db: &mut LadderDebouncer, raw: u16, n: u8) -> Option<Button> {
    let mut fired = None;
    for _ in 0..n {
        fired = fired.or(db.poll(raw));
    }
    fired
}

#[test]
fn decode_covers_every_level_and_band_edge() {
    assert_eq!(decode(4095), Reading::Open);
    assert_eq!(decode(OPEN_MIN), Reading::Open);
    assert_eq!(decode(OPEN_MIN - 1), Reading::Idle);
    assert_eq!(decode(IDLE), Reading::Idle);
    assert_eq!(decode(POWER), Reading::Pressed(Button::Power));
    assert_eq!(decode(STEAM), Reading::Pressed(Button::Steam));
    assert_eq!(decode(FLUSH), Reading::Pressed(Button::Flush));
    assert_eq!(decode(BREW), Reading::Pressed(Button::Brew));
    assert_eq!(decode(BREW_MIN - 1), Reading::Short);
    assert_eq!(decode(0), Reading::Short);
}

#[test]
fn nominal_levels_survive_two_percent_resistor_error() {
    for (nominal, expected) in [
        (IDLE, Reading::Idle),
        (POWER, Reading::Pressed(Button::Power)),
        (STEAM, Reading::Pressed(Button::Steam)),
        (FLUSH, Reading::Pressed(Button::Flush)),
        (BREW, Reading::Pressed(Button::Brew)),
    ] {
        let shift = nominal / 50;
        assert_eq!(decode(nominal + shift), expected);
        assert_eq!(decode(nominal - shift), expected);
    }
}

#[test]
fn press_fires_once_after_five_samples_and_not_on_hold() {
    let mut db = LadderDebouncer::new();
    assert_eq!(feed(&mut db, IDLE, 5), None);
    assert_eq!(feed(&mut db, STEAM, 4), None);
    assert_eq!(db.poll(STEAM), Some(Button::Steam));
    assert_eq!(feed(&mut db, STEAM, 20), None);
}

#[test]
fn bounce_and_slow_release_do_not_fire() {
    let mut db = LadderDebouncer::new();
    feed(&mut db, IDLE, 5);
    for _ in 0..10 {
        assert_eq!(db.poll(BREW), None);
        assert_eq!(db.poll(IDLE), None);
    }
    assert_eq!(feed(&mut db, BREW, 5), Some(Button::Brew));
    // Release passes through other bands for a short time.
    assert_eq!(feed(&mut db, FLUSH, 2), None);
    assert_eq!(feed(&mut db, POWER, 2), None);
    assert_eq!(feed(&mut db, IDLE, 5), None);
    assert_eq!(feed(&mut db, FLUSH, 5), Some(Button::Flush));
}

#[test]
fn slide_between_buttons_without_idle_does_not_fire() {
    let mut db = LadderDebouncer::new();
    feed(&mut db, IDLE, 5);
    assert_eq!(feed(&mut db, POWER, 5), Some(Button::Power));
    assert_eq!(feed(&mut db, STEAM, 10), None);
}

#[test]
fn held_at_boot_needs_release_and_fault_never_fires() {
    let mut db = LadderDebouncer::new();
    assert_eq!(feed(&mut db, FLUSH, 10), None);
    assert_eq!(feed(&mut db, IDLE, 5), None);
    assert_eq!(feed(&mut db, FLUSH, 5), Some(Button::Flush));

    let mut db = LadderDebouncer::new();
    feed(&mut db, IDLE, 5);
    assert_eq!(feed(&mut db, 4095, 10), None);
    assert!(db.stable().is_fault());
    assert_eq!(feed(&mut db, 0, 10), None);
    assert!(db.stable().is_fault());
    // Press from a fault state needs idle first.
    assert_eq!(feed(&mut db, BREW, 10), None);
}

#[test]
fn buttons_map_to_machine_commands() {
    assert_eq!(Button::Power.command(), MachineCommand::TogglePower);
    assert_eq!(Button::Steam.command(), MachineCommand::Steam);
    assert_eq!(Button::Flush.command(), MachineCommand::Flush);
    assert_eq!(Button::Brew.command(), MachineCommand::Brew);
}
