//! Thermal-aware RAM governor.
//!
//! [`crate::memory`] owns the low-level brakes: *is this purge safe?* This
//! module owns the question above it: **which lever do we pull, and when does
//! pulling one cost more heat than it saves?**
//!
//! The physics the decision table encodes:
//!
//! * **Dropping cache** is one bounded disk burst with no user-visible loss.
//!   It is the cheapest lever the app has, so it goes first.
//! * **Compressor churn and swap-in** are CPU and disk work that turns straight
//!   into die temperature, and they keep going for as long as the working set
//!   is bigger than RAM. They are the reason memory pressure is a *thermal*
//!   problem on an Air with no fan authority.
//! * **Asking an idle app to quit** is the only lever that removes the cause,
//!   but it is the one the user notices. It is therefore reserved for real
//!   memory pressure, or for a machine that is hot *and* already churning.
//!
//! Two behaviours fall out of that table and are worth stating up front:
//!
//! 1. A **cool** machine with warning-level pressure is left alone — macOS is
//!    already compressing, there is no heat to avoid, and no user-visible cost
//!    is justified.
//! 2. A **hot** machine does not spend I/O heat on warning-level pressure
//!    unless the compressor itself is already burning more CPU than the purge
//!    would. Protection needs positive evidence, in both directions.
//!
//! Everything here is a pure function of its inputs so the table can be tested
//! without a hot chip, real pressure or a live compressor.

use crate::memory::{self, Pressure, Trend};
use crate::policy::Mode;

// ---------------------------------------------------------------------------
// Thermal context
// ---------------------------------------------------------------------------

/// Where the die sits, in the bands the cap policy already uses. Ordered, so a
/// rule can say "hot or worse" without listing every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalBand {
    Cool,
    Warm,
    Hot,
    Critical,
}

impl ThermalBand {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThermalBand::Cool => "cool",
            ThermalBand::Warm => "warm",
            ThermalBand::Hot => "hot",
            ThermalBand::Critical => "critical",
        }
    }
}

/// Below this the chassis has headroom: cheap I/O is affordable and there is no
/// heat to shed.
pub const WARM_C: f64 = 70.0;
/// Matches the Auto governor's engage point. At or above this the machine is
/// already shedding load, so memory work must be worth its own heat.
pub const HOT_C: f64 = 78.0;
/// Above this the chassis is saturated; only a memory emergency justifies an
/// extra I/O burst.
pub const CRITICAL_C: f64 = 85.0;

/// Classify the die temperature.
///
/// An unreadable or non-finite reading is **never** treated as cool: a missing
/// sensor must not unlock work that is only justified by thermal headroom.
/// OS throttling is positive evidence on its own and floors the band at Hot,
/// because macOS only throttles a chip it cannot cool.
pub fn thermal_band(cpu_temp: Option<f64>, os_throttling: bool) -> ThermalBand {
    let mut band = match cpu_temp {
        Some(t) if t.is_finite() => {
            if t >= CRITICAL_C {
                ThermalBand::Critical
            } else if t >= HOT_C {
                ThermalBand::Hot
            } else if t >= WARM_C {
                ThermalBand::Warm
            } else {
                ThermalBand::Cool
            }
        }
        _ => ThermalBand::Warm,
    };
    if os_throttling && band < ThermalBand::Hot {
        band = ThermalBand::Hot;
    }
    band
}

// ---------------------------------------------------------------------------
// Churn: the part of memory management that actually makes heat
// ---------------------------------------------------------------------------

/// Cumulative Mach VM counters. They only ever go up, so a drop means the
/// machine rebooted or the read is garbage and must not produce a rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub compressions: u64,
    pub decompressions: u64,
    pub swapins: u64,
    pub swapouts: u64,
}

impl Counters {
    pub fn of(compressions: u64, decompressions: u64, swapins: u64, swapouts: u64) -> Counters {
        Counters {
            compressions,
            decompressions,
            swapins,
            swapouts,
        }
    }
}

