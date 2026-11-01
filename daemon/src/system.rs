//! Thin wrappers over the macOS power-management interfaces.
//!
//! Every child process is invoked by absolute path so a modified `PATH` cannot
//! redirect a root daemon to an attacker-controlled binary.

use std::process::Command;

const PMSET: &str = "/usr/bin/pmset";
const SYSCTL: &str = "/usr/sbin/sysctl";
const IOREG: &str = "/usr/sbin/ioreg";
const DATE: &str = "/bin/date";
const STAT: &str = "/usr/bin/stat";
const KILLALL: &str = "/usr/bin/killall";
const PGREP: &str = "/usr/bin/pgrep";
const VM_STAT: &str = "/usr/bin/vm_stat";
const PURGE: &str = "/usr/bin/purge";

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// UID of the logged-in console user (falls back to 501 for a headless boot).
pub fn console_uid() -> u32 {
    run(STAT, &["-f", "%u", "/dev/console"])
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(501)
}

/// Primary GID of the console user (used to own the IPC socket).
pub fn console_gid() -> u32 {
    run(STAT, &["-f", "%g", "/dev/console"])
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(20)
}

/// Username of the logged-in console user, empty when nobody is logged in.
pub fn console_user() -> String {
    let user = run(STAT, &["-f", "%Su", "/dev/console"]).unwrap_or_default();
    let user = user.trim().to_string();
    if user.contains('/') || user.contains(char::is_control) {
        return String::new();
    }
    user
}

/// macOS' own supported power cap. On Intel this drops the ceiling from the
/// turbo ratio (35 x 100 MHz = 3.5 GHz) to the low-power ratio (24 x 100 MHz),
/// which is what actually removes the sustained heat burst.
pub fn set_low_power_mode(on: bool) -> Result<(), String> {
    if let Some(cur) = low_power_mode() {
        if cur == on {
            return Ok(());
        }
    }
    let out = Command::new(PMSET)
        .args(["-a", "lowpowermode", if on { "1" } else { "0" }])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn low_power_mode() -> Option<bool> {
    let text = run(PMSET, &["-g", "custom"])?;
    let mut in_ac = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("AC Power") {
            in_ac = true;
        } else if t.starts_with("Battery Power") {
            in_ac = false;
        } else if in_ac && t.starts_with("lowpowermode") {
            return t.split_whitespace().nth(1).map(|v| v == "1");
        }
    }
    None
}

/// `CPU_Speed_Limit` from the thermal monitor: 100 = unthrottled, below that
/// macOS is actively starving the CPU because of heat.
pub fn cpu_speed_limit() -> Option<u32> {
    let text = run(PMSET, &["-g", "therm"])?;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("CPU_Speed_Limit") {
            return v
                .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                .trim()
                .parse::<u32>()
                .ok();
        }
    }
    None
}

/// The XCPM frequency ceilings, in 100 MHz units.
pub fn xcpm_ratios() -> (Option<u32>, Option<u32>) {
    let text = match run(SYSCTL, &["-a"]) {
        Some(t) => t,
        None => return (None, None),
    };
    let mut soft = None;
    let mut lpm = None;
    for line in text.lines() {
        if let Some(v) = line.split("machdep.xcpm.soft_plimit_max_100mhz_ratio:").nth(1) {
            soft = v.trim().parse::<u32>().ok();
        }
        if let Some(v) = line.split("machdep.xcpm.lpm_plimit_max_100mhz_ratio:").nth(1) {
            lpm = v.trim().parse::<u32>().ok();
        }
    }
    (soft, lpm)
}

/// Battery temperature in degrees Celsius (AppleSmartBattery reports deciKelvin).
pub fn battery_temp() -> Option<f64> {
    let text = run(IOREG, &["-rc", "AppleSmartBattery"])?;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("\"Temperature\"") {
            let val = v
                .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                .trim()
                .parse::<f64>()
                .ok()?;
            return Some(val / 10.0 - 273.15);
        }
    }
    None
}

pub fn on_ac_power() -> bool {
    run(PMSET, &["-g", "batt"])
        .map(|t| t.contains("AC Power"))
        .unwrap_or(false)
}

/// A snapshot of what the kernel reports about memory. Everything here is
/// measured, never inferred: the app only decides what to do with it.
#[derive(Clone, Copy, Debug)]
pub struct MemStats {
    pub total_mb: f64,
    pub free_mb: f64,
    pub compressed_mb: f64,
    /// Purgeable + file-backed pages: cache the kernel can drop on demand.
    pub reclaimable_mb: f64,
    pub swap_used_pct: f64,
    /// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical.
    /// This is the kernel's own verdict and outranks any page heuristic.
    pub kernel_pressure: Option<u8>,
}

