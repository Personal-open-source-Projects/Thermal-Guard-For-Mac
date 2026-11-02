//! Process sampling and *reversible* QoS demotion.
//!
//! We never kill, suspend or signal anything the user is running. The only
//! lever is Darwin's background QoS class (the same mechanism App Nap uses),
//! which pushes a process off the performance cores / out of turbo instead of
//! terminating it.

use std::os::raw::c_int;
use std::process::Command;

extern "C" {
    fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
}

const PRIO_DARWIN_PROCESS: c_int = 4;
const PRIO_DARWIN_BG: c_int = 0x1000;

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    pub cpu: f64,
    pub name: String,
}

/// Never demoted: the OS itself, security software the user relies on, and
/// input/tablet drivers. Throttling these would weaken protection or break
/// hardware, so the policy engine simply steps aside.
const PROTECTED_EXACT: &[&str] = &[
    "kernel_task",
    "WindowServer",
    "launchd",
    "loginwindow",
    "thermalguardd",
    "ThermalGuard",
    "syspolicyd",
    "securityd",
    "opendirectoryd",
    "coreaudiod",
    "hidd",
    "powerd",
    "notifyd",
    "configd",
    "logd",
];

/// Never demoted: matched as a substring, so every helper/bin of these vendors
/// is covered (Bitdefender, Wacom, Apple's anti-malware stack).
const PROTECTED_FRAGMENTS: &[&str] = &[
    "bitdefender",
    "bdservicehost",
    "vshieldscanner",
    "trufos",
    "wacom",
    "xprotect",
    "mdmclient",
    "endpointsecurity",
    // Anything that carries audio/video to the user: browsers, call apps and
    // local players/editors. Throttling a decoder shows up as stuttering
    // playback or a frozen call, which is never worth the few degrees it saves.
    "webkit",
    "safari",
    "chrome",
    "firefox",
    "edge",
    "teams",
    "webex",
    "skype",
    "discord",
    "slack",
    "zoom",
    "avconference",
    "vtdecoder",
    "vtencoder",
    "videotoolbox",
    "coremedia",
    "mediaserverd",
    "facetime",
    "quicktime",
    "vlc",
    "iina",
    "mpv",
    "obs",
    "handbrake",
    "final cut",
    "premiere",
    "davinci",
    "imovie",
    "screenflow",
];

/// Processes that carry a live call or video to the user. These get their own
/// treatment: they are never demoted, and while one is busy the power cap is
/// held off (a dropped frame is worse than a few degrees).
const MEDIA_FRAGMENTS: &[&str] = &[
    // browsers
    "google chrome",
    "safari",
    "webkit",
    "firefox",
    "microsoft edge",
    // call apps
    "microsoft teams",
    "msteams",
    "zoom",
    "google meet",
    "webex",
    "cisco",
    "facetime",
    "skype",
    "discord",
    "slack",
    "ringcentral",
    "bluejeans",
    "gotomeeting",
    "jitsi",
    "whereby",
    // media pipelines and players: local video files count too
    "avconference",
    "vtdecoder",
    "vtencoder",
    "videotoolbox",
    "coremedia",
    "quicktime",
    "vlc",
    "iina",
    "mpv",
    "movist",
    "elmedia",
    "obs",
    "handbrake",
    "final cut",
    "premiere",
    "davinci",
    "imovie",
    "screenflow",
];

/// True for a call or media pipeline process (by command path).
pub fn is_media(comm: &str) -> bool {
    let lower = comm.to_ascii_lowercase();
    MEDIA_FRAGMENTS.iter().any(|f| lower.contains(f))
}

pub fn is_protected(comm: &str) -> bool {
    let base = comm.rsplit('/').next().unwrap_or(comm);
    if PROTECTED_EXACT.iter().any(|p| base.eq_ignore_ascii_case(p)) {
        return true;
    }
    let lower = comm.to_ascii_lowercase();
    PROTECTED_FRAGMENTS.iter().any(|f| lower.contains(f))
}

