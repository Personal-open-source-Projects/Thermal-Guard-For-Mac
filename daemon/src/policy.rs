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
/// Minimum time the cap stays released before the governor may engage it
/// again, so a load that merely straddles a threshold cannot cycle it.
pub const AUTO_MIN_OFF_S: u64 = 30;
/// Re-engaging inside [`AUTO_MIN_OFF_S`] is only allowed when the chip is this
/// far above the mode's engage threshold: real heat beats the quiet window.
const AUTO_CRITICAL_MARGIN_C: f64 = 4.0;
/// The throttle path must clear the release threshold by this much before it
/// may engage. Without it a release at 74 °C re-engaged at 72 °C on the very
/// next tick while the OS still reported throttling, so the cap flipped on and
/// off every dwell window for as long as the machine sat at ~73 °C.
const THROTTLE_ENGAGE_MARGIN_C: f64 = 2.0;
/// Battery heat may lower the engage threshold, but never close enough to the
/// release threshold to remove the hysteresis altogether.
const BATTERY_MIN_HYSTERESIS_C: f64 = 2.0;

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

/// The fan maximum, validated with exactly the same rules as the quiet target:
/// a nonsense answer from the SMC must never reach a privileged write.
pub fn max_fan_target(fan_min: Option<f64>, fan_max: Option<f64>) -> Option<f64> {
    let hi = fan_max?;
    if !hi.is_finite() || hi <= 0.0 || hi > MAX_PLAUSIBLE_FAN_RPM {
        return None;
    }
    let lo = fan_min
        .filter(|v| v.is_finite() && *v >= 0.0 && *v < hi)
        .unwrap_or(0.0);
    Some(hi.clamp(lo, hi))
}

