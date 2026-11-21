use super::*;

#[test]
fn led_states_and_grb_word_packing() {
    // Cold saturated, cold crossfade, on target, hot crossfade, hot saturated
    assert_eq!(temp_color(70.0, 92.0, 10.0), Rgb::new(0, 0, 255));
    let cold_mid = temp_color(87.0, 92.0, 10.0);
    assert_eq!(cold_mid.r, 0);
    assert!(cold_mid.g > 120 && cold_mid.b > 120);
    assert_eq!(temp_color(92.0, 92.0, 10.0), Rgb::new(0, 255, 0));
    let hot_mid = temp_color(97.0, 92.0, 10.0);
    assert!(hot_mid.r > 120 && hot_mid.g > 120);
    assert_eq!(hot_mid.b, 0);
    assert_eq!(temp_color(105.0, 92.0, 10.0), Rgb::new(255, 0, 0));

    let t = Telemetry {
        temp_c: 92.0,
        target_temp: 92.0,
        heater_duty: 50.0,
        pump_duty: 80.0,
        ..Default::default()
    };

    // Sleeping -> both magenta
    assert_eq!(
        render_leds(MachineState::Sleeping, &t, true),
        [Rgb::new(255, 0, 255), Rgb::new(255, 0, 255)]
    );

    // Idle in warmup -> boiler green, LED1 red scaled by heater_duty
    let warm = render_leds(MachineState::Idle, &t, true);
    assert_eq!(warm[0], Rgb::new(0, 255, 0));
    assert_eq!(warm[1].g, 0);
    assert_eq!(warm[1].b, 0);
    assert!(warm[1].r > 120);

    // Idle after warmup -> LED1 off
    assert_eq!(
        render_leds(MachineState::Idle, &t, false),
        [Rgb::new(0, 255, 0), Rgb::off()]
    );

    // Brewing -> LED1 pump duty green at 80%, and off when pump_duty == 0
    assert_eq!(
        render_leds(MachineState::Brewing, &t, false),
        [Rgb::new(0, 255, 0), Rgb::new(0, 255, 0)]
    );
    let t_no_pump = Telemetry {
        pump_duty: 0.0,
        ..t
    };
    assert_eq!(
        render_leds(MachineState::Brewing, &t_no_pump, false),
        [Rgb::new(0, 255, 0), Rgb::off()]
    );

    // Steaming -> LED0 off, LED1 boiler
    assert_eq!(
        render_leds(MachineState::Steaming, &t, false),
        [Rgb::off(), Rgb::new(0, 255, 0)]
    );

    // GRB packing scaled by BRIGHTNESS = 30
    let w = pack_grb(Rgb::new(255, 0, 0));
    assert_eq!(w & 0xFF, 0);
    assert_eq!((w >> 16) & 0xFF, (255 * BRIGHTNESS) >> 8);
}
