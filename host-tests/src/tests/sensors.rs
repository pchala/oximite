use super::*;

#[test]
fn ntc_lut_is_monotonically_decreasing_and_interpolates() {
    for i in 0..NTC_LUT.len() - 1 {
        assert!(NTC_LUT[i] >= NTC_LUT[i + 1]);
    }
    let a = NTC_LUT[250];
    let b = NTC_LUT[251];
    let mid = temp_from_adc(250.5 * 4.0);
    assert!((mid - (a + b) * 0.5).abs() < 1e-3);
    assert_eq!(temp_from_adc(0.0), NTC_LUT[0]);
    assert!(temp_from_adc(4095.0) >= NTC_LUT[1024]);
}

#[test]
fn pressure_scaling_and_zero_clamp() {
    // 0.4 V -> 0 bar
    let raw_04 = 0.4 / 3.3 * 4095.0;
    assert!(pressure_from_adc(raw_04).abs() < 1e-3);
    assert_eq!(pressure_from_adc(0.0), 0.0);

    // 1.9 V -> 9 bar
    let raw_19 = 1.9 / 3.3 * 4095.0;
    assert!((pressure_from_adc(raw_19) - 9.0).abs() < 1e-2);
}

#[test]
fn average_settled_and_sensor_filter_converge() {
    let def_reading = SensorReading::default();
    assert_eq!(def_reading.pressure_bar, 0.0);
    assert_eq!(def_reading.temp_c, 20.0);

    let samples = [0, 0, 0, 0, 0, 1000, 1000, 1000, 1000, 1000];
    assert_eq!(average_settled(&samples), 1000.0);

    let mut filter = SensorFilter::default();
    let raw_p0 = 0.4 / 3.3 * 4095.0;
    let first = filter.update(raw_p0, 2000.0);
    assert!(first.pressure_bar.abs() < 1e-3);
    assert!((first.temp_c - temp_from_adc(2000.0)).abs() < 1e-3);

    // Step inputs across > P_WINDOW ticks to exercise ring-buffer replacement and EMA
    let raw_p9 = 1.9 / 3.3 * 4095.0;
    let mut last = first;
    for _ in 0..(P_WINDOW * 3) {
        last = filter.update(raw_p9, 1000.0);
    }
    assert!((last.pressure_bar - 9.0).abs() < 1e-2);
    assert!((last.temp_c - temp_from_adc(1000.0)).abs() < 0.2);
}