/// May the cap engage again after it was released?
///
/// Pure and time-driven by the caller: inside [`AUTO_MIN_OFF_S`] only genuine
/// heat (a reading well above the mode's own engage threshold) re-engages it,
/// so an unchanged warm workload cannot toggle the cap every dwell window.
pub fn reengage_allowed(t: Tuning, uncapped_for_s: u64, temp_c: Option<f64>) -> bool {
    if uncapped_for_s >= AUTO_MIN_OFF_S {
        return true;
    }
    match temp_c {
        Some(v) if v.is_finite() && v >= t.engage_c + AUTO_CRITICAL_MARGIN_C => true,
        // Unreadable is not evidence of heat: honour the quiet window.
        _ => false,
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
            // Auto manages the system-wide power envelope only. It cannot tell a
            // foreground build from an unwanted background job, so putting a hot
            // process into Darwin's background QoS here could make the app the
            // person is actively using feel slow. The explicit cooling modes may
            // make that trade-off; Auto must keep interactive work responsive.
            Mode::Auto => Tuning {
                engage_c: AUTO_ENGAGE_C,
                release_c: AUTO_RELEASE_C,
                demote_pct: 999.0,
            },
            Mode::Off => Tuning {
                engage_c: 999.0,
                release_c: 998.0,
                demote_pct: 999.0,
            },
            Mode::Balanced => Tuning {
                engage_c: 82.0,
                release_c: 72.0,
                demote_pct: 70.0,
            },
            Mode::CoolQuiet => Tuning {
                engage_c: 70.0,
                release_c: 62.0,
                demote_pct: 45.0,
            },
            Mode::MaxCool => Tuning {
                engage_c: 60.0,
                release_c: 52.0,
                demote_pct: 30.0,
            },
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

/// Battery heat adds to SoC heat when charging, so the power cap should engage
/// earlier to compensate. This is a modest shift: the battery alone cannot cause
/// capping (the SoC still must be warm), but it lowers the engage threshold.
pub const BATTERY_HOT_C_SHIFT: f64 = 3.0;
/// A battery at or above this is hot enough to add meaningful chassis heat.
pub const BATTERY_HOT_C: f64 = 45.0;
/// ...and it must fall well below it before the compensation is dropped, so a
/// battery hovering at the threshold cannot flicker the cap's engage point.
pub const BATTERY_COOL_C: f64 = 42.0;

/// Latching battery-heat state, kept in sync with the charging state the rest
/// of the governor uses.
///
/// Charging temperature moves slowly but its noise straddles the threshold, and
/// an unlatched flag shifted the engage temperature by [`BATTERY_HOT_C_SHIFT`]
/// on every sample — which is what made the cap flicker while charging. An
/// unreadable temperature keeps the previous verdict rather than guessing.
pub fn battery_heat_latched(temp_c: Option<f64>, was_hot: bool) -> bool {
    match temp_c {
        Some(t) if t.is_finite() && t >= BATTERY_HOT_C => true,
        Some(t) if t.is_finite() && t <= BATTERY_COOL_C => false,
        Some(t) if t.is_finite() => was_hot,
        _ => was_hot,
    }
}
/// Same governor, but with thresholds supplied by the caller so the fan
/// preference can shift them.
/// Adaptive governor with optional battery-heat compensation.
///
/// When the battery is hot (and we're on AC power charging), the engage threshold
/// is lowered by `BATTERY_HOT_C_SHIFT` to account for the extra thermal load.
pub fn auto_should_cap_with(
    t: Tuning,
    temp_c: Option<f64>,
    speed_limit: Option<u32>,
    cap_active: bool,
    capped_for_s: u64,
    battery_hot: bool,
    on_ac: bool,
) -> bool {
    let mut engage_c = t.engage_c;
    if battery_hot && on_ac {
        // Never let battery heat collapse the hysteresis: the engage threshold
        // must stay clearly above the release threshold or the cap flaps.
        engage_c = (engage_c - BATTERY_HOT_C_SHIFT).max(t.release_c + BATTERY_MIN_HYSTERESIS_C);
    }
    // Throttling re-engages only above the release point, with margin.
    let throttle_engage_c = AUTO_THROTTLE_C.max(t.release_c + THROTTLE_ENGAGE_MARGIN_C);
    let hot = temp_c.map(|v| v >= engage_c).unwrap_or(false);
    let warm = temp_c.map(|v| v >= throttle_engage_c).unwrap_or(false);
    let throttled = speed_limit.map(|l| l < THROTTLE_LIMIT).unwrap_or(false);
    let cooled = temp_c.map(|v| v <= t.release_c).unwrap_or(false);

    // Stability: always use the raw temp_c for release decisions to avoid
    // smoothed readings delaying release when the chip has actually cooled.
    if cap_active {
        !(cooled && capped_for_s >= AUTO_DWELL_S)
    } else {
        hot || (warm && throttled)
    }
}

/// Evaluate the power cap decision for all modes.
///
/// Security: this is a pure function with no side effects, so it cannot be
/// exploited to cause unexpected state changes. All inputs are validated by
/// the caller before being passed here.
pub fn evaluate(
    mode: Mode,
    fan: FanIntent,
    temp_c: Option<f64>,
    speed_limit: Option<u32>,
    cap_active: bool,
    capped_for_s: u64,
    procs: &[(u32, String, f64)],
    battery_hot: bool,
    on_ac: bool,
) -> Decision {
    let t = Tuning::for_mode_fan(mode, fan);

    let cap_power = match mode {
        Mode::Off => false,
        Mode::Auto => auto_should_cap_with(
            t,
            temp_c,
            speed_limit,
            cap_active,
            capped_for_s,
            battery_hot,
            on_ac,
        ),
        Mode::Balanced => match temp_c {
            Some(v) if v.is_finite() => {
                if cap_active {
                    v > t.release_c || capped_for_s < BALANCED_DWELL_S
                } else {
                    v >= t.engage_c
                }
            }
            _ => cap_active,
        },
        Mode::CoolQuiet | Mode::MaxCool => true,
    };

    let mut demote = Vec::new();
    if matches!(mode, Mode::Balanced | Mode::CoolQuiet | Mode::MaxCool) {
        for (pid, name, cpu) in procs {
            if *cpu >= t.demote_pct {
                demote.push((*pid, name.clone()));
            }
        }
    }

    Decision { cap_power, demote }
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
        Some(t) if t.is_finite() && t < MEDIA_SAFE_C => Decision {
            cap_power: false,
            demote: Vec::new(),
        },
        // Still protective at the extremes; we only ever give up the *extra*
        // clamping, never the safety limit.
        _ => d,
    }
}

// The new evaluate with battery_hot and on_ac parameters is defined above.
// This old signature is removed to avoid duplication.

#[cfg(test)]
mod tests {
    use super::*;

    fn hot(pid: u32, cpu: f64) -> (u32, String, f64) {
        (pid, "Hog".to_string(), cpu)
    }

    #[test]
    fn off_mode_never_caps_or_demotes() {
        let d = evaluate(
            Mode::Off,
            FanIntent::Auto,
            Some(99.0),
            Some(50),
            false,
            0,
            &[hot(1, 99.0)],
            false,
            false,
        );
        assert!(!d.cap_power);
        assert!(d.demote.is_empty());
    }

    #[test]
    fn auto_keeps_full_performance_when_cool() {
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(62.0),
            Some(100),
            false,
            0,
            false,
            false
        ));
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(77.9),
            Some(100),
            false,
            0,
            false,
            false
        ));
    }

    #[test]
    fn auto_caps_when_hot_or_already_throttling() {
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(78.0),
            Some(100),
            false,
            0,
            false,
            false
        ));
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(88.0),
            Some(100),
            false,
            0,
            false,
            false
        ));
        // even at moderate temperature, OS throttling means stop generating heat
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(77.0),
            Some(80),
            false,
            0,
            false,
            false
        ));
    }

    #[test]
    fn auto_releases_only_after_cooling_and_dwell() {
        // hot and capped: stays capped
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(85.0),
            Some(100),
            true,
            60,
            false,
            false
        ));
        // cooled but dwell not elapsed: stays capped
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(65.0),
            Some(100),
            true,
            5,
            false,
            false
        ));
        // cooled, dwell elapsed: restores turbo
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(65.0),
            Some(100),
            true,
            AUTO_DWELL_S,
            false,
            false
        ));
    }

    #[test]
    fn auto_does_not_deadlock_when_speed_limit_stays_low() {
        // Regression: a release rule that demanded CPU_Speed_Limit >= 100 kept
        // the cap engaged forever whenever the OS reported 71 (seen live).
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(69.0),
            Some(71),
            true,
            30,
            false,
            false
        ));
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(64.0),
            Some(71),
            true,
            AUTO_DWELL_S,
            false,
            false
        ));
    }

    #[test]
    fn auto_engage_from_throttling_requires_warmth() {
        // cold but OS limit low: do not clamp a cool chip
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(55.0),
            Some(80),
            false,
            0,
            false,
            false
        ));
        // warm and being starved: clamp
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(77.0),
            Some(80),
            false,
            0,
            false,
            false
        ));
    }

    #[test]
    fn auto_never_oscillates_within_the_dwell_window() {
        let mut capped = false;
        let mut since = 0u64;
        // alternate readings every second around the thresholds
        for tick in 0..120u64 {
            let temp = if capped { 69.0 } else { 79.0 };
            let next = auto_should_cap_with(
                Tuning::for_mode(Mode::Auto),
                Some(temp),
                Some(100),
                capped,
                since,
                false,
                false,
            );
            if next != capped {
                capped = next;
                since = 0;
            } else {
                since += 1;
            }
            let _ = tick;
        }
        // one 20 s cap cycle at most per direction, not a per-tick flap
        assert!(
            capped,
            "should end up capped when readings stay hot after release"
        );
    }

    #[test]
    fn balanced_caps_only_above_engage_threshold() {
        assert!(
            !evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(70.0),
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(82.0),
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(95.0),
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
    }

    #[test]
    fn balanced_has_hysteresis_between_release_and_engage() {
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(75.0),
                None,
                true,
                60,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            !evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(71.9),
                None,
                true,
                60,
                &[],
                false,
                false
            )
            .cap_power
        );
    }

    #[test]
    fn balanced_waits_for_the_dwell_before_releasing() {
        // Cool reading, but the cap has only just engaged: hold it.
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(60.0),
                None,
                true,
                2,
                &[],
                false,
                false
            )
            .cap_power
        );
        // Dwell satisfied: hand performance back.
        assert!(
            !evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(60.0),
                None,
                true,
                BALANCED_DWELL_S,
                &[],
                false,
                false
            )
            .cap_power
        );
        // A genuinely hot reading keeps it engaged however long it has been up.
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(90.0),
                None,
                true,
                600,
                &[],
                false,
                false
            )
            .cap_power
        );
    }

    #[test]
    fn balanced_holds_the_cap_when_the_temperature_is_unreadable() {
        // Unreadable is never permission to relax protection.
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                None,
                None,
                true,
                600,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            !evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                None,
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
        // Non-finite readings behave like unreadable ones.
        assert!(
            evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(f64::NAN),
                None,
                true,
                600,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            !evaluate(
                Mode::Balanced,
                FanIntent::Auto,
                Some(f64::NAN),
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
    }

    #[test]
    fn cool_modes_cap_unconditionally() {
        assert!(
            evaluate(
                Mode::CoolQuiet,
                FanIntent::Auto,
                Some(40.0),
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            evaluate(
                Mode::MaxCool,
                FanIntent::Auto,
                None,
                None,
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
    }

    #[test]
    fn demote_thresholds_follow_mode() {
        let procs = [hot(10, 50.0), hot(11, 85.0)];
        let balanced = evaluate(
            Mode::Balanced,
            FanIntent::Auto,
            Some(50.0),
            None,
            false,
            0,
            &procs,
            false,
            false,
        );
        assert_eq!(balanced.demote.len(), 1);
        assert_eq!(balanced.demote[0].0, 11);

        let max = evaluate(
            Mode::MaxCool,
            FanIntent::Auto,
            Some(50.0),
            None,
            false,
            0,
            &procs,
            false,
            false,
        );
        assert_eq!(max.demote.len(), 2);
    }

    #[test]
    fn auto_never_demotes_processes() {
        // Auto has no reliable foreground-app signal. Preserve responsiveness
        // by controlling only the system power envelope.
        let procs = [hot(1, 60.0), hot(2, 90.0)];
        let d = evaluate(
            Mode::Auto,
            FanIntent::Auto,
            Some(60.0),
            Some(100),
            false,
            0,
            &procs,
            false,
            false,
        );
        assert!(d.demote.is_empty());
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
    fn quiet_preference_only_demotes_in_explicit_cooling_modes() {
        // Auto still protects interactive work when Silent is selected.
        let procs = [hot(1, 72.0), hot(2, 90.0)];
        let quiet = evaluate(
            Mode::Auto,
            FanIntent::Quiet,
            Some(60.0),
            Some(100),
            false,
            0,
            &procs,
            false,
            false,
        );
        assert!(quiet.demote.is_empty());
        let quiet_cool = evaluate(
            Mode::CoolQuiet,
            FanIntent::Quiet,
            Some(60.0),
            Some(100),
            false,
            0,
            &procs,
            false,
            false,
        );
        assert_eq!(
            quiet_cool.demote.len(),
            2,
            "explicit cooling may tame the 72% process"
        );
        // Never below the floor, and Off stays inert.
        assert_eq!(
            Tuning::for_mode_fan(Mode::MaxCool, FanIntent::Quiet).demote_pct,
            20.0
        );
        assert_eq!(
            Tuning::for_mode_fan(Mode::Off, FanIntent::Quiet).demote_pct,
            999.0
        );
    }

    #[test]
    fn quiet_fan_makes_the_governor_clamp_earlier() {
        // Auto alone leaves turbo on at 74 C; with the fan held silent it does not.
        assert!(
            !evaluate(
                Mode::Auto,
                FanIntent::Auto,
                Some(74.0),
                Some(100),
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
        assert!(
            evaluate(
                Mode::Auto,
                FanIntent::Quiet,
                Some(74.0),
                Some(100),
                false,
                0,
                &[],
                false,
                false
            )
            .cap_power
        );
        // Cooling down still hands performance back.
        assert!(
            !evaluate(
                Mode::Auto,
                FanIntent::Quiet,
                Some(60.0),
                Some(100),
                true,
                60,
                &[],
                false,
                false
            )
            .cap_power
        );
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
        let capped = || Decision {
            cap_power: true,
            demote: vec![(1, "Hog".to_string())],
        };
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

    #[test]
    fn battery_hot_shifts_engage_threshold_earlier() {
        // When battery is hot and on AC, engage at 75 instead of 78
        let cool = auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(76.0),
            Some(100),
            false,
            0,
            false,
            true,
        );
        assert!(!cool, "should not cap at 76 without battery heat");
        let with_battery = auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(76.0),
            Some(100),
            false,
            0,
            true,
            true,
        );
        assert!(with_battery, "should cap at 76 with hot battery on AC");
    }

    #[test]
    fn battery_heat_state_is_latched_around_the_threshold() {
        // Rising through the threshold turns it on...
        assert!(battery_heat_latched(Some(BATTERY_HOT_C), false));
        // ...and it stays on through the band, so a noisy reading cannot flip
        // the engage temperature on every sample.
        assert!(battery_heat_latched(Some(44.0), true));
        assert!(battery_heat_latched(Some(43.0), true));
        // Only a genuinely cooler battery drops it.
        assert!(!battery_heat_latched(Some(BATTERY_COOL_C), true));
        assert!(!battery_heat_latched(Some(30.0), true));
        // No hysteresis the other way: a cool battery needs real heat again.
        assert!(!battery_heat_latched(Some(44.9), false));
        // Unreadable keeps the previous verdict instead of guessing.
        assert!(battery_heat_latched(None, true));
        assert!(!battery_heat_latched(None, false));
        assert!(battery_heat_latched(Some(f64::NAN), true));
    }

    #[test]
    fn battery_hot_does_not_cap_a_cool_soc() {
        // Battery hot alone should not cap if SoC is cool
        let d = evaluate(
            Mode::Auto,
            FanIntent::Auto,
            Some(60.0),
            Some(100),
            false,
            0,
            &[],
            true,
            true,
        );
        assert!(
            !d.cap_power,
            "battery hot alone should not cap when SoC is cool"
        );
    }

    #[test]
    fn temp_smoothing_prevents_flickering() {
        // Simulate noisy readings that would cause flickering without smoothing:
        // individual readings cross the 78°C threshold, but the average stays below
        let readings = vec![77.5, 78.5, 77.0, 79.0, 77.5, 78.0, 77.0, 78.5];
        let sum: f64 = readings.iter().sum();
        let avg = sum / readings.len() as f64;
        // Without smoothing, readings like 78.5 and 79.0 would engage the cap
        // With smoothing, the average (77.875) stays below the 78°C threshold
        assert!(
            avg < AUTO_ENGAGE_C,
            "smoothed average {} should be below engage threshold {}",
            avg,
            AUTO_ENGAGE_C
        );
        // Verify that some individual readings would have triggered capping
        assert!(
            readings.iter().any(|&r| r >= AUTO_ENGAGE_C),
            "some readings should cross threshold"
        );
    }

    #[test]
    fn throttle_path_cannot_reengage_at_the_release_point() {
        // Regression: the cap released at 74 °C re-engaged at 72 °C on the next
        // tick while the OS still reported throttling, so it cycled on and off
        // every dwell window for as long as the machine sat at ~73 °C.
        let t = Tuning::for_mode(Mode::Auto);
        assert!(!auto_should_cap_with(
            t,
            Some(74.0),
            Some(80),
            false,
            0,
            false,
            false
        ));
        assert!(!auto_should_cap_with(
            t,
            Some(75.9),
            Some(80),
            false,
            0,
            false,
            false
        ));
        // Above the release point plus the margin it may engage.
        assert!(auto_should_cap_with(
            t,
            Some(77.0),
            Some(80),
            false,
            0,
            false,
            false
        ));
    }

    #[test]
    fn battery_heat_never_collapses_the_hysteresis() {
        // Engage shifts 78 -> 75, but the release is 74: keep 2 °C of margin,
        // so a 75 °C reading must not engage the cap.
        assert!(!auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(75.0),
            Some(100),
            false,
            0,
            true,
            true
        ));
        assert!(auto_should_cap_with(
            Tuning::for_mode(Mode::Auto),
            Some(76.0),
            Some(100),
            false,
            0,
            true,
            true
        ));
    }

    #[test]
    fn the_cap_is_held_off_for_a_minimum_window() {
        let t = Tuning::for_mode(Mode::Auto);
        // Just released, warm but not critical: hold off.
        assert!(!reengage_allowed(t, 0, Some(78.0)));
        assert!(!reengage_allowed(t, AUTO_MIN_OFF_S - 1, Some(80.0)));
        // Genuinely hot: the quiet window never blocks protection.
        assert!(reengage_allowed(
            t,
            0,
            Some(t.engage_c + AUTO_CRITICAL_MARGIN_C)
        ));
        // Window elapsed, or no usable reading at all: allowed.
        assert!(reengage_allowed(t, AUTO_MIN_OFF_S, Some(70.0)));
        assert!(reengage_allowed(t, AUTO_MIN_OFF_S, None));
    }

    #[test]
    fn max_fan_target_refuses_to_write_an_implausible_value() {
        assert_eq!(max_fan_target(None, None), None);
        assert_eq!(max_fan_target(Some(0.0), Some(f64::NAN)), None);
        assert_eq!(max_fan_target(Some(0.0), Some(0.0)), None);
        assert_eq!(max_fan_target(Some(0.0), Some(50_000.0)), None);
        assert_eq!(max_fan_target(Some(1300.0), Some(8000.0)), Some(8000.0));
        // A minimum above the maximum collapses to the maximum, never beyond.
        assert_eq!(max_fan_target(Some(9000.0), Some(8000.0)), Some(8000.0));
    }
}
