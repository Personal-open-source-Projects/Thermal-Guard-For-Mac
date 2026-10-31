//! The decision engine: what to do, given the sensors and a mode.
//!
//! Every action here is reversible and idempotent. The design goal is to stop
//! *heat generation* (turbo boost + background CPU bursts), because on this
//! chassis removing heat after the fact does not work: the fan is not coupled
//! to the heat pipe.
//!
//! `Mode::Auto` is the adaptive governor: it keeps the CPU at full performance
//! whenever the chip is comfortable, and clamps the power envelope only when
//! the temperature or the OS' own throttling says it must. A dwell time stops
//! it from flapping between states.

/// What the user asked the fan to do. Kept separate from the power policy: the
/// fan target is only honoured on Macs whose SMC accepts it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FanIntent {
    /// Leave the fan to Apple's own thermal curve.
    Auto,
    /// Hold the fan at a low, quiet target.
    Quiet,
    /// Pin the fan at the SMC's maximum.
    Max,
}

impl FanIntent {
    pub fn parse(s: &str) -> Option<FanIntent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "smc" | "curve" => Some(FanIntent::Auto),
            "quiet" | "silent" | "low" => Some(FanIntent::Quiet),
            "max" | "maximum" | "full" => Some(FanIntent::Max),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            FanIntent::Auto => "auto",
            FanIntent::Quiet => "quiet",
            FanIntent::Max => "max",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Auto,
    Off,
    Balanced,
    CoolQuiet,
    MaxCool,
}

/// Auto governor thresholds.
const AUTO_ENGAGE_C: f64 = 78.0;
/// Release must be reachable on a machine whose *idle* temperature sits in the
/// low 70s (this Air idles at 71–75 °C while charging). At 70 °C the release
/// condition was effectively unsatisfiable, so the power cap stayed on for
/// hours and trimmed turbo during ordinary work and video playback.
const AUTO_RELEASE_C: f64 = 74.0;
/// Minimum time the cap stays engaged before it may be released again.
const AUTO_DWELL_S: u64 = 20;
/// `CPU_Speed_Limit` below this means macOS is already starving the CPU.
const THROTTLE_LIMIT: u32 = 95;
/// Throttling only counts as a reason to cap once the chip is at least warm,
/// so a brief OS scheduling dip does not clamp the power envelope.
const AUTO_THROTTLE_C: f64 = 72.0;

/// Fraction of the SMC's own min..max fan range that "quiet" asks for. At the
/// bottom of the range the fan is inaudible on this chassis.
const QUIET_FAN_RANGE_FRACTION: f64 = 0.18;
/// Above this the fan is handed straight back to the SMC, whatever the user
/// asked for: a quiet fan is never worth a hot chip.
const FAN_SAFETY_RELEASE_C: f64 = 88.0;
/// Hysteresis before a silent fan is allowed again after a safety release.
const FAN_SAFETY_RESUME_C: f64 = 80.0;
/// A reported fan maximum outside this range means the SMC answer cannot be
/// trusted, and we must not write to it.
const MAX_PLAUSIBLE_FAN_RPM: f64 = 20000.0;

/// `Balanced` keeps the cap engaged for at least this long before it may be
/// released. Without it a single cool sample handed turbo straight back in the
/// middle of a warm session, and the cap flapped on and off every tick.
const BALANCED_DWELL_S: u64 = 10;

/// The quiet fan target, derived only from the SMC's own reported range.
///
/// Nothing from the wire reaches this function: the caller cannot ask for an
/// arbitrary RPM, so there is no privileged write with an attacker-chosen
/// value. `None` means "do not touch the fan".
pub fn quiet_fan_target(fan_min: Option<f64>, fan_max: Option<f64>) -> Option<f64> {
    let hi = fan_max?;
    if !hi.is_finite() || hi <= 0.0 || hi > MAX_PLAUSIBLE_FAN_RPM {
        return None;
    }
    let lo = fan_min
        .filter(|v| v.is_finite() && *v >= 0.0 && *v < hi)
        .unwrap_or(0.0);
    let target = lo + (hi - lo) * QUIET_FAN_RANGE_FRACTION;
    if target.is_finite() && target >= lo && target <= hi {
        Some(target)
    } else {
        None
    }
}

