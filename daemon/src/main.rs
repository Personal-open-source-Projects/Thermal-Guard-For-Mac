//! thermalguardd — the privileged thermal core of ThermalGuard.
//!
//! Runs as a root LaunchDaemon. Samples the SMC, decides a power/heat policy
//! and applies it. Exposes state to the Swift UI over a unix socket so the UI
//! never needs root itself.

mod agents;
mod apple;
mod audio;
mod json;
mod memory;
mod net;
mod policy;
mod procs;
mod ram;
mod smc;
mod system;
mod trim;

use policy::{FanIntent, Mode};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const LOG_PATH: &str = "/var/log/thermalguard.log";

const DEFAULT_SOCK: &str = "/var/run/thermalguard.sock";
const DEFAULT_RUN_DIR: &str = "/var/run/thermalguard";

/// Path overrides exist only for the test suite. They are ignored when the
/// process is root, so the installed daemon can never be redirected by a
/// planted environment variable to write state or accept commands elsewhere.
pub(crate) fn overrides_allowed() -> bool {
    let is_root = unsafe { geteuid() } == 0;
    if is_root {
        return false;
    }
    std::env::var("THERMALGUARD_TESTING").unwrap_or_default() == "1"
}

fn sock_path() -> String {
    if overrides_allowed() {
        if let Ok(v) = std::env::var("THERMALGUARD_SOCK") {
            return v;
        }
    }
    DEFAULT_SOCK.into()
}

fn run_dir() -> String {
    if overrides_allowed() {
        if let Ok(v) = std::env::var("THERMALGUARD_RUN_DIR") {
            return v;
        }
    }
    DEFAULT_RUN_DIR.into()
}

fn state_path() -> String {
    format!("{}/state.json", run_dir())
}

/// How long Auto-trim waits after being armed before the first disable, so the
/// user can switch it off again before anything changes.
const TRIM_GRACE_S: u64 = 30;
/// A service that refused (SIP) is not retried on every tick.
const TRIM_SIP_BACKOFF_S: u64 = 300;
/// How long to wait before retrying a power-cap change the system refused, so
/// a failed write cannot turn into a `pmset` spawn on every tick.
const CAP_FAIL_BACKOFF_S: u64 = 60;
/// A call or video keeps the media shield up this long after its last busy
/// sample, so one quiet 4 s sample cannot flap the shield (and the cap) off.
const MEDIA_HOLD_S: u64 = 20;

fn trim_grace_s() -> u64 {
    if overrides_allowed() {
        if let Ok(v) = std::env::var("THERMALGUARD_TRIM_GRACE_S") {
            if let Ok(n) = v.parse::<u64>() {
                return n;
            }
        }
    }
    TRIM_GRACE_S
}

struct Shared {
    mode: Mode,
    fan_intent: FanIntent,
    cpu_temp: Option<f64>,
    fan_rpm: Option<f64>,
    fan_min: Option<f64>,
    fan_max: Option<f64>,
    battery_temp: Option<f64>,
    /// Latched battery-heat verdict, so the cap's engage point cannot flicker
    /// while the battery temperature hovers around its threshold.
    battery_hot: bool,
    speed_limit: Option<u32>,
    ac: bool,
    cap_active: bool,
    fan_supported: Option<bool>,
    fan_note: String,
    /// Target we last successfully asked the SMC for, if we are driving it.
    fan_target: Option<f64>,
    /// Safety latch: a silent fan is handed back once released for heat.
    fan_quiet_allowed: bool,
    fan_last_write: Option<Instant>,
    /// When the SMC last refused a fan write, so we stop hammering it.
    fan_failed: Option<Instant>,
    /// A call or video player is currently busy.
    media_active: bool,
    /// User setting: protect call/video apps (never throttle, release the cap).
    media_guard: bool,
    /// User setting: reset a stuck Core Audio engine (automatic, default on).
    audio_guard: bool,
    /// Human-readable watchdog state: watching / spike / restarted / off.
    audio_note: String,
    /// Reset bookkeeping: total, last time, and the rolling-hour window.
    audio_restarts: u32,
    audio_last: Option<Instant>,
    audio_restarts_window: Vec<Instant>,
    /// Consecutive samples of coreaudiod above the spike threshold.
    audio_streak: u32,
    coreaudiod_cpu: Option<f64>,
    /// User setting: reclaim OS cache under pressure (default on).
    memory_reclaim: bool,
    mem_pressure: memory::Pressure,
    /// Percentage of RAM available without reclaim (free + inactive +
    /// speculative). This is the number the UI shows as "Free".
    mem_free_pct: f64,
    mem_total_mb: f64,
    mem_available_mb: f64,
    mem_compressed_mb: f64,
    mem_reclaimable_mb: f64,
    mem_swap_used_mb: f64,
    mem_swap_total_mb: f64,
    mem_swap_used_pct: f64,
    memory_reclaims: u32,
    memory_last: Option<Instant>,
    memory_window: Vec<Instant>,
    memory_note: String,
    memory_trend: memory::Trend,
    /// Thermal band the RAM governor used for its last decision.
    mem_band: ram::ThermalBand,
    /// Fault traffic that became heat between the last two samples.
    mem_churn: ram::Churn,
    /// The full ladder decision, so the UI shows the daemon's verdict rather
    /// than re-deriving pressure from a `vm_stat` fork of its own.
    memory_decision: ram::Decision,
    /// What the last purge actually freed, in MB. `None` until one is measured.
    memory_freed_mb: Option<f64>,
    net_link: String,
    net_interface: String,
    net_rssi: Option<f64>,
    net_tx_mbps: Option<f64>,
    net_gateway_ms: Option<f64>,
    net_loss_pct: f64,
    net_score: u8,
    net_grade: String,
    net_note: String,
    /// User setting: trimmer for auto-safe macOS background services (on by
    /// default; it runs in every mode except Off).
    auto_trim: bool,
    /// Services *we* disabled and therefore may restore. Never includes a
    /// service the user disabled by hand.
    auto_trimmed: Vec<String>,
    auto_trim_note: String,
    /// Don't act before this instant (armed, or daemon just started).
    trim_grace_until: Option<Instant>,
    /// Services that recently refused a launchctl change (SIP), with the time
    /// of the refusal, so they are not retried every tick.
    trim_skip: Vec<(String, Instant)>,
    procs: Vec<procs::Proc>,
    demoted: Vec<(u32, String)>,
    events: Vec<String>,
    smc_ok: bool,
    smc_error: String,
    soft_ratio: Option<u32>,
    lpm_ratio: Option<u32>,
    started: Instant,
    /// A user-visible setting or the tracked set changed: persist now, not on
    /// the next 10 s housekeeping write.
    dirty: bool,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            mode: Mode::Auto,
            // Silent by default: where the SMC accepts a low target the machine
            // runs quietly, and where it does not the governor treats the
            // preference as Auto, so silence never costs speed for nothing.
            fan_intent: FanIntent::Quiet,
            cpu_temp: None,
            fan_rpm: None,
            fan_min: None,
            fan_max: None,
            battery_temp: None,
            battery_hot: false,
            speed_limit: None,
            ac: false,
            cap_active: false,
            fan_supported: None,
            fan_note: String::new(),
            fan_target: None,
            fan_quiet_allowed: true,
            fan_last_write: None,
            fan_failed: None,
            media_active: false,
            media_guard: true,
            audio_guard: true,
            audio_note: String::new(),
            audio_restarts: 0,
            audio_last: None,
            audio_restarts_window: Vec::new(),
            audio_streak: 0,
            coreaudiod_cpu: None,
            memory_reclaim: true,
            mem_pressure: memory::Pressure::Normal,
            mem_free_pct: 0.0,
            mem_total_mb: 0.0,
            mem_available_mb: 0.0,
            mem_compressed_mb: 0.0,
            mem_reclaimable_mb: 0.0,
            mem_swap_used_mb: 0.0,
            mem_swap_total_mb: 0.0,
            mem_swap_used_pct: 0.0,
            memory_reclaims: 0,
            memory_last: None,
            memory_window: Vec::new(),
            memory_note: String::new(),
            memory_trend: memory::Trend::default(),
            mem_band: ram::ThermalBand::Warm,
            mem_churn: ram::Churn::default(),
            memory_decision: ram::Decision::default(),
            memory_freed_mb: None,
            net_link: "—".into(),
            net_interface: String::new(),
            net_rssi: None,
            net_tx_mbps: None,
            net_gateway_ms: None,
            net_loss_pct: 0.0,
            net_score: 0,
            net_grade: "—".into(),
            net_note: String::new(),
            auto_trim: true,
            auto_trimmed: Vec::new(),
            auto_trim_note: String::new(),
            trim_grace_until: None,
            trim_skip: Vec::new(),
            procs: Vec::new(),
            demoted: Vec::new(),
            events: Vec::new(),
            smc_ok: false,
            smc_error: String::new(),
            soft_ratio: None,
            lpm_ratio: None,
            started: Instant::now(),
            dirty: true,
        }
    }
}

/// Hard limits that keep one misbehaving client from hurting the daemon.
const MAX_REQUEST_BYTES: u64 = 8192;
const MAX_CONNECTIONS: usize = 16;

const LOG_MAX_BYTES: u64 = 512 * 1024;
const LOG_KEEP_BYTES: usize = 128 * 1024;