fn pages(text: &str, key: &str) -> Option<f64> {
    for line in text.lines() {
        let (name, rest) = line.split_once(':')?;
        if name.trim() == key {
            return rest.trim().trim_end_matches('.').parse::<f64>().ok();
        }
    }
    None
}

fn mib(value: &str, key: &str) -> Option<f64> {
    let rest = value.split(key).nth(1)?;
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse::<f64>().ok()
}

/// Real numbers from `vm_stat` and `vm.swapusage`, including how much cache is
/// actually available to reclaim. A purge is only justified when that number is
/// large — otherwise it frees nothing and just stalls the disk.
pub fn memory_stats() -> Option<MemStats> {
    let page_size = run(SYSCTL, &["-n", "hw.pagesize"])
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(4096.0);
    let total_bytes = run(SYSCTL, &["-n", "hw.memsize"])
        .and_then(|s| s.trim().parse::<f64>().ok())?;
    let total_mb = total_bytes / 1_048_576.0;

    let text = run(VM_STAT, &[])?;
    let to_mb = |p: f64| p * page_size / 1_048_576.0;
    let free_mb = to_mb(pages(&text, "Pages free").unwrap_or(0.0));
    let compressed_mb = to_mb(pages(&text, "Pages occupied by compressor").unwrap_or(0.0));
    let reclaimable_mb = to_mb(
        pages(&text, "Pages purgeable").unwrap_or(0.0)
            + pages(&text, "File-backed pages").unwrap_or(0.0),
    );

    let swap = run(SYSCTL, &["-n", "vm.swapusage"]).unwrap_or_default();
    let swap_total = mib(&swap, "total = ").unwrap_or(0.0);
    let swap_used = mib(&swap, "used = ").unwrap_or(0.0);
    let swap_used_pct = if swap_total > 0.0 {
        (swap_used / swap_total * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    Some(MemStats {
        total_mb,
        free_mb,
        compressed_mb,
        reclaimable_mb,
        swap_used_pct,
        kernel_pressure: run(SYSCTL, &["-n", "kern.memorystatus_vm_pressure_level"])
            .and_then(|s| s.trim().parse::<u8>().ok()),
    })
}

/// `/usr/bin/purge` in production; override refused when root.
fn purge_path() -> String {
    if crate::overrides_allowed() {
        if let Ok(v) = std::env::var("THERMALGUARD_PURGE") {
            return v;
        }
    }
    PURGE.to_string()
}

/// Ask the kernel to release purgeable and file-backed cache. No arguments, no
/// caller input, and the effect is measured by the caller afterwards.
pub fn purge_reclaimable() -> Result<(), String> {
    let out = Command::new(purge_path())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn now_hms() -> String {
    run(DATE, &["+%H:%M:%S"])
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}

/// `/usr/bin/killall` in production. The override exists only for the
/// integration test suite and is refused whenever the process is root, so an
/// installed daemon can never be steered at an arbitrary binary.
fn killall_path() -> String {
    if crate::overrides_allowed() {
        if let Ok(v) = std::env::var("THERMALGUARD_KILLALL") {
            return v;
        }
    }
    KILLALL.to_string()
}

/// PID of the Core Audio engine, used only to report whether it came back.
pub fn coreaudiod_pid() -> Option<u32> {
    let out = Command::new(PGREP).args(["-x", "coreaudiod"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Ask launchd to replace Core Audio with a **graceful** SIGTERM.
///
/// Deliberately not SIGKILL: the audio daemon gets to tear its IOProc graph
/// down instead of leaving clients pointing at a dead HAL. launchd owns the
/// service, so it comes back on its own in about a second. The only argument
/// is the compiled-in process name — nothing from the IPC wire reaches this
/// call — and the result is verified by looking for the new PID.
pub fn restart_coreaudio() -> Result<String, String> {
    let out = Command::new(killall_path())
        .args(["-TERM", "coreaudiod"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "coreaudiod is not running".to_string()
        } else {
            err
        });
    }
    // Give launchd a moment, then report what we can actually observe.
    std::thread::sleep(std::time::Duration::from_millis(400));
    Ok(match coreaudiod_pid() {
        Some(pid) => format!("Core Audio engine restarted by launchd (pid {}).", pid),
        None => "Core Audio signalled; launchd is relaunching it.".to_string(),
    })
}
