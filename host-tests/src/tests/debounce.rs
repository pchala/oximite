use super::*;

#[test]
fn requires_five_consecutive_samples_to_trigger_and_release() {
    let mut db = Debouncer::new(false, 42u8);

    // 4 low ticks (bounce) must not trigger
    for _ in 0..4 {
        assert_eq!(db.poll(true), None);
    }
    // 5th low tick triggers once
    assert_eq!(db.poll(true), Some(42));
    // Holding low does not re-trigger
    for _ in 0..10 {
        assert_eq!(db.poll(true), None);
    }
    // Brief high glitch does not clear pressed state
    assert_eq!(db.poll(false), None);
    assert_eq!(db.poll(true), None);

    // 5 consecutive high ticks release, allowing next press
    for _ in 0..5 {
        assert_eq!(db.poll(false), None);
    }
    for _ in 0..4 {
        assert_eq!(db.poll(true), None);
    }
    assert_eq!(db.poll(true), Some(42));

    // When initialized already low (held at boot), does not fire until released and pressed again
    let mut held_at_boot = Debouncer::new(true, 99u8);
    assert_eq!(held_at_boot.poll(true), None);
    for _ in 0..5 {
        assert_eq!(held_at_boot.poll(false), None);
    }
    for _ in 0..4 {
        assert_eq!(held_at_boot.poll(true), None);
    }
    assert_eq!(held_at_boot.poll(true), Some(99));
}
