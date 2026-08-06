//! Rate-bounding for log sites a peer or the network can drive.
//!
//! Consumers (rustdesk) write debug-and-up to a log file, so any site whose call
//! rate is not bounded by our own code lets a remote peer decide how much a
//! machine writes to disk. Every such site must either sit at `trace` (not
//! written by those consumers) or go through [`throttled_log!`].

use parking_lot::Mutex;

pub(crate) const LOG_THROTTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Collapses a log site into at most one line per interval, carrying the count
/// of everything suppressed since the last line.
pub(crate) struct LogThrottle {
    interval: std::time::Duration,
    state: Mutex<(u64, Option<std::time::Instant>)>,
}

impl LogThrottle {
    pub(crate) const fn new(interval: std::time::Duration) -> Self {
        Self {
            interval,
            state: Mutex::new((0, None)),
        }
    }

    /// Record one occurrence; `Some(n)` = report now, covering `n` occurrences.
    pub(crate) fn due(&self) -> Option<u64> {
        let mut state = self.state.lock();
        state.0 += 1;
        // `map_or(true, ..)` not `is_none_or`: the latter is Rust 1.82, past the
        // rust-version this crate declares. clippy honors that and stays quiet.
        let due = state.1.map_or(true, |last| last.elapsed() >= self.interval);
        if !due {
            return None;
        }
        state.1 = Some(std::time::Instant::now());
        Some(std::mem::replace(&mut state.0, 0))
    }
}

/// Log at most one line per [`LOG_THROTTLE_INTERVAL`] **per call site**, prefixed
/// with how many occurrences that line stands for. The first occurrence always
/// reports, so a one-off event is never swallowed.
///
/// ```ignore
/// throttled_log!(warn, "malformed input, conv: {:?}", conv);
/// // -> "[x37] malformed input, conv: ..."
/// ```
macro_rules! throttled_log {
    ($level:ident, $fmt:literal $(, $arg:expr)* $(,)?) => {{
        static THROTTLE: $crate::log_throttle::LogThrottle =
            $crate::log_throttle::LogThrottle::new($crate::log_throttle::LOG_THROTTLE_INTERVAL);
        if let Some(suppressed) = THROTTLE.due() {
            log::$level!(concat!("[x{}] ", $fmt), suppressed $(, $arg)*);
        }
    }};
}

pub(crate) use throttled_log;
