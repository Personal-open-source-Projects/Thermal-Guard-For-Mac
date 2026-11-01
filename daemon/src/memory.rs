//! Pure policy for OS-level memory reclamation.
//!
//! macOS already moves cold pages to the compressor and to swap by itself; the
//! app must not fight that, and userspace cannot "move" anything. What it *can*
//! add is one bounded, measured step when the kernel has run out of easy
//! options: drop purgeable/file-backed cache so allocations have somewhere to
//! go, instead of immediately quitting the user's apps.
//!
//! The primary signal is the kernel's own verdict,
//! `kern.memorystatus_vm_pressure_level` (1 normal, 2 warning, 4 critical).
//! Free-page percentages are only a fallback: macOS keeps "Pages free" low by
//! design because unused RAM is cache, so a free-percentage rule on its own
//! would purge a perfectly healthy machine.
//!
//! Additional brakes, all enforced here so they are testable without pressure:
//!   * normal pressure is never acted on;
//!   * less than [`MIN_RECLAIM_MB`] of reclaimable cache means a purge would
//!     free nothing while still costing disk I/O;
//!   * a machine already deep into swap is skipped: purging there adds I/O to a
//!     thrashing system, which is the opposite of help;
//!   * a cooldown and an hourly cap bound the worst case.

/// Cache below this is not worth stalling the machine for.
pub const MIN_RECLAIM_MB: f64 = 1024.0;
/// Minimum time between reclaims.
pub const COOLDOWN_S: u64 = 1800;
/// Hard cap per rolling hour.
pub const MAX_PER_HOUR: u32 = 2;
/// Above this swap usage the system is thrashing; do not add I/O.
pub const SWAP_THRASH_PCT: f64 = 70.0;

/// The same bands the UI uses, so both sides agree on "high" and "critical".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// What the daemon asks for at this pressure. Cache first — dropping cache has
/// no user-visible cost — and app trimming only when there is nothing to drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advice {
    Hold,
    ReclaimCache,
    TrimApps,
}

impl Advice {
    pub fn as_str(&self) -> &'static str {
        match self {
            Advice::Hold => "hold",
            Advice::ReclaimCache => "reclaim",
            Advice::TrimApps => "trim",
        }
    }
}

pub fn advice(pressure: Pressure, reclaimable_mb: f64, swap_used_pct: f64) -> Advice {
    match pressure {
        Pressure::Normal => Advice::Hold,
        _ if reclaimable_mb >= MIN_RECLAIM_MB && swap_used_pct < SWAP_THRASH_PCT => {
            Advice::ReclaimCache
        }
        _ => Advice::TrimApps,
    }
}

/// Growth of the compressor and of swap across a window, in MB/s. Predictive,
/// not reactive: a machine climbing into pressure is visible before it is
/// critical, so cache can be dropped while it still helps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Trend {
    pub compressed_mb_per_s: f64,
    pub swap_mb_per_s: f64,
}

