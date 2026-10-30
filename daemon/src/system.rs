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

pub fn now_hms() -> String {
    run(DATE, &["+%H:%M:%S"])
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}