/// Keep the log bounded: a root daemon must not be able to fill the disk over
/// months of use. Old content is trimmed at a line boundary, not deleted
/// wholesale.
fn rotate_log_if_needed() {
    let len = match std::fs::metadata(LOG_PATH) {
        Ok(md) => md.len(),
        Err(_) => return,
    };
    if len <= LOG_MAX_BYTES {
        return;
    }
    let text = match std::fs::read_to_string(LOG_PATH) {
        Ok(t) => t,
        Err(_) => return,
    };
    let keep_from = text.len().saturating_sub(LOG_KEEP_BYTES);
    let tail = text.get(keep_from..).unwrap_or("");
    let tail = match tail.find('\n') {
        Some(i) => &tail[i + 1..],
        None => tail,
    };
    let tmp = format!("{}.rot", LOG_PATH);
    if std::fs::write(&tmp, tail).is_ok() {
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::rename(&tmp, LOG_PATH);
    }
}

fn log_line(msg: &str) {
    // Refuse to follow a symlink planted in /var/log and keep the log 0600:
    // it names the processes we throttled.
    if let Ok(md) = std::fs::symlink_metadata(LOG_PATH) {
        if md.file_type().is_symlink() {
            return;
        }
    }
    rotate_log_if_needed();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
    {
        let _ = writeln!(f, "[{}] {}", system::now_hms(), msg);
    }
    let _ = std::fs::set_permissions(LOG_PATH, std::fs::Permissions::from_mode(0o600));
}

fn push_event(sh: &mut Shared, msg: &str) {
    let stamp = format!("{} {}", system::now_hms(), msg);
    log_line(msg);
    sh.events.push(stamp);
    if sh.events.len() > 40 {
        sh.events.remove(0);
    }
}

fn opt_num(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{:.1}", x),
        None => "null".to_string(),
    }
}

/// Atomic + symlink-safe state publication: write a temp file in the same dir,
/// then rename over the target (rename replaces a symlink rather than
/// following it).
fn write_state(sh: &Shared) {
    let path = state_path();
    let tmp = format!("{}.tmp", path);
    if std::fs::write(&tmp, state_json(sh)).is_err() {
        return;
    }
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
    let _ = std::fs::rename(&tmp, &path);
}

/// Audio watchdog state derived from the *current* settings, so a mode change
/// is visible in the very next status reply instead of on the next tick.
fn audio_state_for(sh: &Shared) -> &'static str {
    if !sh.audio_guard {
        "off"
    } else if sh.mode == Mode::Off {
        "off (mode)"
    } else if sh.audio_streak >= audio::SPIKE_SAMPLES {
        "spike"
    } else if sh
        .audio_last
        .map(|t| t.elapsed().as_secs() < 10)
        .unwrap_or(false)
    {
        "restarted"
    } else {
        "watching"
    }
}

fn state_json(sh: &Shared) -> String {
    let procs_json: Vec<String> = sh
        .procs
        .iter()
        .map(|p| {
            json::obj(&[
                json::i("pid", p.pid as i64),
                json::n("cpu", (p.cpu * 10.0).round() / 10.0),
                json::s("name", &procs::short_name(&p.name)),
            ])
        })
        .collect();
    let demoted_json: Vec<String> = sh
        .demoted
        .iter()
        .map(|(pid, name)| {
            json::obj(&[
                json::i("pid", *pid as i64),
                json::s("name", &procs::short_name(name)),
            ])
        })
        .collect();
    let events_json: Vec<String> = sh
        .events
        .iter()
        .map(|e| format!("\"{}\"", json::esc(e)))
        .collect();

    json::obj(&[
        // The helper's own version. The app compares this with its bundle
        // version: a DMG install does not replace a privileged helper that is
        // already loaded, so without this the UI would talk to an old daemon
        // forever and silently lose any field the new one adds.
        json::s("daemon_version", env!("CARGO_PKG_VERSION")),
        json::s("mode", sh.mode.as_str()),
        json::s("mode_label", sh.mode.label()),
        json::s("fan_intent", sh.fan_intent.as_str()),
        format!("\"fan_target\":{}", opt_num(sh.fan_target)),
        format!("\"cpu_temp\":{}", opt_num(sh.cpu_temp)),
        format!("\"fan_rpm\":{}", opt_num(sh.fan_rpm)),
        format!("\"fan_min\":{}", opt_num(sh.fan_min)),
        format!("\"fan_max\":{}", opt_num(sh.fan_max)),
        format!("\"battery_temp\":{}", opt_num(sh.battery_temp)),
        json::b("battery_hot", sh.battery_hot),
        match sh.speed_limit {
            Some(v) => json::i("speed_limit", v as i64),
            None => "\"speed_limit\":null".to_string(),
        },
        json::b("ac_power", sh.ac),
        json::b("media_active", sh.media_active),
        json::b("media_guard", sh.media_guard),
        json::b("audio_guard", sh.audio_guard),
        json::s("audio_state", audio_state_for(sh)),
        json::s("audio_note", &sh.audio_note),
        json::i("audio_restarts", sh.audio_restarts as i64),
        format!("\"coreaudiod_cpu\":{}", opt_num(sh.coreaudiod_cpu)),
        match sh.audio_last {
            Some(t) => json::i("audio_last_s", t.elapsed().as_secs() as i64),
            None => "\"audio_last_s\":null".to_string(),
        },
        json::b("memory_reclaim", sh.memory_reclaim),
        json::s("memory_pressure", sh.mem_pressure.as_str()),
        json::s("memory_band", sh.mem_band.as_str()),
        json::n("memory_free_pct", (sh.mem_free_pct * 10.0).round() / 10.0),
        json::n("memory_total_mb", sh.mem_total_mb.round()),
        json::n("memory_available_mb", sh.mem_available_mb.round()),
        json::n("memory_compressed_mb", sh.mem_compressed_mb.round()),
        json::n("memory_reclaimable_mb", sh.mem_reclaimable_mb.round()),
        json::n("memory_swap_used_mb", sh.mem_swap_used_mb.round()),
        json::n("memory_swap_total_mb", sh.mem_swap_total_mb.round()),
        json::n(
            "memory_swap_used_pct",
            (sh.mem_swap_used_pct * 10.0).round() / 10.0,
        ),
        json::i("memory_reclaims", sh.memory_reclaims as i64),
        // Backwards-compatible alias for `memory_plan`.
        json::s("memory_advice", sh.memory_decision.as_str()),
        json::n(
            "memory_trend_mb_s",
            (sh.memory_trend.compressed_mb_per_s * 10.0).round() / 10.0,
        ),
        // The ladder the daemon settled on, plus the reason, so the UI never
        // has to second-guess it with a coarser `vm_stat` sample.
        json::s("memory_plan", sh.memory_decision.as_str()),
        json::s("memory_plan_reason", sh.memory_decision.reason.as_str()),
        json::s("memory_plan_detail", sh.memory_decision.detail()),
        json::n(
            "memory_heat_load",
            (sh.mem_churn.heat_load() * 10.0).round() / 10.0,
        ),
        json::n(
            "memory_churn_decomp_s",
            (sh.mem_churn.decompressions_per_s * 10.0).round() / 10.0,
        ),
        json::n(
            "memory_churn_swapin_s",
            (sh.mem_churn.swapins_per_s * 10.0).round() / 10.0,
        ),
        json::s("memory_note", &sh.memory_note),
        json::s("net_link", &sh.net_link),
        json::s("net_interface", &sh.net_interface),
        format!("\"net_rssi\":{}", opt_num(sh.net_rssi)),
        format!("\"net_tx_mbps\":{}", opt_num(sh.net_tx_mbps)),
        format!("\"net_gateway_ms\":{}", opt_num(sh.net_gateway_ms)),
        json::n("net_loss_pct", (sh.net_loss_pct * 10.0).round() / 10.0),
        json::i("net_score", sh.net_score as i64),
        json::s("net_grade", &sh.net_grade),
        json::s("net_note", &sh.net_note),
        json::b("auto_trim", sh.auto_trim),
        json::b("auto_trim_active", sh.auto_trim && sh.mode != Mode::Off),
        json::s("auto_trim_note", &sh.auto_trim_note),
        json::str_array("auto_trimmed", &sh.auto_trimmed),
        json::b("cap_active", sh.cap_active),
        json::b("smc_ok", sh.smc_ok),
        json::s("smc_error", &sh.smc_error),
        match sh.fan_supported {
            Some(true) => json::b("fan_supported", true),
            Some(false) => json::b("fan_supported", false),
            None => "\"fan_supported\":null".to_string(),
        },
        json::s("fan_note", &sh.fan_note),
        match sh.soft_ratio {
            Some(v) => json::i("turbo_mhz", v as i64 * 100),
            None => "\"turbo_mhz\":null".to_string(),
        },
        match sh.lpm_ratio {
            Some(v) => json::i("lpm_mhz", v as i64 * 100),
            None => "\"lpm_mhz\":null".to_string(),
        },
        json::i("uptime_s", sh.started.elapsed().as_secs() as i64),
        format!("\"procs\":[{}]", procs_json.join(",")),
        format!("\"demoted\":[{}]", demoted_json.join(",")),
        format!("\"events\":[{}]", events_json.join(",")),
    ])
}

fn read_opt(smc: &Option<smc::Smc>, key: &str) -> Option<f64> {
    smc.as_ref().and_then(|s| s.read_f64(key))
}

fn read_first(smc: &Option<smc::Smc>, keys: &[&str]) -> Option<f64> {
    for k in keys {
        if let Some(v) = read_opt(smc, k) {
            if v > -20.0 && v < 130.0 {
                return Some(v);
            }
        }
    }
    None
}

