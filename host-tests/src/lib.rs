//! Host (PC) unit, simulation, and fuzz tests for pure oximite firmware modules.

#[path = "../../firmware/src/backoff.rs"]
pub mod backoff;
#[path = "../../firmware/src/board.rs"]
pub mod board;
#[path = "../../firmware/src/buttons.rs"]
pub mod buttons;
#[path = "../../firmware/src/controller.rs"]
pub mod controller;
#[path = "../../firmware/src/dhcp_codec.rs"]
pub mod dhcp_codec;
#[path = "../../firmware/src/flow.rs"]
pub mod flow;
#[path = "../../firmware/src/http_codec.rs"]
pub mod http_codec;
#[path = "../../firmware/src/leds.rs"]
pub mod leds;
#[path = "../../firmware/src/machine.rs"]
pub mod machine;
#[path = "../../firmware/src/pid.rs"]
pub mod pid;
#[path = "../../firmware/src/sensors.rs"]
pub mod sensors;

#[cfg(test)]
mod l0;
#[cfg(test)]
mod sim;