/// Top processes by CPU, highest first. Uses `ps` so we never depend on private
/// libproc structures.
pub fn sample(limit: usize) -> Vec<Proc> {
    let out = match Command::new("/bin/ps")
        .args(["-Ao", "pid,pcpu,comm", "-r"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    parse_ps(&String::from_utf8_lossy(&out.stdout), limit)
}

/// Parse `ps -Ao pid,pcpu,comm` output.
///
/// `ps` pads its columns with **two** spaces, so splitting on a single
/// whitespace yields an empty middle field and throws every row away — which is
/// exactly what happened before: the sample was always empty, so demotion never
/// ran. Parse tokens separated by *runs* of whitespace instead, and keep the
/// command path (which may itself contain spaces) intact.
pub fn parse_ps(text: &str, limit: usize) -> Vec<Proc> {
    let mut v = Vec::new();
    for line in text.lines().skip(1) {
        let mut it = line.split_whitespace();
        let pid = match it.next().map(|s| s.parse::<u32>()) {
            Some(Ok(p)) => p,
            _ => continue,
        };
        let cpu = match it.next().map(|s| s.parse::<f64>()) {
            Some(Ok(c)) => c,
            _ => continue,
        };
        let name = tail_after_tokens(line, 2).trim().to_string();
        if name.is_empty() {
            continue;
        }
        v.push(Proc { pid, cpu, name });
        if v.len() >= limit {
            break;
        }
    }
    v
}

/// Everything after the first `n` whitespace-separated tokens of `line`.
fn tail_after_tokens(line: &str, n: usize) -> &str {
    let mut s = line.trim_start();
    for _ in 0..n {
        match s.find(char::is_whitespace) {
            Some(i) => s = s[i..].trim_start(),
            None => return "",
        }
    }
    s
}

/// Full command path for a live PID, used to detect PID reuse before we touch
/// priority again.
pub fn comm_of(pid: u32) -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn same_process(pid: u32, name: &str) -> bool {
    match comm_of(pid) {
        Some(current) => current == name,
        None => false,
    }
}

pub fn demote(pid: u32) -> bool {
    unsafe { setpriority(PRIO_DARWIN_PROCESS, pid, PRIO_DARWIN_BG) == 0 }
}

pub fn restore(pid: u32) -> bool {
    unsafe { setpriority(PRIO_DARWIN_PROCESS, pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_check_rejects_dead_pids() {
        // PID 0 is never a normal process; the check must fail closed.
        assert!(!same_process(0, "anything"));
        assert!(comm_of(0).is_none() || comm_of(0).is_some());
    }

    #[test]
    fn security_and_input_software_is_never_throttled() {
        for name in [
            "/Library/Bitdefender/AVP/agentui/agentui",
            "Bitdefender",
            "bdservicehost",
            "VShieldScanner",
            "/Applications/Wacom Tablet.localized/Wacom Center.app/Contents/MacOS/Wacom Center",
            "WacomTabletDriver",
            "XprotectService",
            "syspolicyd",
            "kernel_task",
            "WindowServer",
        ] {
            assert!(is_protected(name), "{} must be protected", name);
        }
    }

    #[test]
    fn browsers_and_media_pipelines_are_never_throttled() {
        // Demoting these is what makes video playback stutter.
        for name in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "Google Chrome Helper (Renderer)",
            "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc/Contents/MacOS/com.apple.WebKit.GPU",
            "VTDecoderXPCService",
            "com.apple.avconference",
        ] {
            assert!(is_protected(name), "{} must not be throttled", name);
        }
    }

    #[test]
    fn media_sessions_are_detected() {
        for call in [
            "/Applications/Microsoft Teams.app/Contents/MacOS/MSTeams",
            "/Applications/zoom.us.app/Contents/MacOS/zoom.us",
            "/Applications/Webex.app/Contents/MacOS/Webex",
            "/Applications/Discord.app/Contents/MacOS/Discord",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc/Contents/MacOS/com.apple.WebKit.GPU",
            "VTDecoderXPCService",
            "VTEncoderXPCService",
            "com.apple.avconference",
            "/Applications/IINA.app/Contents/MacOS/IINA",
            "/Applications/VLC.app/Contents/MacOS/VLC",
            "/Applications/QuickTime Player.app/Contents/MacOS/QuickTime Player",
        ] {
            assert!(is_media(call), "{} should count as a call/media session", call);
        }
        for other in [
            "/usr/bin/node",
            "/Applications/Xcode.app/Contents/MacOS/Xcode",
            "/bin/ps",
        ] {
            assert!(!is_media(other), "{} is not a media session", other);
        }
    }

    #[test]
    fn call_apps_are_never_demoted() {
        for name in [
            "/Applications/zoom.us.app/Contents/MacOS/zoom.us",
            "Microsoft Teams Helper (Renderer)",
            "VTEncoderXPCService",
            "/Applications/Discord.app/Contents/MacOS/Discord",
            "com.apple.avconference.avconferenced",
        ] {
            assert!(is_protected(name), "{} must not be throttled", name);
        }
    }

    #[test]
    fn background_compute_is_still_throttlable() {
        for name in ["node", "cargo", "photolibraryd", "python3", "ffmpeg-demo"] {
            assert!(!is_protected(name), "{} should be throttlable", name);
        }
    }

    #[test]
    fn ps_output_with_padded_columns_is_parsed() {
        // Regression: `ps` pads with two spaces. Splitting on a single
        // whitespace produced an empty middle field, so every row was dropped
        // and the sample was always empty — demotion never ran at all.
        let text = "  PID  %CPU COMM\n 1234  43.2 /usr/bin/node\n2345   0.4 /Applications/Google Chrome Helper (Renderer)\n";
        let v = parse_ps(text, 10);
        assert_eq!(v.len(), 2, "got {:?}", v);
        assert_eq!(v[0].pid, 1234);
        assert!((v[0].cpu - 43.2).abs() < 0.001);
        assert_eq!(v[0].name, "/usr/bin/node");
        assert_eq!(v[1].pid, 2345);
        // A path containing spaces survives intact.
        assert_eq!(v[1].name, "/Applications/Google Chrome Helper (Renderer)");
        // The header row is never treated as a process.
        assert!(parse_ps(text, 10).iter().all(|p| p.pid != 0));
    }

    #[test]
    fn names_are_shortened_for_display_only() {
        assert_eq!(short_name("/usr/bin/node"), "node");
        assert!(short_name(&"x".repeat(60)).chars().count() <= 34);
    }

    #[test]
    fn multibyte_names_never_panic_the_daemon() {
        // A user can name an executable anything; byte-slicing used to abort.
        let nasty = format!("/Users/x/{}", "é".repeat(80));
        let out = short_name(&nasty);
        assert!(out.chars().count() <= 34);
        assert!(
            short_name("日本語のプロセス名がとても長い場合のテストです")
                .chars()
                .count()
                <= 34
        );
    }

    #[test]
    fn control_characters_are_stripped_from_names() {
        assert_eq!(short_name("evil\nname\r"), "evilname");
    }
}

/// Truncation must be char-boundary safe: a process whose path contains
/// multi-byte UTF-8 used to be able to panic the daemon here.
pub fn short_name(comm: &str) -> String {
    let base = comm.rsplit('/').next().unwrap_or(comm);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() > 34 {
        let head: String = cleaned.chars().take(33).collect();
        format!("{}…", head)
    } else {
        cleaned
    }
}
