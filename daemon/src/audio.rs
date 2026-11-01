//! Pure planner for the Core Audio watchdog.
//!
//! `coreaudiod` sits between every audio client and the hardware. When its
//! engine gets stuck (a device disappears mid-stream, a sample-rate change
//! races a decoder) it spins, playback stutters and the CPU/fan spike follows
//! — restarting it is Apple's own remedy. Doing that automatically is only
//! acceptable if it is *rare* and *evidence-based*, so the decision is a pure
//! function with three independent brakes:
//!
//!   * a spike has to persist across samples, so one scheduling blip is not a
//!     reason to tear the audio stack down;
//!   * a hard cooldown between resets, so a genuinely broken device cannot
//!     turn the watchdog into a restart loop (which would make the stutter
//!     continuous);
//!   * an hourly cap, so the worst case is bounded and audible only briefly.
//!
//! Nothing here signals anything: `system::restart_coreaudio` performs exactly
//! one SIGTERM, and launchd owns the relaunch.

/// Sustained `coreaudiod` CPU share that counts as a stuck engine. A healthy
/// engine idles near 0 % and only touches the CPU in short bursts.
pub const SPIKE_CPU_PCT: f64 = 20.0;
/// How many consecutive samples must exceed [`SPIKE_CPU_PCT`]. The daemon
/// samples every ~2 s, so this is ~4 s of sustained work.
pub const SPIKE_SAMPLES: u32 = 2;
/// Minimum time between two resets.
pub const COOLDOWN_S: u64 = 600;
/// Hard cap per rolling hour.
pub const MAX_PER_HOUR: u32 = 3;

/// What the watchdog wants to do on this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Restart,
}

/// Everything the decision needs, all supplied by the caller so the policy is
/// testable without an audio device (or a root daemon).
pub struct Inputs {
    /// User setting; the guard is on by default.
    pub enabled: bool,
    /// Mode::Off means the app steps aside entirely.
    pub mode_off: bool,
    /// Consecutive samples above [`SPIKE_CPU_PCT`].
    pub spike_streak: u32,
    /// Seconds since the last reset, `u64::MAX` when there never was one.
    pub since_last_s: u64,
    /// Resets already performed in the last hour.
    pub restarts_last_hour: u32,
}

/// Pure decision: reset only on sustained evidence, never in a loop.
pub fn plan(i: &Inputs) -> Action {
    if !i.enabled || i.mode_off {
        return Action::None;
    }
    if i.spike_streak < SPIKE_SAMPLES {
        return Action::None;
    }
    if i.since_last_s < COOLDOWN_S {
        return Action::None;
    }
    if i.restarts_last_hour >= MAX_PER_HOUR {
        return Action::None;
    }
    Action::Restart
}

/// Fold one CPU sample into the streak. Unknown or non-finite readings reset
/// it: a missing sample is not evidence of a spike.
pub fn spike_streak(previous: u32, coreaudiod_cpu: Option<f64>) -> u32 {
    match coreaudiod_cpu {
        Some(v) if v.is_finite() && v >= SPIKE_CPU_PCT => previous.saturating_add(1),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs {
            enabled: true,
            mode_off: false,
            spike_streak: SPIKE_SAMPLES,
            since_last_s: u64::MAX,
            restarts_last_hour: 0,
        }
    }

    #[test]
    fn a_sustained_spike_is_reset() {
        assert_eq!(plan(&inputs()), Action::Restart);
    }

    #[test]
    fn the_guard_and_off_mode_both_step_aside() {
        let mut i = inputs();
        i.enabled = false;
        assert_eq!(plan(&i), Action::None);
        i.enabled = true;
        i.mode_off = true;
        assert_eq!(plan(&i), Action::None);
    }

    #[test]
    fn one_sample_is_not_a_spike() {
        let mut i = inputs();
        i.spike_streak = SPIKE_SAMPLES - 1;
        assert_eq!(plan(&i), Action::None);
    }

    #[test]
    fn the_cooldown_prevents_a_restart_loop() {
        let mut i = inputs();
        i.since_last_s = COOLDOWN_S - 1;
        assert_eq!(plan(&i), Action::None);
        i.since_last_s = COOLDOWN_S;
        assert_eq!(plan(&i), Action::Restart);
    }

    #[test]
    fn the_hourly_cap_is_enforced() {
        let mut i = inputs();
        i.restarts_last_hour = MAX_PER_HOUR;
        assert_eq!(plan(&i), Action::None);
        i.restarts_last_hour = MAX_PER_HOUR - 1;
        assert_eq!(plan(&i), Action::Restart);
    }

    #[test]
    fn streak_tracks_consecutive_samples_only() {
        assert_eq!(spike_streak(0, Some(25.0)), 1);
        assert_eq!(spike_streak(1, Some(SPIKE_CPU_PCT)), 2);
        assert_eq!(spike_streak(2, Some(19.9)), 0);
        assert_eq!(spike_streak(2, None), 0);
        assert_eq!(spike_streak(2, Some(f64::NAN)), 0);
        assert_eq!(spike_streak(2, Some(f64::INFINITY)), 0);
    }
}