/// Latching safety check for the quiet fan.
///
/// Unreadable temperature means we cannot prove it is safe, so the fan is left
/// to the SMC. Once released for heat, quiet only returns after the chip has
/// cooled well below the release point.
pub fn quiet_fan_allowed(temp_c: Option<f64>, allowed: bool) -> bool {
    match temp_c {
        None => false,
        Some(t) if t >= FAN_SAFETY_RELEASE_C => false,
        Some(t) if !allowed && t > FAN_SAFETY_RESUME_C => false,
        Some(t) if t.is_finite() => true,
        _ => false,
    }
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "adaptive" | "smart" => Some(Mode::Auto),
            "off" => Some(Mode::Off),
            "balanced" => Some(Mode::Balanced),
            "coolquiet" | "cool" | "quiet" => Some(Mode::CoolQuiet),
            "maxcool" | "max" => Some(Mode::MaxCool),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Off => "off",
            Mode::Balanced => "balanced",
            Mode::CoolQuiet => "coolquiet",
            Mode::MaxCool => "maxcool",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Auto => "Auto (adaptive)",
            Mode::Off => "Monitoring only",
            Mode::Balanced => "Balanced",
            Mode::CoolQuiet => "Cool & Quiet",
            Mode::MaxCool => "Max Cool",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Engage the power cap at/above this temperature.
    pub engage_c: f64,
    /// Release the power cap at/below this temperature (hysteresis).
    pub release_c: f64,
    /// Demote a process when its CPU share is at least this percent.
    pub demote_pct: f64,
}

impl Tuning {
    pub fn for_mode(m: Mode) -> Tuning {
        match m {
            Mode::Auto => Tuning { engage_c: AUTO_ENGAGE_C, release_c: AUTO_RELEASE_C, demote_pct: 80.0 },
            Mode::Off => Tuning { engage_c: 999.0, release_c: 998.0, demote_pct: 999.0 },
            Mode::Balanced => Tuning { engage_c: 82.0, release_c: 72.0, demote_pct: 70.0 },
            Mode::CoolQuiet => Tuning { engage_c: 70.0, release_c: 62.0, demote_pct: 45.0 },
            Mode::MaxCool => Tuning { engage_c: 60.0, release_c: 52.0, demote_pct: 30.0 },
        }
    }

    /// A quiet fan moves noticeably less air, so the same temperature is reached
    /// with less heat to remove. The power envelope is therefore clamped earlier
    /// (and handed back earlier) to keep the chip genuinely cool rather than
    /// trading silence for heat.
    pub fn for_mode_fan(m: Mode, f: FanIntent) -> Tuning {
        let mut t = Self::for_mode(m);
        if f == FanIntent::Quiet && m != Mode::Off {
            t.engage_c -= 6.0;
            t.release_c -= 6.0;
            // And cut more of the background load that the fan would otherwise
            // have to remove.
            t.demote_pct = (t.demote_pct - 10.0).max(20.0);
        }
        t
    }
}

/// The adaptive governor.
///
/// Uncapped → capped when the chip is hot, or warm *and* already being starved
/// by the OS.
///
/// Capped → uncapped on **temperature alone** plus the dwell time.
///
/// This asymmetry is deliberate and came out of end-to-end testing: the first
/// version required `CPU_Speed_Limit >= 100` to release, but that value can sit
/// below 100 for reasons of its own (71 was observed live at 69.8 C while
/// capped). Whenever it did, the release condition became unsatisfiable and the
/// governor stayed clamped with full turbo on the table. Throttling is
/// therefore only ever an *engage* signal, never a condition for release.
/// Convenience wrapper with the default `Auto` thresholds; the daemon itself
/// calls the tuning-aware variant so a silent fan can shift them.
#[cfg(test)]
pub fn auto_should_cap(
    temp_c: Option<f64>,
    speed_limit: Option<u32>,
    cap_active: bool,
    capped_for_s: u64,
) -> bool {
    auto_should_cap_with(
        Tuning::for_mode(Mode::Auto),
        temp_c,
        speed_limit,
        cap_active,
        capped_for_s,
    )
}

/// Same governor, but with thresholds supplied by the caller so the fan
/// preference can shift them.
pub fn auto_should_cap_with(
    t: Tuning,
    temp_c: Option<f64>,
    speed_limit: Option<u32>,
    cap_active: bool,
    capped_for_s: u64,
) -> bool {
    let hot = temp_c.map(|v| v >= t.engage_c).unwrap_or(false);
    let warm = temp_c.map(|v| v >= AUTO_THROTTLE_C).unwrap_or(false);
    let throttled = speed_limit.map(|l| l < THROTTLE_LIMIT).unwrap_or(false);
    let cooled = temp_c.map(|v| v <= t.release_c).unwrap_or(false);

    if cap_active {
        !(cooled && capped_for_s >= AUTO_DWELL_S)
    } else {
        hot || (warm && throttled)
    }
}

