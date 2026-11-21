//! Pure 5-sample integrator button debouncer.

pub const DEBOUNCE_SAMPLES: u8 = 5;

pub struct Debouncer<T: Clone> {
    command: T,
    pressed: bool,
    integrator: u8,
}

impl<T: Clone> Debouncer<T> {
    pub fn new(initially_low: bool, command: T) -> Self {
        Self {
            command,
            pressed: initially_low,
            integrator: if initially_low { DEBOUNCE_SAMPLES } else { 0 },
        }
    }

    pub fn poll(&mut self, is_low: bool) -> Option<T> {
        if is_low {
            self.integrator = (self.integrator + 1).min(DEBOUNCE_SAMPLES);
        } else {
            self.integrator = self.integrator.saturating_sub(1);
        }

        match (self.pressed, self.integrator) {
            (false, DEBOUNCE_SAMPLES) => {
                self.pressed = true;
                Some(self.command.clone())
            }
            (true, 0) => {
                self.pressed = false;
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/debounce.rs"]
mod tests;
