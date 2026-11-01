//! Thin wrappers over the macOS power-management interfaces.
//!
//! Every child process is invoked by absolute path so a modified `PATH` cannot
//! redirect a root daemon to an attacker-controlled binary.

use std::os::raw::{c_char, c_int, c_void};
use std::process::Command;

const PMSET: &str = "/usr/bin/pmset";
const SYSCTL: &str = "/usr/sbin/sysctl";
const IOREG: &str = "/usr/sbin/ioreg";
const DATE: &str = "/bin/date";
const STAT: &str = "/usr/bin/stat";
const KILLALL: &str = "/usr/bin/killall";
const PGREP: &str = "/usr/bin/pgrep";
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

/// A snapshot of what the kernel reports about memory, read through Mach and
/// `sysctlbyname` directly.
///
/// This is the hot path: one `host_statistics64` call plus three sysctls, all
/// in-process. No `vm_stat` or `sysctl` child is spawned, so a sample costs
/// microseconds instead of milliseconds and can run on every daemon tick.
#[derive(Clone, Copy, Debug)]
pub struct MemStats {
    pub total_mb: f64,
    /// Free plus speculative pages: memory available without reclaim.
    pub free_mb: f64,
    pub compressed_mb: f64,
    /// Purgeable + file-backed (external) pages: cache the kernel can drop.
    pub reclaimable_mb: f64,
    pub swap_used_mb: f64,
    pub swap_total_mb: f64,
    pub swap_used_pct: f64,
    /// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical.
    pub kernel_pressure: Option<u8>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XswUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: u32,
}

const HOST_VM_INFO64: c_int = 4;
const MIB: f64 = 1_048_576.0;

extern "C" {
    fn mach_host_self() -> u32;
    fn host_statistics64(host: u32, flavor: c_int, info: *mut i32, count: *mut u32) -> c_int;
    fn mach_port_deallocate(task: u32, name: u32) -> c_int;
    static mach_task_self_: u32;
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
}

fn vm_statistics() -> Option<VmStatistics64> {
    let host = unsafe { mach_host_self() };
    if host == 0 {
        return None;
    }
    let mut stats = VmStatistics64::default();
    let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<i32>()) as u32;
    let kr = unsafe {
        host_statistics64(
            host,
            HOST_VM_INFO64,
            &mut stats as *mut VmStatistics64 as *mut i32,
            &mut count,
        )
    };
    unsafe {
        mach_port_deallocate(mach_task_self_, host);
    }
    if kr == 0 {
        Some(stats)
    } else {
        None
    }
}

fn sysctl_named<T: Default>(name: &str) -> Option<T> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut out = T::default();
    let mut len = std::mem::size_of::<T>();
    let rc = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            &mut out as *mut T as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && len == std::mem::size_of::<T>() {
        Some(out)
    } else {
        None
    }
}

pub fn memory_stats() -> Option<MemStats> {
    let vm = vm_statistics()?;
    let page_size = sysctl_named::<u32>("hw.pagesize")
        .map(|v| v as f64)
        .filter(|v| *v > 0.0)
        .unwrap_or(4096.0);
    let total_mb = sysctl_named::<u64>("hw.memsize")? as f64 / MIB;
    let to_mb = |pages: u32| pages as f64 * page_size / MIB;
    let swap = sysctl_named::<XswUsage>("vm.swapusage").unwrap_or_default();
    let swap_total_mb = swap.total as f64 / MIB;
    let swap_used_mb = swap.used as f64 / MIB;
    let swap_used_pct = if swap_total_mb > 0.0 {
        (swap_used_mb / swap_total_mb * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    Some(MemStats {
        total_mb,
        free_mb: to_mb(vm.free_count + vm.speculative_count),
        compressed_mb: to_mb(vm.compressor_page_count),
        reclaimable_mb: to_mb(vm.purgeable_count + vm.external_page_count),
        swap_used_mb,
        swap_total_mb,
        swap_used_pct,
        kernel_pressure: sysctl_named::<u32>("kern.memorystatus_vm_pressure_level")
            .map(|v| v as u8),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mach_sampler_is_plausible_and_cheap() {
        let m = memory_stats().expect("memory stats must be readable on macOS");
        assert!(m.total_mb > 0.0, "total memory unknown");
        assert!(m.free_mb >= 0.0 && m.free_mb <= m.total_mb);
        assert!(m.compressed_mb >= 0.0 && m.compressed_mb <= m.total_mb);
        assert!(m.reclaimable_mb >= 0.0 && m.reclaimable_mb <= m.total_mb);
        assert!((0.0..=100.0).contains(&m.swap_used_pct));
        assert!(m.kernel_pressure.is_some(), "kernel pressure must be readable");

        // The whole point of the Mach path: no child processes. 200 samples
        // spawning `vm_stat` would take hundreds of milliseconds.
        let t = std::time::Instant::now();
        for _ in 0..200 {
            let _ = memory_stats();
        }
        assert!(
            t.elapsed().as_micros() < 200_000,
            "200 samples took {:?}, which means a fork slipped in",
            t.elapsed()
        );
    }
}