pub struct Decision {
    pub cap_power: bool,
    pub demote: Vec<(u32, String)>,
}

/// Below this temperature a live call or video is never clamped: the user is
/// interacting with it, so a stutter is a worse outcome than running warm.
pub const MEDIA_SAFE_C: f64 = 84.0;

/// Media guard, kept separate from the policy above so it can be reasoned about
/// and tested on its own: if a call or player is active and the chip is not
/// genuinely hot, drop the power cap and any demotion.
pub fn apply_media_guard(d: Decision, media_active: bool, temp_c: Option<f64>) -> Decision {
    if !media_active {
        return d;
    }
    // Only a *known*, comfortable temperature relaxes the policy. Unknown or
    // non-finite readings keep the cap: relaxing blindly could be the moment the
    // machine needs protection most.
    match temp_c {
        Some(t) if t.is_finite() && t < MEDIA_SAFE_C => Decision { cap_power: false, demote: Vec::new() },
        // Still protective at the extremes; we only ever give up the *extra*
        // clamping, never the safety limit.
        _ => d,
    }
}

/// Pure function: given the inputs, decide the desired state.
pub fn evaluate(
    mode: Mode,
    fan: FanIntent,
    temp_c: Option<f64>,
    speed_limit: Option<u32>,
    cap_active: bool,
    capped_for_s: u64,
    procs: &[(u32, String, f64)],
) -> Decision {
    let t = Tuning::for_mode_fan(mode, fan);

    let cap_power = match mode {
        Mode::Off => false,
        Mode::Auto => auto_should_cap_with(t, temp_c, speed_limit, cap_active, capped_for_s),
        // Balanced follows the same safety asymmetry as Auto: an unreadable
        // temperature is not evidence of cooling, so an engaged cap is held
        // rather than dropped. Releasing protection is only ever justified by a
        // known, cool reading that has also satisfied the dwell time.
        Mode::Balanced => match temp_c {
            Some(v) if v.is_finite() => {
                if cap_active {
                    v > t.release_c || capped_for_s < BALANCED_DWELL_S
                } else {
                    v >= t.engage_c
                }
            }
            // Unreadable or non-finite: hold whatever we are already doing.
            _ => cap_active,
        },
        Mode::CoolQuiet | Mode::MaxCool => true,
    };

    let mut demote = Vec::new();
    if mode != Mode::Off {
        for (pid, name, cpu) in procs {
            if *cpu >= t.demote_pct {
                demote.push((*pid, name.clone()));
            }
        }
    }

    Decision { cap_power, demote }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hot(pid: u32, cpu: f64) -> (u32, String, f64) {
        (pid, "Hog".to_string(), cpu)
    }

    #[test]
    fn off_mode_never_caps_or_demotes() {
        let d = evaluate(Mode::Off, FanIntent::Auto, Some(99.0), Some(50), false, 0, &[hot(1, 99.0)]);
        assert!(!d.cap_power);
        assert!(d.demote.is_empty());
    }

    #[test]
    fn auto_keeps_full_performance_when_cool() {
        assert!(!auto_should_cap(Some(62.0), Some(100), false, 0));
        assert!(!auto_should_cap(Some(77.9), Some(100), false, 0));
    }

    #[test]
    fn auto_caps_when_hot_or_already_throttling() {
        assert!(auto_should_cap(Some(78.0), Some(100), false, 0));
        assert!(auto_should_cap(Some(88.0), Some(100), false, 0));
        // even at moderate temperature, OS throttling means stop generating heat
        assert!(auto_should_cap(Some(74.0), Some(80), false, 0));
    }

    #[test]
    fn auto_releases_only_after_cooling_and_dwell() {
        // hot and capped: stays capped
        assert!(auto_should_cap(Some(85.0), Some(100), true, 60));
        // cooled but dwell not elapsed: stays capped
        assert!(auto_should_cap(Some(65.0), Some(100), true, 5));
        // cooled, dwell elapsed: restores turbo
        assert!(!auto_should_cap(Some(65.0), Some(100), true, AUTO_DWELL_S));
    }

    #[test]
    fn auto_does_not_deadlock_when_speed_limit_stays_low() {
        // Regression: a release rule that demanded CPU_Speed_Limit >= 100 kept
        // the cap engaged forever whenever the OS reported 71 (seen live).
        assert!(!auto_should_cap(Some(69.0), Some(71), true, 30));
        assert!(!auto_should_cap(Some(64.0), Some(71), true, AUTO_DWELL_S));
    }

    #[test]
    fn auto_engage_from_throttling_requires_warmth() {
        // cold but OS limit low: do not clamp a cool chip
        assert!(!auto_should_cap(Some(55.0), Some(80), false, 0));
        // warm and being starved: clamp
        assert!(auto_should_cap(Some(74.0), Some(80), false, 0));
    }

    #[test]
    fn auto_never_oscillates_within_the_dwell_window() {
        let mut capped = false;
        let mut since = 0u64;
        // alternate readings every second around the thresholds
        for tick in 0..120u64 {
            let temp = if capped { 69.0 } else { 79.0 };
            let next = auto_should_cap(Some(temp), Some(100), capped, since);
            if next != capped {
                capped = next;
                since = 0;
            } else {
                since += 1;
            }
            let _ = tick;
        }
        // one 20 s cap cycle at most per direction, not a per-tick flap
        assert!(capped, "should end up capped when readings stay hot after release");
    }

    #[test]
    fn balanced_caps_only_above_engage_threshold() {
        assert!(!evaluate(Mode::Balanced, FanIntent::Auto, Some(70.0), None, false, 0, &[]).cap_power);
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, Some(82.0), None, false, 0, &[]).cap_power);
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, Some(95.0), None, false, 0, &[]).cap_power);
    }

    #[test]
    fn balanced_has_hysteresis_between_release_and_engage() {
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, Some(75.0), None, true, 60, &[]).cap_power);
        assert!(!evaluate(Mode::Balanced, FanIntent::Auto, Some(71.9), None, true, 60, &[]).cap_power);
    }

    #[test]
    fn balanced_waits_for_the_dwell_before_releasing() {
        // Cool reading, but the cap has only just engaged: hold it.
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, Some(60.0), None, true, 2, &[]).cap_power);
        // Dwell satisfied: hand performance back.
        assert!(!evaluate(Mode::Balanced, FanIntent::Auto, Some(60.0), None, true, BALANCED_DWELL_S, &[]).cap_power);
        // A genuinely hot reading keeps it engaged however long it has been up.
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, Some(90.0), None, true, 600, &[]).cap_power);
    }

    #[test]
    fn balanced_holds_the_cap_when_the_temperature_is_unreadable() {
        // Unreadable is never permission to relax protection.
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, None, None, true, 600, &[]).cap_power);
        assert!(!evaluate(Mode::Balanced, FanIntent::Auto, None, None, false, 0, &[]).cap_power);
        // Non-finite readings behave like unreadable ones.
        assert!(evaluate(Mode::Balanced, FanIntent::Auto, Some(f64::NAN), None, true, 600, &[]).cap_power);
        assert!(!evaluate(Mode::Balanced, FanIntent::Auto, Some(f64::NAN), None, false, 0, &[]).cap_power);
    }

    #[test]
    fn cool_modes_cap_unconditionally() {
        assert!(evaluate(Mode::CoolQuiet, FanIntent::Auto, Some(40.0), None, false, 0, &[]).cap_power);
        assert!(evaluate(Mode::MaxCool, FanIntent::Auto, None, None, false, 0, &[]).cap_power);
    }

    #[test]
    fn demote_thresholds_follow_mode() {
        let procs = [hot(10, 50.0), hot(11, 85.0)];
        let balanced = evaluate(Mode::Balanced, FanIntent::Auto, Some(50.0), None, false, 0, &procs);
        assert_eq!(balanced.demote.len(), 1);
        assert_eq!(balanced.demote[0].0, 11);

        let max = evaluate(Mode::MaxCool, FanIntent::Auto, Some(50.0), None, false, 0, &procs);
        assert_eq!(max.demote.len(), 2);
    }

    #[test]
    fn auto_is_gentle_with_foreground_load() {
        // Auto demotes only true hogs, so interactive work keeps full speed.
        let procs = [hot(1, 60.0), hot(2, 90.0)];
        let d = evaluate(Mode::Auto, FanIntent::Auto, Some(60.0), Some(100), false, 0, &procs);
        assert_eq!(d.demote.len(), 1);
        assert_eq!(d.demote[0].0, 2);
    }

    #[test]
    fn quiet_fan_target_stays_inside_the_smc_range() {
        let t = quiet_fan_target(Some(1300.0), Some(8000.0)).unwrap();
        assert!(t >= 1300.0 && t <= 8000.0);
        // Well below the SMC curve's behaviour at load (it pins 8000 here).
        assert!(t < 3000.0, "quiet target {} is not quiet", t);
    }

    #[test]
    fn quiet_fan_target_refuses_implausible_ranges() {
        assert_eq!(quiet_fan_target(None, None), None);
        assert_eq!(quiet_fan_target(Some(0.0), Some(0.0)), None);
        assert_eq!(quiet_fan_target(Some(0.0), Some(-100.0)), None);
        assert_eq!(quiet_fan_target(Some(0.0), Some(50_000.0)), None);
        assert_eq!(quiet_fan_target(Some(0.0), Some(f64::NAN)), None);
        // A minimum above the maximum must not produce a wild target.
        let t = quiet_fan_target(Some(9000.0), Some(8000.0)).unwrap();
        assert!(t > 0.0 && t <= 8000.0);
    }

    #[test]
    fn quiet_fan_is_released_for_heat_and_only_resumes_once_cool() {
        assert!(quiet_fan_allowed(Some(60.0), true));
        assert!(!quiet_fan_allowed(Some(88.0), true));
        assert!(!quiet_fan_allowed(Some(95.0), true));
        // Latched off: stays off through the hysteresis band.
        assert!(!quiet_fan_allowed(Some(85.0), false));
        assert!(!quiet_fan_allowed(Some(80.5), false));
        assert!(quiet_fan_allowed(Some(79.0), false));
        // Unreadable temperature is never treated as "safe".
        assert!(!quiet_fan_allowed(None, true));
    }

    #[test]
    fn quiet_preference_also_demotes_harder() {
        // Auto demotes at 80 %; with Silent selected the bar drops to 70 %.
        let procs = [hot(1, 72.0), hot(2, 90.0)];
        let quiet = evaluate(Mode::Auto, FanIntent::Quiet, Some(60.0), Some(100), false, 0, &procs);
        let normal = evaluate(Mode::Auto, FanIntent::Auto, Some(60.0), Some(100), false, 0, &procs);
        assert_eq!(quiet.demote.len(), 2, "quiet should also tame the 72% process");
        assert_eq!(normal.demote.len(), 1);
        // Never below the floor, and Off stays inert.
        assert_eq!(Tuning::for_mode_fan(Mode::MaxCool, FanIntent::Quiet).demote_pct, 20.0);
        assert_eq!(Tuning::for_mode_fan(Mode::Off, FanIntent::Quiet).demote_pct, 999.0);
    }

    #[test]
    fn quiet_fan_makes_the_governor_clamp_earlier() {
        // Auto alone leaves turbo on at 74 C; with the fan held silent it does not.
        assert!(!evaluate(Mode::Auto, FanIntent::Auto, Some(74.0), Some(100), false, 0, &[]).cap_power);
        assert!(evaluate(Mode::Auto, FanIntent::Quiet, Some(74.0), Some(100), false, 0, &[]).cap_power);
        // Cooling down still hands performance back.
        assert!(!evaluate(Mode::Auto, FanIntent::Quiet, Some(60.0), Some(100), true, 60, &[]).cap_power);
    }

    #[test]
    fn fan_intent_parsing_is_lenient_but_bounded() {
        assert_eq!(FanIntent::parse("silent"), Some(FanIntent::Quiet));
        assert_eq!(FanIntent::parse("QUIET"), Some(FanIntent::Quiet));
        assert_eq!(FanIntent::parse("smc"), Some(FanIntent::Auto));
        assert_eq!(FanIntent::parse("max"), Some(FanIntent::Max));
        assert_eq!(FanIntent::parse("9000rpm"), None);
        assert_eq!(FanIntent::Quiet.as_str(), "quiet");
    }

    #[test]
    fn media_guard_keeps_calls_smooth_until_it_is_really_hot() {
        let capped = || Decision { cap_power: true, demote: vec![(1, "Hog".to_string())] };
        let d = apply_media_guard(capped(), true, Some(78.0));
        assert!(!d.cap_power);
        assert!(d.demote.is_empty());
        // Safety still wins at the extremes.
        let d = apply_media_guard(capped(), true, Some(86.0));
        assert!(d.cap_power);
        // No media session: untouched.
        let d = apply_media_guard(capped(), false, Some(60.0));
        assert!(d.cap_power);
        // Unknown temperature is not treated as permission to relax.
        let d = apply_media_guard(capped(), true, None);
        assert!(d.cap_power);
    }

    #[test]
    fn mode_parsing_is_lenient_but_bounded() {
        assert_eq!(Mode::parse("cool"), Some(Mode::CoolQuiet));
        assert_eq!(Mode::parse("MAX"), Some(Mode::MaxCool));
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("adaptive"), Some(Mode::Auto));
        assert_eq!(Mode::parse("nonsense"), None);
    }
}