/// Write a fan target and verify it by read-back. Returns None when the SMC
/// offers no readable target (nothing can be claimed either way).
fn probe_fan_support(smc: &Option<smc::Smc>, sh: &mut Shared, target: f64) -> Option<bool> {
    let s = match smc {
        Some(s) => s,
        None => return None,
    };
    let before = s.read_f64("F0Tg");
    let wrote = s
        .write_raw("F0Tg", "fpe2", &smc::encode_fpe2(target))
        .is_ok()
        || s.write_raw("F0Tg", "flt ", &smc::encode_flt(target))
            .is_ok();
    if !wrote {
        sh.fan_note = "SMC rejected the fan target write.".into();
        return Some(false);
    }
    thread::sleep(Duration::from_millis(600));
    let after = s.read_f64("F0Tg");
    let tol = (target * 0.08).max(250.0);
    match (before, after) {
        (Some(b), Some(a)) if (a - target).abs() <= tol => {
            if (b - target).abs() <= tol {
                sh.fan_note = format!(
                    "Fan was already at {:.0} RPM (target {:.0}); the SMC accepted the write but nothing needed to change.",
                    b, target
                );
            } else {
                sh.fan_note = format!("Fan target accepted by SMC ({:.0} -> {:.0} RPM).", b, a);
            }
            Some(true)
        }
        (_, Some(a)) => {
            sh.fan_note = format!(
                "SMC ignored the fan target (asked {:.0} RPM, still {:.0} RPM): fan control is not available on this model.",
                target, a
            );
            Some(false)
        }
        _ => {
            sh.fan_note = "No readable fan target key; fan control unavailable.".into();
            None
        }
    }
}

/// What to do with the fan, as a closed set rather than a nested `Option`.
#[derive(Clone, Copy)]
enum FanCommand {
    /// Give the fan back to the SMC's own curve.
    Release,
    /// Hold the fan at this many RPM (always derived from the SMC's range).
    Target(f64),
}

/// Re-assert a held fan target this often: Apple's thermal manager may retake
/// the fan, and the user's choice should stay in force until changed.
const FAN_REASSERT_S: u64 = 10;
/// Ignore sub-audible differences so we do not write the SMC every tick.
const FAN_CHANGE_RPM: f64 = 200.0;
/// How long to wait before trying the SMC again after it refused a write.
const FAN_FAIL_BACKOFF_S: u64 = 120;

/// Hand the fan back to the SMC (the only releasing write we ever make).
fn drive_fan_release(s: &smc::Smc, sh: &mut Shared) -> Option<FanCommand> {
    let released = s.write_raw("F0Md", "ui8", &[0]).is_ok();
    sh.fan_last_write = Some(Instant::now());
    sh.fan_target = None;
    sh.fan_note = if released {
        "Fan handed back to the SMC while a call or video is playing.".into()
    } else {
        "Fan firmware is in charge (nothing for us to hand back).".into()
    };
    None
}

/// Drive the fan towards the preference the user chose.
///
/// Every value written here is derived from the SMC's own reported ranges —
/// nothing from the IPC wire reaches the write — and each write is verified by
/// read-back before it is reported as supported.
fn drive_fan(smc: &Option<smc::Smc>, sh: &mut Shared, temp_c: Option<f64>) {
    let s = match smc {
        Some(s) => s,
        None => {
            sh.fan_supported = None;
            return;
        }
    };

    // What the preference asks for, stated in the type rather than in a nested
    // Option: `None` means the SMC could not even tell us its range.
    let want: Option<FanCommand> = match sh.fan_intent {
        FanIntent::Auto => Some(FanCommand::Release),
        FanIntent::Max => policy::max_fan_target(sh.fan_min, sh.fan_max).map(FanCommand::Target),
        FanIntent::Quiet => {
            // A call or a video file is playing: cooling wins over silence, so
            // the SMC gets the fan back for the duration.
            if sh.media_active {
                sh.fan_quiet_allowed = true;
                drive_fan_release(s, sh);
                None
            } else {
                let allowed = policy::quiet_fan_allowed(temp_c, sh.fan_quiet_allowed);
                if allowed != sh.fan_quiet_allowed {
                    sh.fan_quiet_allowed = allowed;
                    if allowed {
                        push_event(sh, "silent fan re-applied after cooling down");
                    } else {
                        push_event(sh, "silent fan released: chip too hot, SMC curve restored");
                    }
                }
                if allowed {
                    policy::quiet_fan_target(sh.fan_min, sh.fan_max).map(FanCommand::Target)
                } else {
                    Some(FanCommand::Release)
                }
            }
        }
    };

    let want = match want {
        Some(w) => w,
        None => return,
    };

    // A model that refuses fan writes will refuse them forever. Retrying every
    // tick would hammer the SMC and spam the log, so back off for a while.
    if let Some(t) = sh.fan_failed {
        if t.elapsed().as_secs() < FAN_FAIL_BACKOFF_S {
            return;
        }
    }

    // Rate limit, and skip writes that would not change anything audible.
    let due = sh
        .fan_last_write
        .map(|t| t.elapsed().as_secs() >= FAN_REASSERT_S)
        .unwrap_or(true);
    match (want, sh.fan_target) {
        (FanCommand::Release, None) => return,
        (FanCommand::Target(t), Some(cur)) if !due && (t - cur).abs() < FAN_CHANGE_RPM => return,
        _ => {}
    }

    match want {
        FanCommand::Target(target) => {
            let wrote = s
                .write_raw("F0Tg", "flt ", &smc::encode_flt(target))
                .is_ok()
                || s.write_raw("F0Tg", "fpe2", &smc::encode_fpe2(target))
                    .is_ok();
            sh.fan_last_write = Some(Instant::now());
            if !wrote {
                sh.fan_failed = Some(Instant::now());
                sh.fan_supported = Some(false);
                sh.fan_note =
                    "SMC rejected the fan target write: fan control is not available on this model."
                        .into();
                sh.fan_target = None;
                return;
            }
            thread::sleep(Duration::from_millis(400));
            let applied = s.read_f64("F0Tg");
            let tol = (target * 0.08).max(250.0);
            match applied {
                Some(a) if (a - target).abs() <= tol => {
                    sh.fan_supported = Some(true);
                    // Only once the target is proven accepted do we take the
                    // fan out of Apple's curve: a model that ignores F0Tg must
                    // never be left with its own cooling ramp disabled.
                    let _ = s.write_raw("F0Md", "ui8", &[1]);
                    let actual = s.read_f64("F0Ac");
                    sh.fan_target = Some(target);
                    sh.fan_note = match actual {
                        Some(act) => format!(
                            "Fan target {:.0} RPM accepted by the SMC (now running {:.0} RPM).",
                            target, act
                        ),
                        None => format!("Fan target {:.0} RPM accepted by the SMC.", target),
                    };
                }
                other => {
                    // Hand the fan straight back: never leave it in manual mode
                    // after a refused target.
                    let _ = s.write_raw("F0Md", "ui8", &[0]);
                    sh.fan_failed = Some(Instant::now());
                    sh.fan_supported = Some(false);
                    sh.fan_target = None;
                    sh.fan_note = format!(
                        "SMC ignored the fan target (asked {:.0} RPM, reads {:?}). This model's firmware owns the fan, so silence is achieved by cutting heat instead: the power cap is applied earlier while Silent is selected.",
                        target, other
                    );
                }
            }
        }
        FanCommand::Release => {
            // Hand the fan back by asking the SMC for its own automatic mode
            // (F0Md = 0). Deliberately *no* fallback that writes an RPM: the
            // earlier version fell back to the fan maximum, which on a model
            // that ignores F0Tg would pin the fan loud for no benefit.
            let released = s.write_raw("F0Md", "ui8", &[0]).is_ok();
            sh.fan_last_write = Some(Instant::now());
            sh.fan_target = None;
            sh.fan_note = if released {
                "Fan handed back to the SMC thermal curve.".into()
            } else {
                "Fan firmware is in charge (nothing for us to hand back).".into()
            };
        }
    }
}

static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

struct ConnGuard;

impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
    }
}

extern "C" {
    fn getpeereid(fd: std::os::raw::c_int, euid: *mut u32, egid: *mut u32) -> std::os::raw::c_int;
    fn umask(mask: u32) -> u32;
    fn geteuid() -> u32;
}

/// Root and local interactive accounts may drive this machine-wide daemon.
/// Socket permissions let launchd start before login; peer credentials reject
/// system service identities while allowing every local user account.
fn peer_allowed(stream: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut uid: u32 = u32::MAX;
    let mut gid: u32 = u32::MAX;
    let rc = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return false;
    }
    peer_uid_allowed(uid)
}

fn peer_uid_allowed(uid: u32) -> bool {
    uid == 0 || uid >= 500
}

#[cfg(test)]
mod peer_auth_tests {
    use super::peer_uid_allowed;

    #[test]
    fn root_and_all_local_user_accounts_are_authorized() {
        assert!(peer_uid_allowed(0));
        assert!(peer_uid_allowed(501));
        assert!(peer_uid_allowed(502));
        assert!(!peer_uid_allowed(499));
    }
}

/// Labels come from the wire, so keep them to a printable safe alphabet before
/// they are logged or compared.
fn safe_label(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ' '))
        .take(120)
        .collect()
}

