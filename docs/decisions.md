# Decision log

This is the single decision log for the complete system and all components.
Record unresolved matters in [Open questions](open-questions.md).

Record each decision or baseline change here. Do not delete rows. Mark a replaced row as `Superseded`.

| ID   | Date       | Decision                                                                                                                                                                                                                                                             |
| ---- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D-01 | 2026-10-21 | Core split: Core 0 executes hard real-time control (PIO, ADC, PID, actuators); Core 1 executes networking, web server, and telemetry.                                                                                                                                |
| D-02 | 2026-10-21 | AC synchronization: lock control loop to mains frequency (50/60 Hz) triggered by the falling edge of the zero-cross optocoupler input.                                                                                                                               |
| D-03 | 2026-10-21 | Actuator switching isolation: pump triac uses random-phase MOC mid-half-wave; boiler heater uses zero-cross MOC latched 2 ms after the zero-cross edge.                                                                                                              |
| D-04 | 2026-10-21 | Hardware heater fail-safe: heater PIO state machine uses `pull noblock` where empty FIFO reads 0 (OFF). If control task stalls, heater turns off without software intervention.                                                                                      |
| D-05 | 2026-10-21 | Dual network interfaces: Port 80 serves interactive HTTP/JSON web dashboard; port 8080 streams raw postcard binary frames and receives newline-delimited JSON commands.                                                                                              |
| D-06 | 2026-10-21 | Safe-boot fallback: holding GP8 (Flush button) during boot forces Wi-Fi Access Point mode for credential recovery.                                                                                                                                                   |
| D-08 | 2026-10-21 | Shared analog nets: GP26 and GP27 share physical PCB nets with GP40 (A0) and GP41 (A1). Firmware configures GP26/GP27 as high-impedance inputs (`Pull::None`).                                                                                                       |
| D-09 | 2026-10-21 | Dual-PID architecture: separate PID controllers for thermal regulation (`temp_pid` at 5 Hz with flow feed-forward) and pump modulation (`pump_pid` at 50 Hz). Profiles use 2D steps with simultaneous pressure and flow targets, governed by the binding constraint. |