/// Page faults per second that the compressor and swap are causing.
///
/// A decompression is a CPU page fault plus usually a recompression later; a
/// swap-in is the same plus a disk read. A page is 4 KiB on this Intel Air, so
/// 200 pages/s is roughly 0.8 MB/s and 1500 pages/s is 6 MB/s of churn.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Churn {
    pub compressions_per_s: f64,
    pub decompressions_per_s: f64,
    pub swapins_per_s: f64,
}

/// Below this the compressor is doing ordinary background work.
pub const CHURN_NOTABLE_PER_S: f64 = 200.0;
/// Above this the working set is clearly bigger than RAM and the resulting
/// fault traffic is a real heat source.
pub const CHURN_HEAVY_PER_S: f64 = 1500.0;

impl Churn {
    /// Rate between two samples. A backwards counter, a non-finite or a
    /// non-positive interval yields no churn rather than a nonsense spike.
    pub fn between(before: &Counters, after: &Counters, dt_s: f64) -> Churn {
        if !dt_s.is_finite() || dt_s <= 0.0 {
            return Churn::default();
        }
        let rate = |a: u64, b: u64| if b >= a { (b - a) as f64 / dt_s } else { 0.0 };
        Churn {
            compressions_per_s: rate(before.compressions, after.compressions),
            decompressions_per_s: rate(before.decompressions, after.decompressions),
            swapins_per_s: rate(before.swapins, after.swapins),
        }
    }

    /// Fault traffic that becomes heat. A swap-in costs a decompression *and* a
    /// disk read, so it counts double.
    pub fn heat_load(&self) -> f64 {
        self.decompressions_per_s + self.swapins_per_s * 2.0
    }

    pub fn is_notable(&self) -> bool {
        self.heat_load() >= CHURN_NOTABLE_PER_S
    }

    pub fn is_heavy(&self) -> bool {
        self.heat_load() >= CHURN_HEAVY_PER_S
    }
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

/// What the governor asks for. Ordered by how much it costs the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// Watch. The kernel's own compressor is the right tool here.
    Hold,
    /// One bounded cache drop: no app is touched.
    DropCache,
    /// Ask genuinely idle apps to quit. Executed by the unprivileged UI.
    QuitIdleApps,
}

impl Rung {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rung::Hold => "hold",
            Rung::DropCache => "reclaim",
            Rung::QuitIdleApps => "trim",
        }
    }
}

/// Why the governor landed on its rung. Kept as data so the UI can explain the
/// decision instead of showing an opaque temperature or percentage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Disabled,
    ModeOff,
    Comfortable,
    /// Pressure is real and the cache is worth dropping.
    CacheFirst,
    /// Nothing is wrong yet, but the compressor is climbing: drop cache while
    /// it is still cheap.
    PreemptiveCache,
    /// Pressure is real and there is nothing worth dropping.
    NoCacheToDrop,
    /// Swap is already saturated: more I/O would make the stall worse.
    SwapThrashing,
    /// The chip is hot and pressure is only advisory: a purge would spend heat
    /// to fix something the kernel can still absorb.
    HotHold,
    /// The last purge measurably freed nothing; repeating it is pure I/O heat.
    PurgeFutile,
    /// A purge is available but rate-limited.
    Cooldown,
    HourlyCap,
    /// Memory is critical; an idle app is the only lever left.
    MemoryEmergency,
    /// This mode trims as soon as pressure is high, not just when it is
    /// critical.
    ModePressure,
    /// The machine is hot *and* the compressor is churning: quitting an idle
    /// app removes the cause of the heat, not just the symptom.
    HeatRelief,
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::Disabled => "disabled",
            Reason::ModeOff => "mode-off",
            Reason::Comfortable => "comfortable",
            Reason::CacheFirst => "cache-first",
            Reason::PreemptiveCache => "preemptive-cache",
            Reason::NoCacheToDrop => "no-cache",
            Reason::SwapThrashing => "swap-thrashing",
            Reason::HotHold => "hot-hold",
            Reason::PurgeFutile => "purge-futile",
            Reason::Cooldown => "cooldown",
            Reason::HourlyCap => "hourly-cap",
            Reason::MemoryEmergency => "memory-emergency",
            Reason::ModePressure => "mode-pressure",
            Reason::HeatRelief => "heat-relief",
        }
    }

    /// One sentence for the UI. Honest about what is *not* being done.
    pub fn detail(&self) -> &'static str {
        match self {
            Reason::Disabled => "Cache reclaim is switched off.",
            Reason::ModeOff => "Off mode: monitoring only, nothing is reclaimed.",
            Reason::Comfortable => "Memory is comfortable; the kernel is handling it.",
            Reason::CacheFirst => "Reclaimable cache is dropped before any app is touched.",
            Reason::PreemptiveCache => "Compressor is climbing; cache is being dropped early, while it is cheap.",
            Reason::NoCacheToDrop => "No cache worth dropping, and the chip is cool enough to let the kernel compress.",
            Reason::SwapThrashing => "Swap is already saturated; adding I/O would make the stall worse.",
            Reason::HotHold => "Chip is hot; a purge is deferred because pressure is only advisory.",
            Reason::PurgeFutile => "The last purge freed almost nothing; not repeating it.",
            Reason::Cooldown => "Purge is inside its cooldown window.",
            Reason::HourlyCap => "Purge already ran twice this hour.",
            Reason::MemoryEmergency => "Memory is critical; asking an idle app to quit is the only lever left.",
            Reason::ModePressure => "This mode trims idle apps once pressure is high, not only when it is critical.",
            Reason::HeatRelief => "Chip is hot and the compressor is churning; an idle app is the cheapest way to shed the load.",
        }
    }
}