/// Reconcile the background-service trimmer with launchd.
///
/// Called once per daemon tick, so at most one service changes per tick. The
/// decision itself is pure (`trim::plan`); this function only performs the one
/// verified side effect it returns and records what we did. It is active in
/// every cooling mode except Off and only ever touches the compiled-in
/// background-only catalog.
fn reconcile_auto_trim(sh: &mut Shared) {
    let active = sh.auto_trim && sh.mode != Mode::Off;

    // Nothing to restore and nothing to trim: stay completely idle.
    if !active && sh.auto_trimmed.is_empty() {
        sh.trim_skip.clear();
        return;
    }

    // A tampered state file cannot widen the set: only auto-safe ids survive.
    sh.auto_trimmed.retain(|id| apple::is_auto_safe(id));
    sh.auto_trimmed.sort();
    sh.auto_trimmed.dedup();

    let now = Instant::now();
    sh.trim_skip
        .retain(|(_, at)| now.duration_since(*at).as_secs() < TRIM_SIP_BACKOFF_S);

    let states = match apple::service_states() {
        Some(s) => s,
        None => {
            if active {
                sh.auto_trim_note = "launchd state unreadable; not trimming.".into();
            }
            return;
        }
    };

    let grace_elapsed = !active || sh.trim_grace_until.map(|t| now >= t).unwrap_or(true);
    let blocked: Vec<String> = sh.trim_skip.iter().map(|(id, _)| id.clone()).collect();
    let auto_safe = apple::auto_safe_ids();
    let action = trim::plan(
        active,
        Some(&states),
        &sh.auto_trimmed,
        &auto_safe,
        grace_elapsed,
        &blocked,
    );

    match action {
        trim::Action::None => {
            if active && !grace_elapsed {
                let left = sh
                    .trim_grace_until
                    .map(|t| t.saturating_duration_since(now).as_secs())
                    .unwrap_or(0);
                sh.auto_trim_note = format!("Auto-trim armed; first action in {} s.", left.max(1));
            } else if active {
                sh.auto_trim_note =
                    format!("{} background service(s) trimmed.", sh.auto_trimmed.len());
            } else if sh.auto_trimmed.is_empty() {
                sh.auto_trim_note = "Nothing trimmed.".into();
            }
        }
        trim::Action::Disable(id) => match apple::set(&id, false) {
            Ok(_) => {
                if !sh.auto_trimmed.iter().any(|x| x == &id) {
                    sh.auto_trimmed.push(id.clone());
                }
                sh.auto_trimmed.sort();
                sh.auto_trimmed.dedup();
                sh.dirty = true;
                // Disabling stops future launches; also stop the copy that is
                // running now, gracefully, so the saving starts immediately.
                let stopped = apple::stop_running(&id);
                sh.auto_trim_note = if stopped {
                    format!("Trimmed {} and stopped it.", id)
                } else {
                    format!("Trimmed {}.", id)
                };
                push_event(
                    sh,
                    &format!(
                        "auto-trim: disabled {}{}",
                        id,
                        if stopped { " (stopped)" } else { "" }
                    ),
                );
            }
            Err(e) => {
                sh.trim_skip.push((id.clone(), now));
                sh.auto_trim_note = format!("Could not trim {}: {}", id, e);
                push_event(sh, &format!("auto-trim: {} refused: {}", id, e));
            }
        },
        trim::Action::Enable(id) => match apple::set(&id, true) {
            Ok(_) => {
                sh.auto_trimmed.retain(|x| x != &id);
                sh.dirty = true;
                sh.auto_trim_note = format!("Restored {}.", id);
                push_event(sh, &format!("auto-trim: restored {}", id));
            }
            Err(e) => {
                sh.trim_skip.push((id.clone(), now));
                sh.auto_trim_note = format!("Could not restore {}: {}", id, e);
                push_event(sh, &format!("auto-trim: restore {} failed: {}", id, e));
            }
        },
        trim::Action::Forget(id) => {
            sh.auto_trimmed.retain(|x| x != &id);
            sh.dirty = true;
            sh.auto_trim_note = format!("{} is enabled again; no longer tracked.", id);
        }
    }
}

fn handle_client(mut stream: UnixStream, shared: Arc<Mutex<Shared>>) {
    if !peer_allowed(&stream) {
        log_line("rejected IPC connection from an unauthorised uid");
        let _ = stream.write_all(b"{\"error\":\"unauthorised\"}\n");
        let _ = stream.flush();
        return;
    }

    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // Bounded, newline-terminated read: a client cannot grow our heap with a
    // huge line, and one that never sends a newline cannot hold us forever
    // (read timeout above applies too).
    let mut req = String::new();
    if let Ok(p) = stream.try_clone() {
        let mut r = BufReader::new(p);
        let mut buf = [0u8; 1024];
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let chunk = &buf[..n];
            match chunk.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    req.push_str(&String::from_utf8_lossy(&chunk[..pos]));
                    break;
                }
                None => {
                    req.push_str(&String::from_utf8_lossy(chunk));
                    if req.len() as u64 >= MAX_REQUEST_BYTES {
                        break;
                    }
                }
            }
        }
    }
    if req.len() as u64 > MAX_REQUEST_BYTES {
        let mut cut = MAX_REQUEST_BYTES as usize;
        while cut > 0 && !req.is_char_boundary(cut) {
            cut -= 1;
        }
        req.truncate(cut);
    }
    let req = req.trim().to_string();
    let cmd = json::get_str(&req, "cmd").unwrap_or_else(|| "status".into());

    // macOS' own background services, toggled by the user.
    if cmd == "apple" || cmd == "apple_set" {
        let mut note = String::new();
        if cmd == "apple_set" {
            let id = safe_label(&json::get_str(&req, "id").unwrap_or_default());
            let enable = json::get_bool(&req, "enable").unwrap_or(false);
            match apple::set(&id, enable) {
                Ok(msg) => {
                    log_line(&msg);
                    note = msg;
                }
                Err(e) => {
                    log_line(&format!("apple toggle {} failed: {}", id, e));
                    note = e;
                }
            }
        }
        let body = {
            let sh = shared.lock().unwrap();
            let active = sh.auto_trim && sh.mode != Mode::Off;
            apple::items_json(
                &apple::scan(),
                &note,
                sh.auto_trim,
                active,
                &sh.auto_trimmed,
                &sh.auto_trim_note,
            )
        };
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
        return;
    }

    // Manual control of third-party background items. Only ever runs because a
    // person unticked a box in the UI.
    if cmd == "agents" || cmd == "agent" {
        let mut note = String::new();
        if cmd == "agent" {
            let label = safe_label(&json::get_str(&req, "label").unwrap_or_default());
            let enable = json::get_bool(&req, "enable").unwrap_or(false);
            // No path or scope from the caller: the daemon resolves them itself
            // by label, so it cannot be steered at an arbitrary plist.
            match agents::set_enabled(&label, enable) {
                Ok(_) => {
                    let msg = format!("{} {}", if enable { "enabled" } else { "disabled" }, label);
                    log_line(&msg);
                    note = msg;
                }
                Err(e) => {
                    log_line(&format!("toggle {} failed: {}", label, e));
                    note = format!("failed: {}", e);
                }
            }
        }
        let items = agents::scan();
        let mut body = agents::items_json(&items);
        if !note.is_empty() {
            body = format!(
                "{},\"note\":\"{}\"}}",
                body.trim_end_matches('}'),
                json::esc(&note)
            );
        }
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
        return;
    } // Test-only hook: exercises the OS-level reclaim path with a stub
      // `purge`, never the real one. Not reachable in a root install.
    if cmd == "memory_reclaim_now" {
        let body = if overrides_allowed() {
            match system::purge_reclaimable() {
                Ok(_) => {
                    let mut sh = shared.lock().unwrap();
                    sh.memory_reclaims += 1;
                    let now = Instant::now();
                    sh.memory_last = Some(now);
                    sh.memory_freed_mb = None;
                    sh.memory_window.push(now);
                    sh.memory_note = "Reclaimable cache was released (test hook).".into();
                    push_event(&mut sh, "memory reclaim (test hook)");
                    state_json(&sh)
                }
                Err(e) => json::obj(&[json::s("error", &e)]),
            }
        } else {
            json::obj(&[json::s("error", "unsupported")])
        };
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
        return;
    }

    // Test-only hook: drives the very same verified reset path with a stub
    // `killall`, so the end-to-end test never touches the real audio stack.
    // `overrides_allowed()` is false for the installed root daemon, so this
    // command cannot exist in production.
    if cmd == "audio_restart" {
        let body = if overrides_allowed() {
            match system::restart_coreaudio() {
                Ok(note) => {
                    log_line(&note);
                    let mut sh = shared.lock().unwrap();
                    sh.audio_restarts += 1;
                    sh.audio_last = Some(Instant::now());
                    sh.audio_restarts_window.push(Instant::now());
                    sh.audio_streak = 0;
                    sh.audio_note = note;
                    push_event(&mut sh, "core audio reset (test hook)");
                    state_json(&sh)
                }
                Err(e) => json::obj(&[json::s("error", &e)]),
            }
        } else {
            json::obj(&[json::s("error", "unsupported")])
        };
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
        return;
    }

    let mut apply_mode: Option<Mode> = None;
    let mut apply_fan: Option<FanIntent> = None;

    match cmd.as_str() {
        "auto_trim" => {
            // Strict boolean only: a string such as "yes" must not arm it.
            if let Some(on) = json::get_bool(&req, "value") {
                let mut sh = shared.lock().unwrap();
                if sh.auto_trim != on {
                    sh.auto_trim = on;
                    sh.dirty = true;
                    if on && sh.mode != Mode::Off {
                        let grace = trim_grace_s();
                        sh.trim_grace_until = Some(Instant::now() + Duration::from_secs(grace));
                        sh.auto_trim_note =
                            format!("Auto-trim armed; first action in {} s.", grace);
                        push_event(&mut sh, "auto-trim armed");
                    } else {
                        sh.trim_grace_until = None;
                        sh.auto_trim_note = if on {
                            "Auto-trim on; it starts when a cooling mode is active.".into()
                        } else {
                            "Auto-trim off; restoring any trimmed services.".into()
                        };
                        push_event(
                            &mut sh,
                            if on {
                                "auto-trim on (waits for a cooling mode)"
                            } else {
                                "auto-trim off"
                            },
                        );
                    }
                }
            }
        }
        "mode" => {
            if let Some(m) = json::get_str(&req, "value").and_then(|v| Mode::parse(&v)) {
                apply_mode = Some(m);
            }
        }
        "fan" => match json::get_str(&req, "value").as_deref() {
            Some(v) => apply_fan = FanIntent::parse(v),
            _ => {}
        },
        "media_guard" => {
            // Strictly a boolean: anything else is ignored.
            if let Some(v) = json::get_str(&req, "value") {
                let on = match v.as_str() {
                    "on" | "true" | "1" => Some(true),
                    "off" | "false" | "0" => Some(false),
                    _ => None,
                };
                if let Some(on) = on {
                    let mut sh = shared.lock().unwrap();
                    if sh.media_guard != on {
                        sh.media_guard = on;
                        sh.dirty = true;
                        push_event(
                            &mut sh,
                            if on {
                                "video & call apps shielded"
                            } else {
                                "video & call shield off"
                            },
                        );
                    }
                }
            }
        }
        "audio_guard" => {
            // Strict boolean only: a string must not arm or disarm the watchdog.
            if let Some(on) = json::get_bool(&req, "value") {
                let mut sh = shared.lock().unwrap();
                if sh.audio_guard != on {
                    sh.audio_guard = on;
                    sh.audio_streak = 0;
                    sh.dirty = true;
                    if !on {
                        sh.audio_note = "Core Audio watchdog off; nothing is signalled.".into();
                    }
                    push_event(
                        &mut sh,
                        if on {
                            "core audio watchdog on"
                        } else {
                            "core audio watchdog off"
                        },
                    );
                }
            }
        }
        "memory_reclaim" => {
            if let Some(on) = json::get_bool(&req, "value") {
                let mut sh = shared.lock().unwrap();
                if sh.memory_reclaim != on {
                    sh.memory_reclaim = on;
                    sh.dirty = true;
                    sh.memory_note = if on {
                        "Cache reclaim on; it runs only under real pressure.".into()
                    } else {
                        "Cache reclaim off; nothing is purged.".into()
                    };
                    push_event(
                        &mut sh,
                        if on {
                            "memory cache reclaim on"
                        } else {
                            "memory cache reclaim off"
                        },
                    );
                }
            }
        }
        _ => {}
    }

    {
        let mut sh = shared.lock().unwrap();
        if let Some(m) = apply_mode {
            let was_off = sh.mode == Mode::Off;
            if sh.mode != m {
                push_event(&mut sh, &format!("mode -> {}", m.label()));
            }
            sh.mode = m;
            sh.dirty = true;
            // Any cooling mode keeps the trimmer working; Off hands every
            // tracked service back. Leaving Off starts the grace clock, so a
            // mode change never disables a service the same instant.
            if m != Mode::Off && was_off && sh.auto_trim {
                let grace = trim_grace_s();
                sh.trim_grace_until = Some(Instant::now() + Duration::from_secs(grace));
            } else if m == Mode::Off {
                sh.trim_grace_until = None;
            }
            if m == Mode::Off {
                // Release everything we ever applied.
                if sh.cap_active {
                    let _ = system::set_low_power_mode(false);
                    sh.cap_active = false;
                }
                for (pid, _) in sh.demoted.clone() {
                    procs::restore(pid);
                }
                sh.demoted.clear();
            }
        }
        if let Some(f) = apply_fan {
            sh.fan_intent = f;
            sh.dirty = true;
            // A fresh choice gets a fresh attempt: an earlier refused write
            // must not delay the SMC being asked again (or the fan being
            // handed back when the user returns to the SMC curve).
            sh.fan_failed = None;
            match f {
                FanIntent::Max => {
                    // Never ask for a value outside the SMC's own reported range.
                    match policy::max_fan_target(sh.fan_min, sh.fan_max) {
                        Some(target) => {
                            let res = probe_fan_support(&smc_holder(), &mut sh, target);
                            sh.fan_supported = res;
                            push_event(
                                &mut sh,
                                match res {
                                    Some(true) => "fan target -> max (verified by read-back)",
                                    Some(false) => "fan control unsupported on this model",
                                    None => "fan target unreadable",
                                },
                            );
                        }
                        None => {
                            // No trustworthy range: never write a guessed RPM.
                            sh.fan_supported = None;
                            sh.fan_note =
                                "SMC fan range unreadable; the fan is left to Apple's curve."
                                    .into();
                            push_event(&mut sh, "fan target skipped: SMC range unreadable");
                        }
                    }
                }
                FanIntent::Quiet => {
                    sh.fan_quiet_allowed = true;
                    sh.fan_note = "Silent requested: quieter fan if the SMC allows it, earlier power cap either way.".into();
                }
                FanIntent::Auto => {
                    sh.fan_note = "Fan left to the SMC curve.".into();
                }
            }
        }
    }

    {
        let mut sh = shared.lock().unwrap();
        if sh.dirty {
            write_state(&sh);
            sh.dirty = false;
        }
    }

    let body = {
        let sh = shared.lock().unwrap();
        state_json(&sh)
    };
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}

