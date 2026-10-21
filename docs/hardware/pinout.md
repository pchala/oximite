# Hardware pin assignments

This document is the single home for GPIO assignments, peripheral functions, and PCB net sharing rules for oximite on the Pimoroni Pico Plus 2 W (RP2350B).

## Pinout allocation

| Pin / GPIO | Peripheral / Name | Direction | Description |
|---|---|---|---|
| **GP0** | PIO0 (SM2) | Output | **Triac Control:** Phase-angle gate firing for vibration pump modulation. |
| **GP2** | PIO0 (SM0) | Output | **Heater Relay:** Solid State Relay (SSR) control for the boiler. |
| **GP3** | Standard GPIO | Output | **3-Way Valve:** Solenoid valve control for brew grouphead pressure. |
| **GP5** | GPIO (Pull-Up) | Input | **Left Toggle UP:** Power toggle (momentary flip UP to wake / sleep). |
| **GP6** | GPIO (Pull-Up) | Input | **Right Toggle DOWN:** Brew control (momentary flip DOWN to start / stop extraction). |
| **GP7** | GPIO (Pull-Up) | Input | **Left Toggle DOWN:** Steam mode (momentary flip DOWN to toggle steam). |
| **GP8** | GPIO (Pull-Up) | Input | **Right Toggle UP:** Flush (momentary flip UP to flush; held at boot forces AP mode). |
| **GP9** | PIO2 (SM1) | Output | **Status LEDs:** Serial data for 2x chained WS2812 RGB LEDs (LED0 boiler, LED1 process). |
| **GP10** | PIO0 (SM1) | Input | **Zero-Cross:** Synchronizes triac firing with 50/60 Hz AC mains. |
| **GP15** | PIO2 (SM0) | Input | **Flow Meter:** Flow turbine pulse input. |
| **GP23** | WL_ON | Output | *Internal:* CYW43 Wi-Fi power enable. |
| **GP24** | WL_D / PIO1 | In/Out | *Internal:* CYW43 Wi-Fi SPI data line. |
| **GP25** | WL_CS | Output | *Internal:* CYW43 Wi-Fi chip select. |
| **GP26** | GPIO (HiZ) | — | *Unused:* Shared PCB net with A0 (GP40). See below. |
| **GP27** | GPIO (HiZ) | — | *Unused:* Shared PCB net with A1 (GP41). See below. |
| **GP29** | WL_CLK / PIO1 | Output | *Internal:* CYW43 Wi-Fi SPI clock. |
| **GP40** | ADC (A0) | Analog In | **Pressure Sensor:** Pressure transducer input. See [Schematics](schematics.md). |
| **GP41** | ADC (A1) | Analog In | **Temperature Sensor:** NTC thermistor input. See [Schematics](schematics.md). |

## Unused and shared pins

- **EMI mitigation:** Unused GPIOs (GP1, GP4, GP11..GP14, GP16..GP22, GP28) are driven Output Low on startup to suppress noise coupling inside the metal machine chassis.
- **Shared analog nets:** GP26 and GP27 share physical PCB traces with analog inputs GP40 and GP41. GP26 and GP27 MUST remain high-impedance inputs (`Pull::None`) at all times, so they do not load the ADC signals.
