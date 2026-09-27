//! Virtual time.
//!
//! The simulator never looks at the wall clock. Time is a plain integer number
//! of microseconds that only advances when the event loop pops the next event,
//! so a run that "takes" ten seconds of cluster time finishes in milliseconds
//! and behaves identically every time it is replayed.

/// Microseconds of virtual time since the start of the simulation.
pub type Time = u64;

pub const MILLIS: Time = 1_000;
pub const SECS: Time = 1_000_000;

pub const fn ms(n: u64) -> Time {
    n * MILLIS
}

/// Human-readable form with millisecond precision, e.g. `1.234s`.
pub fn fmt(t: Time) -> String {
    if t == Time::MAX {
        return "∞".to_string();
    }
    format!("{}.{:03}s", t / SECS, (t % SECS) / MILLIS)
}

/// Full-precision form used in traces, e.g. `1.234567s`.
pub fn fmt_us(t: Time) -> String {
    format!("{}.{:06}s", t / SECS, t % SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt(0), "0.000s");
        assert_eq!(fmt(ms(1234)), "1.234s");
        assert_eq!(fmt(Time::MAX), "∞");
        assert_eq!(fmt_us(1_234_567), "1.234567s");
    }
}
