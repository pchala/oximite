//! Pure decoding and debouncing for the front-panel resistor ladder on one ADC pin.
//!
//! One button at a time is pressed. Each button switches its own resistor to GND,
//! so every button, the idle panel, an open wire and a short give a separate level.

use oximite_proto::model::MachineCommand;

pub const DEBOUNCE_SAMPLES: u8 = 5;

/// Lower bounds of each band in 12-bit counts. They sit at the midpoints of the
/// nominal levels in `docs/hardware/schematics.md`.
const OPEN_MIN: u16 = 3911;
const IDLE_MIN: u16 = 3399;
const POWER_MIN: u16 = 2662;
const STEAM_MIN: u16 = 1843;
const FLUSH_MIN: u16 = 1024;
const BREW_MIN: u16 = 205;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Power,
    Steam,
    Flush,
    Brew,
}

impl Button {
    pub fn command(self) -> MachineCommand {
        match self {
            Button::Power => MachineCommand::TogglePower,
            Button::Steam => MachineCommand::Steam,
            Button::Flush => MachineCommand::Flush,
            Button::Brew => MachineCommand::Brew,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    /// Wire open or panel unplugged.
    Open,
    Idle,
    Pressed(Button),
    /// Wire shorted to GND.
    Short,
}

impl Reading {
    pub fn is_fault(self) -> bool {
        matches!(self, Reading::Open | Reading::Short)
    }
}

pub fn decode(raw: u16) -> Reading {
    match raw {
        r if r >= OPEN_MIN => Reading::Open,
        r if r >= IDLE_MIN => Reading::Idle,
        r if r >= POWER_MIN => Reading::Pressed(Button::Power),
        r if r >= STEAM_MIN => Reading::Pressed(Button::Steam),
        r if r >= FLUSH_MIN => Reading::Pressed(Button::Flush),
        r if r >= BREW_MIN => Reading::Pressed(Button::Brew),
        _ => Reading::Short,
    }
}

/// Accepts a reading only after `DEBOUNCE_SAMPLES` identical samples.
/// A press fires only on the change from `Idle` to a button, so a button held at
/// boot, a slide between buttons and a fault never fire.
pub struct LadderDebouncer {
    stable: Reading,
    candidate: Reading,
    count: u8,
}

impl LadderDebouncer {
    /// Starts in a fault state so a button held at boot needs a release first.
    pub const fn new() -> Self {
        Self {
            stable: Reading::Open,
            candidate: Reading::Open,
            count: 0,
        }
    }

    pub fn stable(&self) -> Reading {
        self.stable
    }

    pub fn poll(&mut self, raw: u16) -> Option<Button> {
        let reading = decode(raw);
        if reading != self.candidate {
            self.candidate = reading;
            self.count = 1;
        } else {
            self.count = self.count.saturating_add(1).min(DEBOUNCE_SAMPLES);
        }
        if self.count < DEBOUNCE_SAMPLES || self.candidate == self.stable {
            return None;
        }
        let previous = self.stable;
        self.stable = self.candidate;
        match (previous, self.stable) {
            (Reading::Idle, Reading::Pressed(button)) => Some(button),
            _ => None,
        }
    }
}

impl Default for LadderDebouncer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/buttons.rs"]
mod tests;
