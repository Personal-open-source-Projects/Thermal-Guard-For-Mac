//! Signals and safety constants for OS-level memory reclamation.
//!
//! macOS already moves cold pages to the compressor and to swap by itself; the
//! app must not fight that, and userspace cannot "move" anything. What it *can*
//! add is one bounded, measured step when the kernel has run out of easy
//! options: drop purgeable/file-backed cache so allocations have somewhere to
//! go, instead of immediately quitting the user's apps.
//!
//! This module owns the *measurement* (pressure level, trend) and the safety
//! constants. [`crate::ram`] owns the decision ladder that consumes them and
//! applies the thermal brakes; keeping the two apart is what lets each be
//! tested without a hot chip or a live compressor.
//!
//! The primary signal is the kernel's own verdict,
//! `kern.memorystatus_vm_pressure_level` (1 normal, 2 warning, 4 critical).
//! Free-page percentages are only a fallback: macOS keeps "Pages free" low by
//! design because unused RAM is cache, so a free-percentage rule on its own
//! would purge a perfectly healthy machine.

/// Cache below this is not worth stalling the machine for.
pub const MIN_RECLAIM_MB: f64 = 1024.0;
/// Minimum time between reclaims.
pub const COOLDOWN_S: u64 = 1800;
/// Hard cap per rolling hour.
pub const MAX_PER_HOUR: u32 = 2;
/// Above this swap usage the system is thrashing; do not add I/O.
pub const SWAP_THRASH_PCT: f64 = 70.0;

/// The same bands the UI uses, so both sides agree on "high" and "critical".
/// Ordered, so a rule can ask for "high or worse" instead of listing cases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    Normal,
    High,
    Critical,
}

impl Pressure {
    pub fn as_str(&self) -> &'static str {
        match self {
            Pressure::Normal => "normal",
            Pressure::High => "high",
            Pressure::Critical => "critical",
        }
    }
}

/// Map the kernel's pressure level, falling back to measured thresholds only
/// when it cannot be read.
///
/// `kernel` is `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning,
/// 4 critical. Anything else is treated as unreadable rather than guessed at.
pub fn pressure_level(
    kernel: Option<u8>,
    free_pct: f64,
    swap_used_pct: f64,
    compressed_pct: f64,
) -> Pressure {
    match kernel {
        Some(4) => return Pressure::Critical,
        Some(2) => return Pressure::High,
        Some(1) => return Pressure::Normal,
        _ => {}
    }
    if !free_pct.is_finite() {
        return Pressure::Critical;
    }
    if free_pct <= 5.0 || swap_used_pct >= 80.0 || compressed_pct >= 60.0 {
        Pressure::Critical
    } else if free_pct <= 10.0 {
        Pressure::High
    } else {
        Pressure::Normal
    }
}

/// How long the trend window looks back.
pub const TREND_WINDOW_S: f64 = 20.0;

/// Growth of the compressor and of swap across a window, in MB/s. Predictive,
/// not reactive: a machine climbing into pressure is visible before it is
/// critical, so cache can be dropped while it still helps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Trend {
    pub compressed_mb_per_s: f64,
    pub swap_mb_per_s: f64,
}

/// `samples` is `(elapsed_seconds, compressed_mb, swap_used_mb)`, oldest first.
/// Callers may use either a forward clock or an age (where the oldest sample
/// has the largest number), so only the magnitude of the elapsed delta matters.
pub fn trend(samples: &[(f64, f64, f64)], window_s: f64) -> Trend {
    let (first, last) = match (samples.first(), samples.last()) {
        (Some(a), Some(b)) => (a, b),
        _ => return Trend::default(),
    };
    let dt = (last.0 - first.0).abs().min(window_s);
    if dt <= 0.0 {
        return Trend::default();
    }
    Trend {
        compressed_mb_per_s: ((last.1 - first.1) / dt).max(0.0),
        swap_mb_per_s: ((last.2 - first.2) / dt).max(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_verdict_wins_over_free_page_heuristics() {
        // macOS keeps free pages low on purpose: a healthy kernel verdict must
        // not be overridden by a tiny free percentage.
        assert_eq!(pressure_level(Some(1), 1.0, 0.0, 0.0), Pressure::Normal);
        assert_eq!(pressure_level(Some(2), 90.0, 0.0, 0.0), Pressure::High);
        assert_eq!(pressure_level(Some(4), 90.0, 0.0, 0.0), Pressure::Critical);
        // Unknown levels fall through to the measurements.
        assert_eq!(pressure_level(Some(7), 50.0, 0.0, 0.0), Pressure::Normal);
    }

    #[test]
    fn measured_thresholds_are_only_a_fallback() {
        assert_eq!(pressure_level(None, 50.0, 0.0, 0.0), Pressure::Normal);
        assert_eq!(pressure_level(None, 10.0, 0.0, 0.0), Pressure::High);
        assert_eq!(pressure_level(None, 5.0, 0.0, 0.0), Pressure::Critical);
        assert_eq!(pressure_level(None, 50.0, 80.0, 0.0), Pressure::Critical);
        assert_eq!(pressure_level(None, 50.0, 0.0, 60.0), Pressure::Critical);
        assert_eq!(pressure_level(None, f64::NAN, 0.0, 0.0), Pressure::Critical);
    }

    #[test]
    fn trend_is_positive_growth_only() {
        let rising = trend(&[(0.0, 100.0, 50.0), (10.0, 300.0, 150.0)], 20.0);
        assert!((rising.compressed_mb_per_s - 20.0).abs() < 1e-9);
        assert!((rising.swap_mb_per_s - 10.0).abs() < 1e-9);

        let falling = trend(&[(0.0, 300.0, 150.0), (10.0, 100.0, 50.0)], 20.0);
        assert_eq!(falling, Trend::default());

        // The daemon supplies sample ages, so its oldest sample has the
        // largest timestamp. That ordering must still expose growth.
        let aged = trend(&[(10.0, 100.0, 50.0), (0.0, 300.0, 150.0)], 20.0);
        assert!((aged.compressed_mb_per_s - 20.0).abs() < 1e-9);
        assert!((aged.swap_mb_per_s - 10.0).abs() < 1e-9);

        // One sample, or a zero window, is not a trend.
        assert_eq!(trend(&[(0.0, 100.0, 50.0)], 20.0), Trend::default());
        assert_eq!(
            trend(&[(0.0, 1.0, 1.0), (5.0, 9.0, 9.0)], 0.0),
            Trend::default()
        );
    }
}