/// The governor's answer. `band` and `heat_load` are echoed so the UI and the
/// log can explain a decision without re-sampling anything.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub rung: Rung,
    pub reason: Reason,
    pub band: ThermalBand,
    pub heat_load: f64,
}

impl Default for Decision {
    fn default() -> Self {
        Decision {
            rung: Rung::Hold,
            reason: Reason::Comfortable,
            band: ThermalBand::Warm,
            heat_load: 0.0,
        }
    }
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        self.rung.as_str()
    }

    pub fn detail(&self) -> &'static str {
        self.reason.detail()
    }
}

/// Cache growth (compressor MB/s) that is worth acting on before pressure
/// itself appears. 8 MB/s is ~0.5 GB/min: a machine that will be in trouble
/// inside the next few minutes.
pub const TREND_ALARM_MB_S: f64 = 8.0;
/// A purge that frees less than this bought nothing; the cache was a mirage
/// (dirty pages waiting on writeback, or memory that was not really
/// reclaimable), so the next attempt must not repeat it.
pub const MIN_PURGE_GAIN_MB: f64 = 128.0;
/// How long a futile purge keeps the cache lever closed.
pub const FUTILE_WINDOW_S: u64 = 900;

/// Per-mode tolerances for the RAM ladder.
///
/// The same signal means different things in different modes, exactly as it
/// does for the power cap: `Auto` and `Balanced` must not cost the user an app
/// for warning-level pressure, while `Cool & Quiet` and `Max Cool` are explicit
/// requests to trade convenience for temperature and therefore act earlier.
///
/// A table, not a chain of conditionals: a new mode is one entry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// `false` for `Mode::Off`: monitoring only.
    pub active: bool,
    /// At or above this band, churn is treated as a heat source to remove.
    pub act_band: ThermalBand,
    /// Pressure at which an idle app may be asked to quit.
    pub trim_pressure: Pressure,
    /// Multiplier on the purge cooldown. Below 1.0 is more permissive.
    pub cooldown_scale: f64,
    /// Purge budget per rolling hour.
    pub max_per_hour: u32,
}

impl Tuning {
    pub fn for_mode(mode: Mode) -> Tuning {
        match mode {
            Mode::Off => Tuning {
                active: false,
                act_band: ThermalBand::Hot,
                trim_pressure: Pressure::Critical,
                cooldown_scale: 1.0,
                max_per_hour: memory::MAX_PER_HOUR,
            },
            // Performance first: only real pressure or real heat is acted on.
            Mode::Auto | Mode::Balanced => Tuning {
                active: true,
                act_band: ThermalBand::Hot,
                trim_pressure: Pressure::Critical,
                cooldown_scale: 1.0,
                max_per_hour: memory::MAX_PER_HOUR,
            },
            // The user asked for cool: warn-level pressure and a merely warm
            // chip are enough, and the purge budget is larger.
            Mode::CoolQuiet => Tuning {
                active: true,
                act_band: ThermalBand::Warm,
                trim_pressure: Pressure::High,
                cooldown_scale: 0.5,
                max_per_hour: memory::MAX_PER_HOUR + 1,
            },
            Mode::MaxCool => Tuning {
                active: true,
                act_band: ThermalBand::Warm,
                trim_pressure: Pressure::High,
                cooldown_scale: 0.25,
                max_per_hour: memory::MAX_PER_HOUR + 2,
            },
        }
    }