/// Opened per use: cheap, and avoids sharing one handle across threads.
fn smc_holder() -> Option<smc::Smc> {
    smc::Smc::open().ok()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("thermalguardd {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--memory-probe") {
        let n = 200u32;
        let started = Instant::now();
        let mut last = None;
        for _ in 0..n {
            last = system::memory_stats();
        }
        let avg_us = started.elapsed().as_secs_f64() * 1e6 / n as f64;
        match last {
            Some(m) => println!(
                "{{\"total_mb\":{:.0},\"free_mb\":{:.0},\"available_mb\":{:.0},\"free_pct\":{:.1},\"compressed_mb\":{:.0},\"reclaimable_mb\":{:.0},\"swap_used_mb\":{:.0},\"swap_total_mb\":{:.0},\"swap_used_pct\":{:.1},\"kernel_pressure\":{},\"compressions\":{},\"decompressions\":{},\"swapins\":{},\"swapouts\":{},\"avg_us\":{:.1}}}",
                m.total_mb,
                m.free_mb,
                m.available_mb,
                if m.total_mb > 0.0 {
                    m.available_mb / m.total_mb * 100.0
                } else {
                    0.0
                },
                m.compressed_mb,
                m.reclaimable_mb,
                m.swap_used_mb,
                m.swap_total_mb,
                m.swap_used_pct,
                m.kernel_pressure.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                m.compressions,
                m.decompressions,
                m.swapins,
                m.swapouts,
                avg_us
            ),
            None => println!("{{\"error\":\"memory stats unreadable\"}}"),
        }
        return;
    }
    // One real decision per mode, from a live two-sample churn window. Read
    // only: it prints the ladder, it never purges.
    if args.iter().any(|a| a == "--ram-plan") {
        let before = system::memory_stats();
        let t0 = Instant::now();
        thread::sleep(Duration::from_secs(1));
        let after = system::memory_stats();
        let (Some(b), Some(a)) = (before, after) else {
            println!("{{\"error\":\"memory stats unreadable\"}}");
            return;
        };
        let churn = ram::Churn::between(&b.counters(), &a.counters(), t0.elapsed().as_secs_f64());
        let temp = read_first(&smc_holder(), smc::CPU_TEMP_KEYS);
        let limit = system::cpu_speed_limit();
        let band = ram::thermal_band(
            temp,
            limit.map(|v| v < policy::THROTTLE_LIMIT).unwrap_or(false),
        );
        let free_pct = if a.total_mb > 0.0 {
            a.available_mb / a.total_mb * 100.0
        } else {
            0.0
        };
        let compressed_pct = if a.total_mb > 0.0 {
            a.compressed_mb / a.total_mb * 100.0
        } else {
            0.0
        };
        let pressure =
            memory::pressure_level(a.kernel_pressure, free_pct, a.swap_used_pct, compressed_pct);
        let plans: Vec<String> = [
            Mode::Auto,
            Mode::Balanced,
            Mode::CoolQuiet,
            Mode::MaxCool,
            Mode::Off,
        ]
        .iter()
        .map(|mode| {
            let d = ram::decide(&ram::Inputs {
                enabled: true,
                mode: *mode,
                pressure,
                band,
                churn,
                trend: memory::Trend::default(),
                reclaimable_mb: a.reclaimable_mb,
                swap_used_pct: a.swap_used_pct,
                purge_since_s: u64::MAX,
                purges_last_hour: 0,
                last_purge: None,
            });
            format!(
                "{{\"mode\":\"{}\",\"plan\":\"{}\",\"reason\":\"{}\",\"detail\":\"{}\"}}",
                mode.as_str(),
                d.as_str(),
                d.reason.as_str(),
                json::esc(d.detail())
            )
        })
        .collect();
        println!(
            "{{\"temp\":{},\"band\":\"{}\",\"pressure\":\"{}\",\"reclaimable_mb\":{:.0},\"swap_used_pct\":{:.1},\"heat_load\":{:.1},\"plans\":[{}]}}",
            temp.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "null".into()),
            band.as_str(),
            pressure.as_str(),
            a.reclaimable_mb,
            a.swap_used_pct,
            churn.heat_load(),
            plans.join(",")
        );
        return;
    }
    // Record churn vs. die temperature for N seconds so the two thresholds
    // (CHURN_NOTABLE_PER_S / CHURN_HEAVY_PER_S) can be tuned against real
    // hardware instead of reason. Read-only: it samples, it never purges.
    if let Some(secs) = args
        .iter()
        .position(|a| a == "--ram-log")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0 && n <= 7200)
    {
        let smc = smc_holder();
        let mut last = system::memory_stats();
        let mut last_t = Instant::now();
        let mut first = true;
        let deadline = Instant::now() + Duration::from_secs(secs);
        println!(
            "{{\"seconds\":{},\"note\":\"one JSON line per 2 s sample; churn is between this and the previous sample; exit 0\"}}",
            secs
        );
        while Instant::now() < deadline {
            let now = Instant::now();
            if let Some(m) = system::memory_stats() {
                let dt = now.duration_since(last_t).as_secs_f64();
                let churn = last
                    .as_ref()
                    .map(|b| ram::Churn::between(&b.counters(), &m.counters(), dt))
                    .unwrap_or_default();
                let temp = read_first(&smc, smc::CPU_TEMP_KEYS);
                let free_pct = if m.total_mb > 0.0 {
                    m.available_mb / m.total_mb * 100.0
                } else {
                    0.0
                };
                let compressed_pct = if m.total_mb > 0.0 {
                    m.compressed_mb / m.total_mb * 100.0
                } else {
                    0.0
                };
                let pressure =
                    memory::pressure_level(m.kernel_pressure, free_pct, m.swap_used_pct, compressed_pct);
                let band = ram::thermal_band(
                    temp,
                    m.swap_used_pct >= 100.0 || pressure == memory::Pressure::Critical,
                );
                let plan = ram::decide(&ram::Inputs {
                    enabled: true,
                    mode: crate::policy::Mode::Auto,
                    pressure,
                    band,
                    churn,
                    trend: memory::Trend::default(),
                    reclaimable_mb: m.reclaimable_mb,
                    swap_used_pct: m.swap_used_pct,
                    purge_since_s: u64::MAX,
                    purges_last_hour: 0,
                    last_purge: None,
                });
                println!(
                    "{{\"t\":{:.1},\"temp\":{},\"band\":\"{}\",\"pressure\":\"{}\",\"free_pct\":{:.1},\"compressed_mb\":{:.0},\"reclaimable_mb\":{:.0},\"swap_used_pct\":{:.1},\"decomp_s\":{:.1},\"swapin_s\":{:.1},\"heat_load\":{:.1},\"notable\":{},\"heavy\":{},\"plan\":\"{}\",\"reason\":\"{}\"}}",
                    dt,
                    temp.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "null".into()),
                    band.as_str(),
                    pressure.as_str(),
                    free_pct,
                    m.compressed_mb,
                    m.reclaimable_mb,
                    m.swap_used_pct,
                    churn.decompressions_per_s,
                    churn.swapins_per_s,
                    churn.heat_load(),
                    churn.is_notable(),
                    churn.is_heavy(),
                    plan.as_str(),
                    plan.reason.as_str()
                );
                last = Some(m);
                last_t = now;
            }
            thread::sleep(Duration::from_secs(2));
            let _ = first;
            first = false;
        }
        return;
    }
    if args.iter().any(|a| a == "--probe") {
        match smc::Smc::open() {
            Ok(s) => {
                println!("SMC opened.");
                for k in smc::CPU_TEMP_KEYS.iter() {
                    if let Ok((tag, raw)) = s.read_raw(k) {
                        println!(
                            "  {} [{}] = {:?} -> {:?}",
                            k,
                            tag,
                            raw,
                            smc::decode(&tag, &raw)
                        );
                    }
                }
                for k in ["F0Ac", "F0Mn", "F0Mx", "F0Tg", "F0ID"].iter() {
                    if let Ok((tag, raw)) = s.read_raw(k) {
                        println!(
                            "  {} [{}] = {:?} -> {:?}",
                            k,
                            tag,
                            raw,
                            smc::decode(&tag, &raw)
                        );
                    }
                }
            }
            Err(e) => println!("SMC open failed: {}", e),
        }
        return;
    }

    let _ = std::fs::create_dir_all(run_dir());
    log_line("thermalguardd starting");

    let shared = Arc::new(Mutex::new(Shared::default()));

    // Restore the last chosen mode so a reboot or daemon restart never quietly
    // drops the user back to an unprotected default.
    if let Ok(saved) = std::fs::read_to_string(state_path()) {
        let restored_mode = json::get_str(&saved, "mode").and_then(|v| Mode::parse(&v));
        let media_guard_off = json::get_bool(&saved, "media_guard") == Some(false);
        let audio_guard_off = json::get_bool(&saved, "audio_guard") == Some(false);
        let memory_reclaim_off = json::get_bool(&saved, "memory_reclaim") == Some(false);
        let auto_trim = json::get_bool(&saved, "auto_trim") != Some(false);
        let saved_fan = json::get_str(&saved, "fan_intent").and_then(|v| FanIntent::parse(&v));
        // The cap is persisted like the mode: a crash while it was engaged must
        // not leave the machine capped while the daemon believes it is not.
        let cap_was_on = json::get_bool(&saved, "cap_active") == Some(true);
        let tracked: Vec<String> = json::get_str_array(&saved, "auto_trimmed")
            .unwrap_or_default()
            .into_iter()
            .filter(|id| apple::is_auto_safe(id))
            .collect();

        let mut sh = shared.lock().unwrap();
        if let Some(m) = restored_mode {
            sh.mode = m;
            log_line(&format!("restored mode {}", m.as_str()));
        }
        sh.media_guard = !media_guard_off;
        sh.audio_guard = !audio_guard_off;
        sh.memory_reclaim = !memory_reclaim_off;
        sh.auto_trim = auto_trim;
        if let Some(f) = saved_fan {
            sh.fan_intent = f;
        }
        if cap_was_on {
            sh.cap_active = true;
            log_line("restored power cap state (engaged)");
        }
        sh.auto_trimmed = tracked;
        sh.auto_trimmed.sort();
        sh.auto_trimmed.dedup();
        if sh.auto_trim && sh.mode != Mode::Off {
            // A restart is not an excuse to disable something instantly.
            sh.trim_grace_until = Some(Instant::now() + Duration::from_secs(trim_grace_s()));
        }
        if !sh.auto_trimmed.is_empty() {
            sh.auto_trim_note = format!(
                "{} service(s) tracked from the last run.",
                sh.auto_trimmed.len()
            );
        }
    }

    {
        let smc = smc_holder();
        let mut sh = shared.lock().unwrap();
        sh.smc_ok = smc.is_some();
        if smc.is_none() {
            sh.smc_error = match smc::Smc::open() {
                Err(e) => e,
                Ok(_) => "unknown".into(),
            };
        }
        sh.fan_rpm = read_opt(&smc, "F0Ac");
        sh.fan_min = read_opt(&smc, "F0Mn");
        sh.fan_max = read_opt(&smc, "F0Mx");
        let (soft, lpm) = system::xcpm_ratios();
        sh.soft_ratio = soft;
        sh.lpm_ratio = lpm;
        drop(sh);
    }

    // IPC listener
    let sock = sock_path();
    let _ = std::fs::remove_file(&sock);
    // The daemon may start before a console user logs in. Keep the socket
    // reachable across that transition; peer_allowed authenticates every
    // connection by UID before any command is handled.
    let old_mask = unsafe { umask(0) };
    let bind_result = UnixListener::bind(&sock);
    unsafe {
        umask(old_mask);
    }
    match bind_result {
        Ok(listener) => {
            let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666));

            let sh = shared.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let s = match stream {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    // Connection cap: no thread-exhaustion from a connect loop.
                    if ACTIVE_CONNS.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                        log_line("dropped IPC connection: connection limit reached");
                        drop(s);
                        continue;
                    }
                    ACTIVE_CONNS.fetch_add(1, Ordering::SeqCst);
                    let sh = sh.clone();
                    thread::spawn(move || {
                        let _guard = ConnGuard;
                        // A panic in one request must not abort the daemon.
                        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            handle_client(s, sh)
                        }));
                        if res.is_err() {
                            log_line("IPC handler panicked; connection dropped");
                        }
                    });
                }
            });
        }
        Err(e) => log_line(&format!("socket bind failed: {}", e)),
    }

    // Main control loop.
    let mut last_demote_scan = Instant::now() - Duration::from_secs(10);
    let mut cached_speed_limit: Option<u32> = None;
    let mut last_speed_scan = Instant::now() - Duration::from_secs(10);
    let mut cached_ac = false;
    let mut last_ac_scan = Instant::now() - Duration::from_secs(60);
    let mut last_state_write = Instant::now() - Duration::from_secs(10);
    let mut cached_batt_temp: Option<f64> = None;
    let mut last_batt_scan = Instant::now() - Duration::from_secs(60);
    let mut last_mem_scan = Instant::now() - Duration::from_secs(2);
    // Short ring of Mach samples used for the compressor/swap trend.
    let mut mem_samples: Vec<(Instant, f64, f64)> = Vec::new();
    // Previous counters and sample time, so churn is a real delta rather than
    // a reading of a monotonic total.
    let mut last_mem_counters: Option<(Instant, ram::Counters)> = None;
    let mut last_net_scan = Instant::now() - Duration::from_secs(30);
    let mut last_trim_scan = Instant::now() - Duration::from_secs(15);
    let mut logged_cap = false;
    // The last process sample survives between scans (see the sampling block),
    // and the media shield stays up for a short hold after the last busy one.
    let mut last_procs: Vec<procs::Proc> = Vec::new();
    let mut media_until: Option<Instant> = None;
    // When the power cap last changed hands, so the Auto governor can enforce
    // its dwell time and cannot flap every tick. A cap restored from the state
    // file keeps its dwell instead of being released on the first tick.
    let cap_restored = shared.lock().unwrap().cap_active;
    let mut capped_since: Option<Instant> = if cap_restored {
        Some(Instant::now())
    } else {
        None
    };
    let mut uncapped_since: Option<Instant> = if cap_restored {
        None
    } else {
        Some(Instant::now())
    };
    // A refused `pmset` write is retried after a pause, not on every tick.
    let mut cap_failed: Option<Instant> = None;

    // Temperature smoothing: a small ring buffer to reduce SMC noise.
    // The SMC can return slightly different values on consecutive reads,
    // and when those values hover around a threshold the cap will flicker.
    // A short-term average prevents this without adding noticeable lag.
    const TEMP_SMOOTH_N: usize = 5;
    let mut temp_buffer: Vec<f64> = Vec::new();

    loop {
        let smc = smc_holder();
        let raw_temp = read_first(&smc, smc::CPU_TEMP_KEYS);
        // Smooth the temperature reading to prevent SMC noise from causing
        // the power cap to flicker when the temperature hovers around a threshold.
        let cpu_temp: Option<f64> = {
            let valid = raw_temp.filter(|t| t.is_finite() && *t >= -20.0 && *t <= 130.0);
            match valid {
                Some(v) => {
                    temp_buffer.push(v);
                    if temp_buffer.len() > TEMP_SMOOTH_N {
                        temp_buffer.remove(0);
                    }
                    let sum: f64 = temp_buffer.iter().sum();
                    Some(sum / temp_buffer.len() as f64)
                }
                None => {
                    // Keep the buffer drained when readings are invalid so we
                    // don't average stale data with new readings.
                    temp_buffer.clear();
                    None
                }
            }
        };
        // Keep smoothing for the dashboard and normal threshold decisions,
        // but never let it hide a real crossing of Auto's thermal ceiling.
        // On a fast all-core burst, five 2-second samples can otherwise let
        // the fan reach its loud region before the governor reacts.
        let policy_temp = match (cpu_temp, raw_temp) {
            (_, Some(raw)) if raw.is_finite() && raw >= 78.0 => Some(raw),
            (smoothed, _) => smoothed,
        };
        let fan_rpm = read_opt(&smc, "F0Ac");
        let fan_min = read_opt(&smc, "F0Mn");
        let fan_max = read_opt(&smc, "F0Mx");
        // Battery temperature comes from the SMC when it is there. The `ioreg`
        // fallback is a process spawn, so it is cached: battery temperature
        // moves on the order of minutes, not seconds.
        let battery_temp = match read_first(&smc, smc::BATTERY_TEMP_KEYS) {
            Some(v) => {
                cached_batt_temp = Some(v);
                Some(v)
            }
            None => {
                if last_batt_scan.elapsed() >= Duration::from_secs(15) {
                    last_batt_scan = Instant::now();
                    cached_batt_temp = system::battery_temp();
                }
                cached_batt_temp
            }
        };
        // `pmset` is a process spawn, so these are cached and refreshed on their
        // own slower cadence instead of twice per tick. Neither value changes
        // fast enough for a 2 s sample to buy anything.
        let speed_limit = if last_speed_scan.elapsed() >= Duration::from_secs(6) {
            last_speed_scan = Instant::now();
            cached_speed_limit = system::cpu_speed_limit();
            cached_speed_limit
        } else {
            cached_speed_limit
        };
        let ac = if last_ac_scan.elapsed() >= Duration::from_secs(30) {
            last_ac_scan = Instant::now();
            cached_ac = system::on_ac_power();
            cached_ac
        } else {
            cached_ac
        };

        let (mode, cap_active) = {
            let sh = shared.lock().unwrap();
            (sh.mode, sh.cap_active)
        };

        // `ps` is a fork, so the process list is refreshed every 4 s and *kept*
        // for the ticks in between. Deriving media detection, demotion and the
        // audio signal from an empty sample made every other tick look idle:
        // the media shield dropped and re-applied the power cap, demoted
        // processes were restored and re-demoted, and the audio spike streak
        // reset before it could ever reach its threshold.
        let procs_fresh = last_demote_scan.elapsed() >= Duration::from_secs(4);
        if procs_fresh {
            last_demote_scan = Instant::now();
            let sampled = procs::sample(14);
            if !sampled.is_empty() {
                last_procs = sampled;
            }
        }
        let procs_sample: Vec<procs::Proc> = last_procs.clone();

        // A busy browser or call app means the user is watching or talking:
        // that session is protected from throttling and from the power cap. A
        // busy sample holds the shield up for a moment, so a single quiet
        // sample in the middle of a call cannot hand the cap back and forth.
        if procs_fresh
            && procs_sample
                .iter()
                .any(|p| p.cpu >= 6.0 && procs::is_media(&p.name))
        {
            media_until = Some(Instant::now() + Duration::from_secs(MEDIA_HOLD_S));
        }
        let media_active = media_until.map(|t| Instant::now() < t).unwrap_or(false);

        // coreaudiod is protected from demotion, so its own CPU share is a
        // clean signal: a healthy engine idles near zero.
        let coreaudiod_cpu = procs_sample
            .iter()
            .find(|p| p.name.contains("coreaudiod"))
            .map(|p| p.cpu);

        // Memory is sampled slower than the CPU: the compressor moves on the
        // order of seconds, and this is two extra process spawns.
        // Mach-level sampling: one syscall, no child processes, so this runs on
        // every tick instead of every tenth.
        let mem = if last_mem_scan.elapsed() >= Duration::from_secs(2) {
            last_mem_scan = Instant::now();
            system::memory_stats()
        } else {
            None
        };
        // Churn: the part of memory management that becomes heat. Derived from
        // two cumulative-counter samples, so it is a rate, not a total.
        let mut churn = ram::Churn::default();
        if let Some(m) = &mem {
            let now = Instant::now();
            if let Some((then, counters)) = &last_mem_counters {
                churn = ram::Churn::between(
                    counters,
                    &m.counters(),
                    now.duration_since(*then).as_secs_f64(),
                );
            }
            last_mem_counters = Some((now, m.counters()));
            mem_samples.push((now, m.compressed_mb, m.swap_used_mb));
        }
        mem_samples.retain(|(t, _, _)| t.elapsed().as_secs_f64() < memory::TREND_WINDOW_S + 5.0);
        let mem_trend = memory::trend(
            &mem_samples
                .iter()
                .map(|(t, c, s)| (t.elapsed().as_secs_f64(), *c, *s))
                .collect::<Vec<(f64, f64, f64)>>(),
            memory::TREND_WINDOW_S,
        );

        // The link changes slowly and each probe costs a ping; 30 s is enough.
        let net = if last_net_scan.elapsed() >= Duration::from_secs(30) {
            last_net_scan = Instant::now();
            system::network_probe()
        } else {
            None
        };

        let inputs: Vec<(u32, String, f64)> = procs_sample
            .iter()
            .filter(|p| !procs::is_protected(&p.name))
            .map(|p| (p.pid, p.name.clone(), p.cpu))
            .collect();

        let capped_for_s = capped_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let (fan_intent, fan_held) = {
            let sh = shared.lock().unwrap();
            (
                sh.fan_intent,
                sh.fan_supported == Some(true) && sh.fan_target.is_some(),
            )
        };
        // Silent only costs performance where it actually buys silence. If the
        // fans cannot be held low on this model, its earlier cap would be a
        // pure speed loss.
        let policy_fan = if fan_intent == FanIntent::Quiet && !fan_held {
            FanIntent::Auto
        } else {
            fan_intent
        };
        let guard_on = {
            let sh = shared.lock().unwrap();
            sh.media_guard
        };
        // The battery/charging side of the policy: latched, so a battery
        // sitting on its threshold cannot shift the cap's engage point on
        // every sample (and the shift only applies while charging on AC).
        let battery_hot =
            policy::battery_heat_latched(battery_temp, shared.lock().unwrap().battery_hot);
        let mut decision = policy::apply_media_guard(
            policy::evaluate(
                mode,
                policy_fan,
                policy_temp,
                speed_limit,
                cap_active,
                capped_for_s,
                &inputs,
                battery_hot,
                ac,
            ),
            media_active && guard_on,
            policy_temp,
        );
        // Stability: once released, the cap stays off for a quiet window so a
        // workload sitting on the threshold cannot toggle it every dwell. Only
        // the adaptive modes have a threshold to sit on; a mode the user chose
        // explicitly engages at once, and a chip with no usable reading is
        // never left unprotected because of a timer.
        if !cap_active && decision.cap_power && matches!(mode, Mode::Auto | Mode::Balanced) {
            let off_for_s = uncapped_since
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(u64::MAX);
            if !policy::reengage_allowed(
                policy::Tuning::for_mode_fan(mode, policy_fan),
                off_for_s,
                policy_temp,
            ) {
                decision.cap_power = false;
            }
        }

        // 1) Power envelope.
        if decision.cap_power != cap_active {
            // A refused change is retried after a pause, never every tick.
            let backing_off = cap_failed
                .map(|t| t.elapsed().as_secs() < CAP_FAIL_BACKOFF_S)
                .unwrap_or(false);
            if !backing_off {
                match system::set_low_power_mode(decision.cap_power) {
                    Ok(_) => {
                        cap_failed = None;
                        capped_since = if decision.cap_power {
                            Some(Instant::now())
                        } else {
                            None
                        };
                        uncapped_since = if decision.cap_power {
                            None
                        } else {
                            Some(Instant::now())
                        };
                        let mut sh = shared.lock().unwrap();
                        sh.cap_active = decision.cap_power;
                        // Persist at once: a crash between the `pmset` write and the
                        // periodic state write would otherwise lose the fact that
                        // the machine is capped.
                        sh.dirty = true;
                        if decision.cap_power {
                            let msg = format!(
                                "power cap ON (turbo ratio {} -> {})",
                                sh.soft_ratio.unwrap_or(0),
                                sh.lpm_ratio.unwrap_or(0)
                            );
                            push_event(&mut sh, &msg);
                        } else {
                            push_event(&mut sh, "power cap OFF (full turbo available)");
                        }
                        logged_cap = false;
                    }
                    Err(e) => {
                        // Back off, or a machine that refuses the write would spawn
                        // `pmset` on every 2 s tick for the life of the process.
                        cap_failed = Some(Instant::now());
                        if !logged_cap {
                            let mut sh = shared.lock().unwrap();
                            push_event(&mut sh, &format!("power cap change failed: {}", e));
                            logged_cap = true;
                        }
                    }
                }
            }
        }

        // 2) Background load demotion.
        //
        // The pids came from a `ps` snapshot taken earlier in this tick, so a
        // process may have exited and its pid been reused in the meantime.
        // Confirm identity immediately before touching it, so background QoS
        // is never applied to an unrelated process.
        let already: Vec<u32> = {
            let sh = shared.lock().unwrap();
            sh.demoted.iter().map(|(p, _)| *p).collect()
        };
        let mut newly = Vec::new();
        for (pid, name) in &decision.demote {
            // Already demoted: re-applying it every tick would just fork `ps`
            // for a process whose priority has not changed.
            if already.contains(pid) {
                continue;
            }
            if procs::same_process(*pid, name) && procs::demote(*pid) {
                newly.push((*pid, name.clone()));
            }
        }

        // 3) Core Audio watchdog. The decision is made under the lock and the
        //    signal is sent outside it, so the UI is never blocked behind a
        //    restart and the policy stays a pure function.
        let audio_action = {
            let mut sh = shared.lock().unwrap();
            sh.coreaudiod_cpu = coreaudiod_cpu;
            // Only a fresh sample is evidence: folding the cached one twice
            // would count the same observation as a sustained spike.
            if procs_fresh {
                sh.audio_streak = audio::spike_streak(sh.audio_streak, coreaudiod_cpu);
            }
            sh.audio_restarts_window
                .retain(|t| t.elapsed().as_secs() < 3600);
            let inputs = audio::Inputs {
                enabled: sh.audio_guard,
                mode_off: sh.mode == Mode::Off,
                spike_streak: sh.audio_streak,
                since_last_s: sh
                    .audio_last
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(u64::MAX),
                restarts_last_hour: sh.audio_restarts_window.len() as u32,
            };
            audio::plan(&inputs)
        };
        if audio_action == audio::Action::Restart {
            match system::restart_coreaudio() {
                Ok(note) => {
                    let mut sh = shared.lock().unwrap();
                    sh.audio_restarts += 1;
                    sh.audio_last = Some(Instant::now());
                    sh.audio_restarts_window.push(Instant::now());
                    sh.audio_streak = 0;
                    sh.audio_note = note;
                    push_event(&mut sh, "core audio engine reset after a sustained spike");
                }
                Err(e) => {
                    let mut sh = shared.lock().unwrap();
                    sh.audio_streak = 0;
                    sh.audio_note = format!("Core Audio reset failed: {}", e);
                    push_event(&mut sh, &format!("core audio reset failed: {}", e));
                }
            }
        }

        // 4) Memory: decide the ladder under the lock, perform the side effect
        //    outside it, then measure the result. The governor is thermal-aware,
        //    so a hot chip does not get an extra I/O burst for advisory
        //    pressure, and a climbing compressor is stopped while it is cheap.
        let memory_decision = {
            let mut sh = shared.lock().unwrap();
            if let Some(m) = &mem {
                // "Free" for a person means memory the kernel can hand out
                // without reclaiming: free + inactive + speculative. Free
                // pages alone sit near zero on a healthy Mac by design, which
                // is what made the UI read "Free 0%".
                let free_pct = if m.total_mb > 0.0 {
                    m.available_mb / m.total_mb * 100.0
                } else {
                    0.0
                };
                let compressed_pct = if m.total_mb > 0.0 {
                    m.compressed_mb / m.total_mb * 100.0
                } else {
                    0.0
                };
                sh.mem_pressure = memory::pressure_level(
                    m.kernel_pressure,
                    free_pct,
                    m.swap_used_pct,
                    compressed_pct,
                );
                sh.mem_free_pct = free_pct;
                sh.mem_total_mb = m.total_mb;
                sh.mem_available_mb = m.available_mb;
                sh.mem_compressed_mb = m.compressed_mb;
                sh.mem_reclaimable_mb = m.reclaimable_mb;
                sh.mem_swap_used_mb = m.swap_used_mb;
                sh.mem_swap_total_mb = m.swap_total_mb;
                sh.mem_swap_used_pct = m.swap_used_pct;
                sh.mem_churn = churn;
                sh.mem_band = ram::thermal_band(
                    cpu_temp,
                    speed_limit
                        .map(|v| v < policy::THROTTLE_LIMIT)
                        .unwrap_or(false),
                );
            }
            sh.memory_window.retain(|t| t.elapsed().as_secs() < 3600);
            sh.memory_trend = mem_trend;
            let inputs = ram::Inputs {
                enabled: sh.memory_reclaim,
                mode: sh.mode,
                pressure: sh.mem_pressure,
                band: sh.mem_band,
                churn: sh.mem_churn,
                trend: sh.memory_trend,
                reclaimable_mb: sh.mem_reclaimable_mb,
                swap_used_pct: sh.mem_swap_used_pct,
                purge_since_s: sh
                    .memory_last
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(u64::MAX),
                purges_last_hour: sh.memory_window.len() as u32,
                last_purge: sh.memory_last.and_then(|t| {
                    sh.memory_freed_mb
                        .map(|freed| (freed, t.elapsed().as_secs()))
                }),
            };
            let decision = ram::decide(&inputs);
            sh.memory_decision = decision;
            decision
        };
        if memory_decision.rung == ram::Rung::DropCache {
            match system::purge_reclaimable() {
                Ok(_) => {
                    let after = system::memory_stats();
                    let freed_mb = match (mem.as_ref(), after.as_ref()) {
                        (Some(b), Some(a)) => (b.reclaimable_mb - a.reclaimable_mb).max(0.0),
                        _ => 0.0,
                    };
                    let note = match (mem.as_ref(), after.as_ref()) {
                        (Some(b), Some(a)) => format!(
                            "Reclaimed {:.0} MB of cache (free {:.0} -> {:.0} MB, compressed {:.0} -> {:.0} MB) [{}].",
                            freed_mb,
                            b.free_mb,
                            a.free_mb,
                            b.compressed_mb,
                            a.compressed_mb,
                            memory_decision.reason.as_str()
                        ),
                        _ => "Purgeable and file-backed cache was released.".to_string(),
                    };
                    let mut sh = shared.lock().unwrap();
                    sh.memory_reclaims += 1;
                    let now = Instant::now();
                    sh.memory_last = Some(now);
                    sh.memory_freed_mb = Some(freed_mb);
                    sh.memory_window.push(now);
                    sh.memory_note = note.clone();
                    push_event(&mut sh, &format!("memory reclaim: {}", note));
                }
                Err(e) => {
                    let mut sh = shared.lock().unwrap();
                    // Count the failed attempt against the cooldown, or a
                    // machine where `purge` is broken would spawn it on every
                    // 2 s tick. Same reasoning as `cap_failed`.
                    let now = Instant::now();
                    sh.memory_last = Some(now);
                    sh.memory_freed_mb = Some(0.0);
                    sh.memory_window.push(now);
                    sh.memory_note = format!("Memory reclaim failed: {}", e);
                    push_event(&mut sh, &format!("memory reclaim failed: {}", e));
                }
            }
        }

        {
            let mut sh = shared.lock().unwrap();
            sh.cpu_temp = cpu_temp;
            sh.fan_rpm = fan_rpm;
            sh.fan_min = fan_min;
            sh.fan_max = fan_max;
            sh.battery_temp = battery_temp;
            sh.battery_hot = battery_hot;
            sh.speed_limit = speed_limit;
            sh.ac = ac;
            sh.procs = procs_sample.clone();
            sh.media_active = media_active;
            for (pid, name) in newly {
                if !sh.demoted.iter().any(|(p, _)| *p == pid) {
                    sh.demoted.push((pid, name));
                }
            }
            // Drop entries that are no longer hot so they get their QoS back.
            let hot: Vec<u32> = decision.demote.iter().map(|(p, _)| *p).collect();
            let stale: Vec<(u32, String)> = sh
                .demoted
                .iter()
                .filter(|(p, _)| !hot.contains(p))
                .cloned()
                .collect();
            for (pid, name) in &stale {
                // PID reuse guard: only hand priority back if that PID is still
                // the same executable we demoted.
                if procs::same_process(*pid, name) {
                    procs::restore(*pid);
                }
            }
            sh.demoted.retain(|(p, _)| hot.contains(p));

            drive_fan(&smc, &mut sh, cpu_temp);

            // Auto-mode trimmer: checks every 15 s, at most one safe change
            // per check, so OS/non-OS services are never impacted together.
            if last_trim_scan.elapsed() >= Duration::from_secs(15) {
                last_trim_scan = Instant::now();
                reconcile_auto_trim(&mut sh);
            }

            if let Some(n) = net.as_ref() {
                let report = net::evaluate(&net::Inputs {
                    link: n.link,
                    rssi_dbm: n.rssi_dbm,
                    noise_dbm: n.noise_dbm,
                    tx_mbps: n.tx_mbps,
                    gateway_ms: n.gateway_ms,
                    loss_pct: n.loss_pct,
                });
                sh.net_link = n.link.as_str().to_string();
                sh.net_interface = n.interface();
                sh.net_rssi = n.rssi_dbm;
                sh.net_tx_mbps = n.tx_mbps;
                sh.net_gateway_ms = n.gateway_ms;
                sh.net_loss_pct = n.loss_pct;
                sh.net_score = report.score;
                sh.net_grade = report.grade.to_string();
                sh.net_note = report.note();
            }

            // Persist immediately when something user-visible changed; otherwise
            // the file is only touched every 10 s for external tooling.
            if sh.dirty || last_state_write.elapsed() >= Duration::from_secs(10) {
                last_state_write = Instant::now();
                sh.dirty = false;
                write_state(&sh);
            }
        }

        thread::sleep(Duration::from_secs(2));
    }
}
