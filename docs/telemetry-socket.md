# Telemetry socket (port 8080)

Port 8080 is a one-way TCP stream from the controller to the client (`diag_task.rs` + `proto/src/protocol.rs`). The controller ignores all bytes that the client sends. Commands and queries use HTTP on port 80. See [Web interface](web-interface.md).

Port 8080 accepts one client. The control app `oximite-control` uses it for live graphs and event notices.

## Framing

Each message is `[len: u16 LE][postcard DeviceMsg]`. There is no handshake. The client treats "socket opened" as the start of a new session. On connect, the client reads settings and profiles over HTTP.

## Messages (`DeviceMsg`)

- `Tick(WireTick)`: One sample for each mains cycle. The code in `WireTick` owns the field list and the fixed-point units.
- `Event(DeviceEvent)`: A state or data change notice.

### Tick size

`WireTick` uses fixed-point integers to keep each frame small. The client rebuilds floats with `WireTick::to_frame`. `seq` is 8 bits and wraps. The client unwraps it and detects lost ticks. `dt_us` is the time since the previous tick. The client sums `dt_us` to get device time.

### Events (`DeviceEvent`)

- `StateChanged`, `StepChanged`: Machine state and profile step changes.
- `SettingsUpdated`, `ProfilesUpdated`: The client MUST refetch the data over HTTP.
- `FlashSaved`: A flash write finished.

## Compatibility

The client checks the firmware only through HTTP and through frame decode errors. Any change to `WireTick` or `DeviceMsg` is a breaking change. Update the control app in the same change.

## Isolation

A stuck browser on port 80 MUST NOT block this stream. A stuck stream client MUST NOT block HTTP.
