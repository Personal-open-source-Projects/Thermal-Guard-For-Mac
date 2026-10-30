//! thermalguardd — the privileged thermal core of ThermalGuard.
//!
//! Runs as a root LaunchDaemon. Samples the SMC, decides a power/heat policy
//! and applies it. Exposes state to the Swift UI over a unix socket so the UI
//! never needs root itself.

mod agents;
mod apple;
mod json;
mod policy;
mod procs;
mod smc;
mod system;

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
fn overrides_allowed() -> bool {
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

struct Shared {
    mode: Mode,
    fan_intent: FanIntent,
    cpu_temp: Option<f64>,
    fan_rpm: Option<f64>,
    fan_min: Option<f64>,
    fan_max: Option<f64>,
    battery_temp: Option<f64>,
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
    procs: Vec<procs::Proc>,
    demoted: Vec<(u32, String)>,
    events: Vec<String>,
    smc_ok: bool,
    smc_error: String,
    soft_ratio: Option<u32>,
    lpm_ratio: Option<u32>,
    started: Instant,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            mode: Mode::Auto,
            fan_intent: FanIntent::Auto,
            cpu_temp: None,
            fan_rpm: None,
            fan_min: None,
            fan_max: None,
            battery_temp: None,
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
            procs: Vec::new(),
            demoted: Vec::new(),
            events: Vec::new(),
            smc_ok: false,
            smc_error: String::new(),
            soft_ratio: None,
            lpm_ratio: None,
            started: Instant::now(),
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
        json::s("mode", sh.mode.as_str()),
        json::s("mode_label", sh.mode.label()),
        json::s(
            "fan_intent",
            sh.fan_intent.as_str(),
        ),
        format!("\"fan_target\":{}", opt_num(sh.fan_target)),
        format!("\"cpu_temp\":{}", opt_num(sh.cpu_temp)),
        format!("\"fan_rpm\":{}", opt_num(sh.fan_rpm)),
        format!("\"fan_min\":{}", opt_num(sh.fan_min)),
        format!("\"fan_max\":{}", opt_num(sh.fan_max)),
        format!("\"battery_temp\":{}", opt_num(sh.battery_temp)),
        match sh.speed_limit {
            Some(v) => json::i("speed_limit", v as i64),
            None => "\"speed_limit\":null".to_string(),
        },
        json::b("ac_power", sh.ac),
        json::b("media_active", sh.media_active),
        json::b("media_guard", sh.media_guard),
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
    let wrote = s.write_raw("F0Tg", "fpe2", &smc::encode_fpe2(target)).is_ok()
        || s.write_raw("F0Tg", "flt ", &smc::encode_flt(target)).is_ok();
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
        FanIntent::Max => sh.fan_max.map(FanCommand::Target),
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
                || s.write_raw("F0Tg", "fpe2", &smc::encode_fpe2(target)).is_ok();
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

/// Only root and the logged-in console user may drive this root daemon. The
/// socket is mode 0666 for convenience, so this check is what actually keeps
/// other local accounts out.
fn peer_allowed(stream: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut uid: u32 = u32::MAX;
    let mut gid: u32 = u32::MAX;
    let rc = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return false;
    }
    uid == 0 || uid == system::console_uid()
}

/// Labels come from the wire, so keep them to a printable safe alphabet before
/// they are logged or compared.
fn safe_label(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ' '))
        .take(120)
        .collect()
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
            let enable = req.contains("\"enable\":true");
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
        let body = apple::items_json(&apple::scan(), &note);
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
            let enable = req.contains("\"enable\":true");
            // No path or scope from the caller: the daemon resolves them itself
            // by label, so it cannot be steered at an arbitrary plist.
            match agents::set_enabled(&label, enable) {
                Ok(_) => {
                    let msg = format!(
                        "{} {}",
                        if enable { "enabled" } else { "disabled" },
                        label
                    );
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
    }

    let mut apply_mode: Option<Mode> = None;
    let mut apply_fan: Option<FanIntent> = None;

    match cmd.as_str() {
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
        _ => {}
    }

    {
        let mut sh = shared.lock().unwrap();
        if let Some(m) = apply_mode {
            if sh.mode != m {
                push_event(&mut sh, &format!("mode -> {}", m.label()));
            }
            sh.mode = m;
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
            match f {
                FanIntent::Max => {
                    // Never ask for a value outside the SMC's own reported range.
                    let hi = sh.fan_max.unwrap_or(7200.0);
                    let lo = sh.fan_min.unwrap_or(0.0);
                    let target = hi.clamp(lo, hi.max(lo));
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
                FanIntent::Quiet => {
                    sh.fan_quiet_allowed = true;
                    // New request: allow one fresh attempt at the SMC.
                    sh.fan_failed = None;
                    sh.fan_note = "Silent requested: quieter fan if the SMC allows it, earlier power cap either way.".into();
                }
                FanIntent::Auto => {
                    sh.fan_note = "Fan left to the SMC curve.".into();
                }
            }
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
        println!("thermalguardd 1.0.0");
        return;
    }
    if args.iter().any(|a| a == "--probe") {
        match smc::Smc::open() {
            Ok(s) => {
                println!("SMC opened.");
                for k in smc::CPU_TEMP_KEYS.iter() {
                    if let Ok((tag, raw)) = s.read_raw(k) {
                        println!("  {} [{}] = {:?} -> {:?}", k, tag, raw, smc::decode(&tag, &raw));
                    }
                }
                for k in ["F0Ac", "F0Mn", "F0Mx", "F0Tg", "F0ID"].iter() {
                    if let Ok((tag, raw)) = s.read_raw(k) {
                        println!("  {} [{}] = {:?} -> {:?}", k, tag, raw, smc::decode(&tag, &raw));
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
        if let Some(m) = json::get_str(&saved, "mode").and_then(|v| Mode::parse(&v)) {
            shared.lock().unwrap().mode = m;
            log_line(&format!("restored mode {}", m.as_str()));
        }
        if saved.contains("\"media_guard\":false") {
            shared.lock().unwrap().media_guard = false;
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
    // Create the socket 0600 from the very first instant (umask), so there is
    // no window in which another local user could connect before the chmod.
    let old_mask = unsafe { umask(0o177) };
    let bind_result = UnixListener::bind(&sock);
    unsafe {
        umask(old_mask);
    }
    match bind_result {
        Ok(listener) => {
            // Owner-only socket: hand it to the logged-in console user, mode
            // 0600. The peer-credential check stays as defence in depth.
            let uid = system::console_uid();
            let gid = system::console_gid();
            if let Err(e) = std::os::unix::fs::chown(&sock, Some(uid), Some(gid)) {
                log_line(&format!("socket chown failed: {}", e));
            }
            let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));

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
    let mut logged_cap = false;
    // When the power cap last changed hands, so the Auto governor can enforce
    // its dwell time and cannot flap every tick.
    let mut capped_since: Option<Instant> = None;

    loop {
        let smc = smc_holder();
        let cpu_temp = read_first(&smc, smc::CPU_TEMP_KEYS);
        let fan_rpm = read_opt(&smc, "F0Ac");
        let fan_min = read_opt(&smc, "F0Mn");
        let fan_max = read_opt(&smc, "F0Mx");
        let battery_temp = read_first(&smc, smc::BATTERY_TEMP_KEYS).or_else(system::battery_temp);
        let speed_limit = system::cpu_speed_limit();
        let ac = system::on_ac_power();

        let (mode, cap_active) = {
            let sh = shared.lock().unwrap();
            (sh.mode, sh.cap_active)
        };

        let procs_sample = if last_demote_scan.elapsed() >= Duration::from_secs(2) {
            last_demote_scan = Instant::now();
            procs::sample(14)
        } else {
            Vec::new()
        };

        // A busy browser or call app means the user is watching or talking:
        // that session is protected from throttling and from the power cap.
        let media_active = procs_sample
            .iter()
            .any(|p| p.cpu >= 6.0 && procs::is_media(&p.name));

        let inputs: Vec<(u32, String, f64)> = procs_sample
            .iter()
            .filter(|p| !procs::is_protected(&p.name))
            .map(|p| (p.pid, p.name.clone(), p.cpu))
            .collect();

        let capped_for_s = capped_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let fan_intent = {
            let sh = shared.lock().unwrap();
            sh.fan_intent
        };
        let guard_on = {
            let sh = shared.lock().unwrap();
            sh.media_guard
        };
        let decision = policy::apply_media_guard(
            policy::evaluate(
                mode,
                fan_intent,
                cpu_temp,
                speed_limit,
                cap_active,
                capped_for_s,
                &inputs,
            ),
            media_active && guard_on,
            cpu_temp,
        );

        // 1) Power envelope.
        if decision.cap_power != cap_active {
            match system::set_low_power_mode(decision.cap_power) {
                Ok(_) => {
                    capped_since = if decision.cap_power {
                        Some(Instant::now())
                    } else {
                        None
                    };
                    let mut sh = shared.lock().unwrap();
                    sh.cap_active = decision.cap_power;
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
                    logged_cap = true;
                }
                Err(e) => {
                    if !logged_cap {
                        let mut sh = shared.lock().unwrap();
                        push_event(&mut sh, &format!("power cap change failed: {}", e));
                        logged_cap = true;
                    }
                }
            }
        }

        // 2) Background load demotion.
        let mut newly = Vec::new();
        for (pid, name) in &decision.demote {
            if procs::demote(*pid) {
                newly.push((*pid, name.clone()));
            }
        }

        {
            let mut sh = shared.lock().unwrap();
            sh.cpu_temp = cpu_temp;
            sh.fan_rpm = fan_rpm;
            sh.fan_min = fan_min;
            sh.fan_max = fan_max;
            sh.battery_temp = battery_temp;
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

            write_state(&sh);
        }

        thread::sleep(Duration::from_secs(2));
    }
}
