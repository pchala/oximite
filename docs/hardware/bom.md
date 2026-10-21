# Bill of Materials (BOM)

This document lists the recommended components and ratings for building an oximite retrofit kit.

| Category | Component | Description / Rating | Example Part |
|---|---|---|---|
| **MCU Board** | Pimoroni Pico Plus 2 W | RP2350B, 16 MB Flash, onboard antenna | PIM736 |
| **Power Supply** | AC-DC Isolated Converter | Input 85–264 VAC, Output 5 VDC 1 A | Mean Well IRM-05-5 |
| **Zero-Cross** | Optocoupler | AC input bidirectional opto, open-collector | PC814 / EL814 |
| **Pump Triac** | Phase-angle Triac + Driver | 600 V 8 A triac + non-zero-cross opto | BTA08-600B + MOC3021 |
| **Heater Relay** | Solid State Relay (SSR) | DC control 3–32 VDC, AC load 250 VAC 25 A | Fotek SSR-25DA / Crydom |
| **3-Way Solenoid** | Valve Control Relay | Small SSR or Opto-isolated relay 250 VAC 2 A | G3MB-202P |
| **Flow Meter** | Digmesa Nano DM60 Turbine | 49,162 pulses/L, plastic housing, 98,324 edges/L via PIO | Digmesa Nano DM60 plastic |
| **Pressure Transducer**| Piezoresistive Sensor | 3.3 V supply, 0.4–2.4 V output, 0–1.2 MPa (0–12 bar), G1/8" thread | 0–1.2 MPa G1/8" 3.3V |
| **Temperature Sensor** | Semitec NTC Thermistor | 100 kΩ NTC (B25/85 = 4267 K, ADR™ series), paired with 10 kΩ 0.1% pull-up | Semitec 104NT-4-R025H42G |
| **Status LEDs** | Dual Addressable RGB LEDs | 2x chained WS2812B / SK6812 on 5V strip | WS2812B mini (x2) |
| **Panel Switches** | Mini Toggle Switches (x2) | 3-position momentary (ON)-OFF-(ON), spring return to center | Mini toggle 3-pos momentary (x2) |
