//! Pure reconnect backoff and gateway probe helpers adapted from `lite`.

pub const BACKOFF_BASE_MS: u64 = 1_000;
pub const BACKOFF_CAP_MS: u64 = 30_000;
pub const JITTER_SPAN_MS: u64 = 250;

pub const GATEWAY_PROBE_INTERVAL_S: u32 = 30;
pub const GATEWAY_PROBE_MAX_MISSES: u8 = 3;

pub fn backoff_delay_ms(attempt: u32, base_ms: u64, cap_ms: u64, jitter_ticks: u64) -> u64 {
    let shift = attempt.min(6);
    let grown = base_ms.saturating_mul(1u64 << shift);
    let capped = grown.min(cap_ms);
    let jitter = jitter_ticks % JITTER_SPAN_MS;
    capped.saturating_add(jitter)
}

pub fn default_backoff_ms(attempt: u32, jitter_ticks: u64) -> u64 {
    backoff_delay_ms(attempt, BACKOFF_BASE_MS, BACKOFF_CAP_MS, jitter_ticks)
}

/// Self-arming tracker for silent AP / gateway loss detection.
///
/// Only arms after at least one ICMP echo reply succeeds so routers that block
/// LAN ICMP never cause false disconnect loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayProbeTracker {
    armed: bool,
    consecutive_misses: u8,
}

impl Default for GatewayProbeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl GatewayProbeTracker {
    pub const fn new() -> Self {
        Self {
            armed: false,
            consecutive_misses: 0,
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn on_probe_ok(&mut self) {
        self.armed = true;
        self.consecutive_misses = 0;
    }

    /// Records a timed-out gateway probe. Returns `true` when the gateway has
    /// missed `GATEWAY_PROBE_MAX_MISSES` consecutive probes after arming.
    pub fn on_probe_timeout(&mut self) -> bool {
        if !self.armed {
            return false;
        }
        self.consecutive_misses = self.consecutive_misses.saturating_add(1);
        self.consecutive_misses >= GATEWAY_PROBE_MAX_MISSES
    }

    /// Clears missed probe counts when inbound LAN traffic confirms connectivity.
    pub fn on_lan_activity(&mut self) {
        self.consecutive_misses = 0;
    }

    pub fn is_recent_activity(now_s: u32, last_activity_s: u32) -> bool {
        last_activity_s != 0
            && now_s
                .checked_sub(last_activity_s)
                .is_some_and(|dt| dt < GATEWAY_PROBE_INTERVAL_S)
    }
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/backoff.rs"]
mod tests;