    /// The effective purge cooldown for this mode.
    pub fn cooldown_s(&self) -> u64 {
        (memory::COOLDOWN_S as f64 * self.cooldown_scale).round() as u64
    }
}

/// Everything the governor is allowed to look at. Grouped so a new signal is
/// one field, not a new parameter threaded through every caller.
pub struct Inputs {
    /// User setting; on by default.
    pub enabled: bool,
    /// Mode the user picked. Off steps aside entirely; the rest differ in how
    /// much convenience they are allowed to spend on temperature.
    pub mode: Mode,
    pub pressure: Pressure,
    pub band: ThermalBand,
    pub churn: Churn,
    pub trend: Trend,
    /// Purgeable + file-backed cache, in MB.
    pub reclaimable_mb: f64,
    pub swap_used_pct: f64,
    /// Seconds since the last purge, `u64::MAX` when there never was one.
    pub purge_since_s: u64,
    pub purges_last_hour: u32,
    /// What the last purge actually freed, and how long ago it ran. `None`
    /// means no purge has been measured yet.
    pub last_purge: Option<(f64, u64)>,
}

impl Inputs {
    /// The measured heat cost of leaving the compressor to its own devices.
    pub fn heat_load(&self) -> f64 {
        self.churn.heat_load()
    }

    /// A purge that freed almost nothing is not worth repeating for a while.
    fn purge_is_futile(&self) -> bool {
        match self.last_purge {
            Some((freed_mb, ago_s)) => freed_mb < MIN_PURGE_GAIN_MB && ago_s < FUTILE_WINDOW_S,
            None => false,
        }
    }

    /// Is the cache a lever at all right now?
    fn cache_lever(&self, t: &Tuning) -> Result<(), Reason> {
        if self.reclaimable_mb < memory::MIN_RECLAIM_MB {
            return Err(Reason::NoCacheToDrop);
        }
        if self.purge_is_futile() {
            return Err(Reason::PurgeFutile);
        }
        if self.swap_used_pct >= memory::SWAP_THRASH_PCT {
            return Err(Reason::SwapThrashing);
        }
        if self.purge_since_s < t.cooldown_s() {
            return Err(Reason::Cooldown);
        }
        if self.purges_last_hour >= t.max_per_hour {
            return Err(Reason::HourlyCap);
        }
        // Thermal brake: at a saturated chassis, only a memory emergency or a
        // compressor that is already burning more CPU than the purge would
        // justifies another I/O burst.
        if self.band == ThermalBand::Critical
            && self.pressure != Pressure::Critical
            && !self.churn.is_heavy()
        {
            return Err(Reason::HotHold);
        }
        Ok(())
    }
}