/// `samples` is `(elapsed_seconds, compressed_mb, swap_used_mb)`, oldest first.
pub fn trend(samples: &[(f64, f64, f64)], window_s: f64) -> Trend {
    let (first, last) = match (samples.first(), samples.last()) {
        (Some(a), Some(b)) => (a, b),
        _ => return Trend::default(),
    };
    let dt = (last.0 - first.0).min(window_s);
    if dt <= 0.0 {
        return Trend::default();
    }
    Trend {
        compressed_mb_per_s: ((last.1 - first.1) / dt).max(0.0),
        swap_mb_per_s: ((last.2 - first.2) / dt).max(0.0),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Purge,
}

pub struct Inputs {
    /// User setting; on by default.
    pub enabled: bool,
    /// Mode::Off steps aside entirely.
    pub mode_off: bool,
    pub pressure: Pressure,
    /// Purgeable + file-backed cache, in MB.
    pub reclaimable_mb: f64,
    pub swap_used_pct: f64,
    /// Seconds since the last reclaim, `u64::MAX` when there never was one.
    pub since_last_s: u64,
    pub reclaims_last_hour: u32,
}

pub fn plan(i: &Inputs) -> Action {
    if !i.enabled || i.mode_off || i.pressure == Pressure::Normal {
        return Action::None;
    }
    if i.reclaimable_mb < MIN_RECLAIM_MB {
        return Action::None;
    }
    if i.swap_used_pct >= SWAP_THRASH_PCT {
        return Action::None;
    }
    if i.since_last_s < COOLDOWN_S {
        return Action::None;
    }
    if i.reclaims_last_hour >= MAX_PER_HOUR {
        return Action::None;
    }
    Action::Purge
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs {
            enabled: true,
            mode_off: false,
            pressure: Pressure::High,
            reclaimable_mb: 2048.0,
            swap_used_pct: 20.0,
            since_last_s: u64::MAX,
            reclaims_last_hour: 0,
        }
    }

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
    fn a_generous_cache_under_pressure_is_reclaimed() {
        assert_eq!(plan(&inputs()), Action::Purge);
    }

    #[test]
    fn nothing_happens_without_pressure_or_a_cache() {
        let mut i = inputs();
        i.pressure = Pressure::Normal;
        assert_eq!(plan(&i), Action::None);
        let mut i = inputs();
        i.reclaimable_mb = MIN_RECLAIM_MB - 1.0;
        assert_eq!(plan(&i), Action::None);
    }

    #[test]
    fn off_mode_and_the_toggle_step_aside() {
        let mut i = inputs();
        i.mode_off = true;
        assert_eq!(plan(&i), Action::None);
        i.mode_off = false;
        i.enabled = false;
        assert_eq!(plan(&i), Action::None);
    }

    #[test]
    fn a_thrashing_swap_is_never_made_worse() {
        let mut i = inputs();
        i.swap_used_pct = SWAP_THRASH_PCT;
        assert_eq!(plan(&i), Action::None);
    }

    #[test]
    fn advice_reclaims_before_asking_anyone_to_quit() {
        assert_eq!(advice(Pressure::Normal, 4096.0, 0.0), Advice::Hold);
        assert_eq!(
            advice(Pressure::High, 4096.0, 10.0),
            Advice::ReclaimCache
        );
        // Nothing reclaimable, or swap is already thrashing: apps are the lever.
        assert_eq!(advice(Pressure::High, 512.0, 10.0), Advice::TrimApps);
        assert_eq!(
            advice(Pressure::Critical, 4096.0, SWAP_THRASH_PCT),
            Advice::TrimApps
        );
    }

    #[test]
    fn trend_is_positive_growth_only() {
        let rising = trend(&[(0.0, 100.0, 50.0), (10.0, 300.0, 150.0)], 20.0);
        assert!((rising.compressed_mb_per_s - 20.0).abs() < 1e-9);
        assert!((rising.swap_mb_per_s - 10.0).abs() < 1e-9);

        let falling = trend(&[(0.0, 300.0, 150.0), (10.0, 100.0, 50.0)], 20.0);
        assert_eq!(falling, Trend::default());

        // One sample, or a zero window, is not a trend.
        assert_eq!(trend(&[(0.0, 100.0, 50.0)], 20.0), Trend::default());
        assert_eq!(trend(&[(0.0, 1.0, 1.0), (5.0, 9.0, 9.0)], 0.0), Trend::default());
    }

    #[test]
    fn cooldown_and_hourly_cap_are_enforced() {
        let mut i = inputs();
        i.since_last_s = COOLDOWN_S - 1;
        assert_eq!(plan(&i), Action::None);
        i.since_last_s = COOLDOWN_S;
        assert_eq!(plan(&i), Action::Purge);
        i.reclaims_last_hour = MAX_PER_HOUR;
        assert_eq!(plan(&i), Action::None);
    }
}