/// Decide the next step.
///
/// Written as an ordered ladder rather than a matrix of conditions so each
/// rule is one early return with one reason, and adding a rung later means
/// adding a rule instead of editing five branches.
pub fn decide(i: &Inputs) -> Decision {
    let t = Tuning::for_mode(i.mode);
    let base = |rung: Rung, reason: Reason| Decision {
        rung,
        reason,
        band: i.band,
        heat_load: i.heat_load(),
    };

    // 0. The user and Off mode come before any measurement.
    if !i.enabled {
        return base(Rung::Hold, Reason::Disabled);
    }
    if !t.active {
        return base(Rung::Hold, Reason::ModeOff);
    }

    let churning = i.churn.is_notable();
    let climbing = i.trend.compressed_mb_per_s >= TREND_ALARM_MB_S;

    // 1. Comfortable, quiet, stable: the kernel is doing the right thing and
    //    any I/O we add is pure heat. This holds in every mode, including the
    //    aggressive ones: an idle machine has nothing to shed.
    if i.pressure == Pressure::Normal && !churning && !climbing {
        return base(Rung::Hold, Reason::Comfortable);
    }

    // 2. Cache first — including preemptively, while the machine is still only
    //    warm and the compressor is starting to climb. Cheap work done early
    //    replaces expensive work done late.
    let cache_wanted = i.pressure != Pressure::Normal || churning || climbing;
    let cache_blocked = match i.cache_lever(&t) {
        Ok(()) if cache_wanted => {
            let reason = if i.pressure == Pressure::Normal {
                Reason::PreemptiveCache
            } else {
                Reason::CacheFirst
            };
            return base(Rung::DropCache, reason);
        }
        Ok(()) => None,
        Err(reason) => Some(reason),
    };

    // 3. Nothing safe to drop. Only two things justify costing the user an app:
    //    a memory emergency (or a mode that volunteered to trim early), or a
    //    hot chassis whose compressor is the heat.
    if i.pressure == Pressure::Critical {
        return base(Rung::QuitIdleApps, Reason::MemoryEmergency);
    }
    if i.pressure >= t.trim_pressure {
        return base(Rung::QuitIdleApps, Reason::ModePressure);
    }
    if i.band >= t.act_band && churning {
        return base(Rung::QuitIdleApps, Reason::HeatRelief);
    }

    // 4. Everything else is a hold. Reporting the *specific* brake (swap
    //    thrash, cooldown, a hot chip) is what makes the UI trustworthy.
    let reason = cache_blocked.unwrap_or(Reason::NoCacheToDrop);
    base(Rung::Hold, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Inputs {
        Inputs {
            enabled: true,
            mode: Mode::Auto,
            pressure: Pressure::High,
            band: ThermalBand::Warm,
            churn: Churn::default(),
            trend: Trend::default(),
            reclaimable_mb: 2048.0,
            swap_used_pct: 20.0,
            purge_since_s: u64::MAX,
            purges_last_hour: 0,
            last_purge: None,
        }
    }

    fn churn(decompressions_per_s: f64, swapins_per_s: f64) -> Churn {
        Churn {
            compressions_per_s: 0.0,
            decompressions_per_s,
            swapins_per_s,
        }
    }

    // ---- thermal band -----------------------------------------------------

    #[test]
    fn bands_follow_the_cap_policy() {
        assert_eq!(thermal_band(Some(60.0), false), ThermalBand::Cool);
        assert_eq!(thermal_band(Some(70.0), false), ThermalBand::Warm);
        assert_eq!(thermal_band(Some(78.0), false), ThermalBand::Hot);
        assert_eq!(thermal_band(Some(85.0), false), ThermalBand::Critical);
        // Unreadable is never "cool": that would unlock work reserved for
        // machines with headroom.
        assert_eq!(thermal_band(None, false), ThermalBand::Warm);
        assert_eq!(thermal_band(Some(f64::NAN), false), ThermalBand::Warm);
    }

    #[test]
    fn os_throttling_is_positive_evidence_of_heat() {
        // A cool-looking sample while macOS is starving the CPU must not
        // unlock the "there is headroom" path.
        assert_eq!(thermal_band(Some(60.0), true), ThermalBand::Hot);
        // It never *downgrades* a hotter, measured band.
        assert_eq!(thermal_band(Some(90.0), true), ThermalBand::Critical);
    }

    // ---- churn ------------------------------------------------------------

    #[test]
    fn churn_is_a_rate_between_two_counters() {
        let before = Counters::of(0, 0, 0, 0);
        let after = Counters::of(2000, 1000, 300, 100);
        let c = Churn::between(&before, &after, 10.0);
        assert!((c.compressions_per_s - 200.0).abs() < 1e-9);
        assert!((c.decompressions_per_s - 100.0).abs() < 1e-9);
        assert!((c.swapins_per_s - 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_restarted_counter_or_a_bad_interval_is_not_a_spike() {
        let before = Counters::of(10_000, 10_000, 10_000, 10_000);
        let after = Counters::of(1, 1, 1, 1);
        assert_eq!(Churn::between(&before, &after, 2.0), Churn::default());
        let a = Counters::of(0, 0, 0, 0);
        let b = Counters::of(100, 100, 100, 100);
        assert_eq!(Churn::between(&a, &b, 0.0), Churn::default());
        assert_eq!(Churn::between(&a, &b, f64::NAN), Churn::default());
        assert_eq!(Churn::between(&a, &b, -2.0), Churn::default());
    }

    #[test]
    fn a_swap_in_costs_twice_a_decompression() {
        // Same page count, but the swap path also reads the disk.
        assert_eq!(churn(100.0, 0.0).heat_load(), 100.0);
        assert_eq!(churn(0.0, 100.0).heat_load(), 200.0);
        assert!(!churn(CHURN_NOTABLE_PER_S - 1.0, 0.0).is_notable());
        assert!(churn(CHURN_NOTABLE_PER_S, 0.0).is_notable());
        assert!(churn(0.0, CHURN_HEAVY_PER_S / 2.0).is_heavy());
    }

    // ---- ladder -----------------------------------------------------------

    #[test]
    fn nothing_happens_without_pressure_churn_or_a_trend() {
        let mut i = inputs();
        i.pressure = Pressure::Normal;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::Comfortable);
    }

    #[test]
    fn the_toggle_and_off_mode_come_first() {
        let mut i = inputs();
        i.enabled = false;
        assert_eq!(decide(&i).reason, Reason::Disabled);
        i.enabled = true;
        i.mode = Mode::Off;
        assert_eq!(decide(&i).reason, Reason::ModeOff);
        // Even with critical pressure: Off means Off.
        i.pressure = Pressure::Critical;
        assert_eq!(decide(&i).rung, Rung::Hold);
    }

    #[test]
    fn the_performance_modes_protect_the_user() {
        // Auto and Balanced must not cost an app for warning-level pressure on
        // a chip that is not hot.
        for mode in [Mode::Auto, Mode::Balanced] {
            let mut i = inputs();
            i.mode = mode;
            i.reclaimable_mb = 0.0;
            i.band = ThermalBand::Warm;
            let d = decide(&i);
            assert_eq!(d.rung, Rung::Hold, "{:?} trimmed without cause", mode);
        }
    }

    #[test]
    fn the_cooling_modes_volunteer_to_act_earlier() {
        // Same signal, different contract: Cool & Quiet and Max Cool were
        // chosen to spend convenience on temperature.
        for mode in [Mode::CoolQuiet, Mode::MaxCool] {
            let mut i = inputs();
            i.mode = mode;
            i.reclaimable_mb = 0.0;
            i.band = ThermalBand::Warm;
            let d = decide(&i);
            assert_eq!(d.rung, Rung::QuitIdleApps, "{:?} should trim", mode);
            assert_eq!(d.reason, Reason::ModePressure);
        }
        // An idle, comfortable machine is still left alone in every mode.
        for mode in [Mode::CoolQuiet, Mode::MaxCool] {
            let mut i = inputs();
            i.mode = mode;
            i.pressure = Pressure::Normal;
            i.reclaimable_mb = 0.0;
            assert_eq!(
                decide(&i).rung,
                Rung::Hold,
                "{:?} acted on a comfortable machine",
                mode
            );
        }
    }

    #[test]
    fn cooling_modes_have_a_larger_but_still_bounded_purge_budget() {
        let auto = Tuning::for_mode(Mode::Auto);
        let quiet = Tuning::for_mode(Mode::CoolQuiet);
        let max = Tuning::for_mode(Mode::MaxCool);
        assert_eq!(auto.cooldown_s(), memory::COOLDOWN_S);
        assert!(quiet.cooldown_s() < auto.cooldown_s());
        assert!(max.cooldown_s() <= quiet.cooldown_s());
        assert!(quiet.max_per_hour > auto.max_per_hour);
        assert!(max.max_per_hour >= quiet.max_per_hour);
        // Still bounded: a root daemon must never be able to purge in a loop.
        assert!(max.max_per_hour <= memory::MAX_PER_HOUR + 2);

        let mut i = inputs();
        i.mode = Mode::MaxCool;
        i.purge_since_s = max.cooldown_s();
        assert_eq!(decide(&i).rung, Rung::DropCache);
        // One second short of the (shorter) Max Cool cooldown: the purge is
        // blocked and the cooling mode escalates instead of waiting.
        i.purge_since_s = max.cooldown_s() - 1;
        assert_eq!(decide(&i).reason, Reason::ModePressure);
        // Auto, at the same instant, has not even left its own cooldown.
        i.mode = Mode::Auto;
        assert_eq!(decide(&i).reason, Reason::Cooldown);
    }

    #[test]
    fn cache_is_dropped_before_any_app_is_touched() {
        let d = decide(&inputs());
        assert_eq!(d.rung, Rung::DropCache);
        assert_eq!(d.reason, Reason::CacheFirst);
        // ... and that holds even at a critical pressure: the cheapest lever
        // that can work, works.
        let mut i = inputs();
        i.pressure = Pressure::Critical;
        assert_eq!(decide(&i).rung, Rung::DropCache);
    }

    #[test]
    fn a_climbing_compressor_is_stopped_early_while_the_chip_is_warm() {
        let mut i = inputs();
        i.pressure = Pressure::Normal;
        i.churn = churn(CHURN_NOTABLE_PER_S, 0.0);
        let d = decide(&i);
        assert_eq!(d.rung, Rung::DropCache);
        assert_eq!(d.reason, Reason::PreemptiveCache);

        // A growth trend alone is enough, without any fault traffic yet.
        let mut i = inputs();
        i.pressure = Pressure::Normal;
        i.trend.compressed_mb_per_s = TREND_ALARM_MB_S;
        assert_eq!(decide(&i).reason, Reason::PreemptiveCache);
    }

    #[test]
    fn cheap_io_is_affordable_on_a_cool_machine() {
        // Churn on a cool chip: drop cache (cheap I/O) rather than wait for it
        // to become expensive heat.
        let mut i = inputs();
        i.pressure = Pressure::Normal;
        i.band = ThermalBand::Cool;
        i.churn = churn(CHURN_NOTABLE_PER_S, 0.0);
        assert_eq!(decide(&i).rung, Rung::DropCache);
    }

    #[test]
    fn a_cool_machine_with_warning_pressure_is_left_alone() {
        // No cache to drop, cool chip, warning-level pressure: no user-visible
        // cost is justified, macOS is already compressing.
        let mut i = inputs();
        i.band = ThermalBand::Cool;
        i.reclaimable_mb = 0.0;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::NoCacheToDrop);
    }

    #[test]
    fn critical_pressure_always_has_an_answer() {
        let mut i = inputs();
        i.pressure = Pressure::Critical;
        i.reclaimable_mb = 0.0;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::QuitIdleApps);
        assert_eq!(d.reason, Reason::MemoryEmergency);
        // It does not need the chip to be hot: memory exhaustion is its own
        // emergency.
        i.band = ThermalBand::Cool;
        assert_eq!(decide(&i).rung, Rung::QuitIdleApps);
    }

    #[test]
    fn a_hot_churning_machine_sheds_load_even_at_warning_pressure() {
        let mut i = inputs();
        i.reclaimable_mb = 0.0;
        i.band = ThermalBand::Hot;
        i.churn = churn(CHURN_NOTABLE_PER_S, 0.0);
        let d = decide(&i);
        assert_eq!(d.rung, Rung::QuitIdleApps);
        assert_eq!(d.reason, Reason::HeatRelief);

        // Hot but quiet: the heat is not coming from memory, so an app quit
        // would cost the user without fixing anything.
        i.churn = Churn::default();
        assert_eq!(decide(&i).rung, Rung::Hold);
    }

    #[test]
    fn a_saturated_chassis_does_not_spend_io_on_advisory_pressure() {
        let mut i = inputs();
        i.band = ThermalBand::Critical;
        i.pressure = Pressure::High;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::HotHold);

        // Critical pressure outranks the thermal brake...
        i.pressure = Pressure::Critical;
        assert_eq!(decide(&i).rung, Rung::DropCache);
        // ... and so does a compressor that is already burning more than the
        // purge would.
        i.pressure = Pressure::High;
        i.churn = churn(CHURN_HEAVY_PER_S, 0.0);
        assert_eq!(decide(&i).rung, Rung::DropCache);
    }

    #[test]
    fn a_thrashing_swap_is_never_made_worse() {
        let mut i = inputs();
        i.swap_used_pct = memory::SWAP_THRASH_PCT;
        // Cool: hold and let the kernel work through it.
        i.band = ThermalBand::Cool;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::SwapThrashing);
        // Critical pressure: an app is the only remaining lever.
        i.pressure = Pressure::Critical;
        assert_eq!(decide(&i).rung, Rung::QuitIdleApps);
    }

    #[test]
    fn a_futile_purge_is_not_repeated_and_escalates_instead() {
        let mut i = inputs();
        i.band = ThermalBand::Hot;
        i.churn = churn(CHURN_NOTABLE_PER_S, 0.0);
        i.last_purge = Some((MIN_PURGE_GAIN_MB - 1.0, 60));
        let d = decide(&i);
        assert_eq!(d.reason, Reason::HeatRelief);
        assert_eq!(d.rung, Rung::QuitIdleApps);

        // Once the window has passed, the cache lever is available again.
        i.last_purge = Some((MIN_PURGE_GAIN_MB - 1.0, FUTILE_WINDOW_S));
        assert_eq!(decide(&i).rung, Rung::DropCache);
        // A purge that did work does not close the lever.
        i.last_purge = Some((900.0, 60));
        i.purge_since_s = memory::COOLDOWN_S;
        assert_eq!(decide(&i).rung, Rung::DropCache);
    }

    #[test]
    fn the_rate_limits_are_reported_and_not_ignored() {
        let mut i = inputs();
        i.purge_since_s = memory::COOLDOWN_S - 1;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::Cooldown);

        i.purge_since_s = u64::MAX;
        i.purges_last_hour = memory::MAX_PER_HOUR;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::HourlyCap);

        // Under critical pressure the rate limit cannot leave the machine
        // without an answer.
        i.pressure = Pressure::Critical;
        assert_eq!(decide(&i).rung, Rung::QuitIdleApps);
    }

    #[test]
    fn a_tiny_cache_is_not_worth_stalling_for() {
        let mut i = inputs();
        i.reclaimable_mb = memory::MIN_RECLAIM_MB - 1.0;
        let d = decide(&i);
        assert_eq!(d.rung, Rung::Hold);
        assert_eq!(d.reason, Reason::NoCacheToDrop);
    }

    #[test]
    fn every_rung_has_a_stable_wire_name() {
        assert_eq!(Rung::Hold.as_str(), "hold");
        assert_eq!(Rung::DropCache.as_str(), "reclaim");
        assert_eq!(Rung::QuitIdleApps.as_str(), "trim");
        for r in [Rung::Hold, Rung::DropCache, Rung::QuitIdleApps] {
            assert!(r
                .as_str()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-'));
        }
    }

    #[test]
    fn every_reason_has_a_slug_and_a_sentence() {
        let reasons = [
            Reason::Disabled,
            Reason::ModeOff,
            Reason::Comfortable,
            Reason::CacheFirst,
            Reason::PreemptiveCache,
            Reason::NoCacheToDrop,
            Reason::SwapThrashing,
            Reason::HotHold,
            Reason::PurgeFutile,
            Reason::Cooldown,
            Reason::HourlyCap,
            Reason::MemoryEmergency,
            Reason::ModePressure,
            Reason::HeatRelief,
        ];
        for r in reasons {
            assert!(!r.as_str().is_empty());
            assert!(r
                .as_str()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-'));
            assert!(r.detail().ends_with('.'), "{:?} must read as a sentence", r);
        }
    }

    #[test]
    fn decisions_echo_their_context_for_the_ui() {
        let mut i = inputs();
        i.band = ThermalBand::Hot;
        i.churn = churn(0.0, 10.0);
        let d = decide(&i);
        assert_eq!(d.band, ThermalBand::Hot);
        assert!((d.heat_load - 20.0).abs() < 1e-9);
        assert_eq!(d.as_str(), d.rung.as_str());
        assert_eq!(d.detail(), d.reason.detail());
    }
}
