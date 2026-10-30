//! User-controlled switches for macOS' own background services.
//!
//! Security notes (see SECURITY.md):
//!   * only the curated allowlist below can be toggled; arbitrary labels are
//!     rejected, so a peer cannot disable unrelated Apple services
//!   * every child process is invoked by absolute path (no PATH hijack)
//!   * all changes are reversible via launchd's own `disable`/`enable` state

use crate::json;
use crate::system;
use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

/// Curated: the Apple services that actually move the CPU needle on a 10 W Mac,
/// with an honest one-line description each.
const SERVICES: &[(&str, &str)] = &[
    ("com.apple.photoanalysisd", "Photos analysis — scans your whole library in the background; the single heaviest Apple background job."),
    ("com.apple.mediaanalysisd", "Media analysis — on-device ML over images and video."),
    ("com.apple.photolibraryd", "Photos library — maintenance and syncing of the library."),
    ("com.apple.siriknowledged", "Siri knowledge — builds the on-device suggestion index."),
    ("com.apple.Siri", "Siri agent — background assistant."),
    ("com.apple.assistant_service", "Siri service — assistant plumbing."),
    ("com.apple.parsecd", "Siri/Safari parsing — background text parsing."),
    ("com.apple.bird", "iCloud Drive — uploads and syncs files."),
    ("com.apple.cloudd", "iCloud sync — account and key-value syncing."),
    ("com.apple.tipsd", "Tips — background refresh for the Tips app."),
    ("com.apple.quicklook.ThumbnailsAgent", "Quick Look thumbnails — generates previews in the background."),
];

#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub desc: String,
    pub kind: String, // "service" | "spotlight" | "powernap"
    pub enabled: bool,
}

pub fn is_known(id: &str) -> bool {
    id == "spotlight" || id == "powernap" || SERVICES.iter().any(|(l, _)| *l == id)
}

fn disabled_set() -> Vec<String> {
    let out = run(
        "/bin/launchctl",
        &["print-disabled", &format!("gui/{}", system::console_uid())],
    );
    out.lines()
        .filter(|l| l.contains("=> disabled"))
        .filter_map(|l| l.split('"').nth(1).map(|s| s.to_string()))
        .collect()
}

fn spotlight_enabled() -> bool {
    !run("/usr/bin/mdutil", &["-s", "/"]).contains("Indexing disabled")
}

fn power_nap_enabled() -> bool {
    run("/usr/bin/pmset", &["-g", "custom"]).lines().any(|l| {
        let t = l.trim();
        t.starts_with("powernap") && t.split_whitespace().nth(1) == Some("1")
    })
}

pub fn scan() -> Vec<Item> {
    let disabled = disabled_set();
    let mut items = vec![
        Item {
            id: "spotlight".into(),
            desc: "Spotlight indexing — re-indexes drives in the background (mds/mds_stores). Turning this off is the biggest single win on this chip.".into(),
            kind: "spotlight".into(),
            enabled: spotlight_enabled(),
        },
        Item {
            id: "powernap".into(),
            desc: "Power Nap — keeps doing background work while the Mac sleeps/charges, which is pure heat while plugged in.".into(),
            kind: "powernap".into(),
            enabled: power_nap_enabled(),
        },
    ];
    for (label, desc) in SERVICES {
        items.push(Item {
            id: (*label).to_string(),
            desc: (*desc).to_string(),
            kind: "service".into(),
            enabled: !disabled.iter().any(|d| d == label),
        });
    }
    items
}

pub fn set(id: &str, enable: bool) -> Result<String, String> {
    if !is_known(id) {
        return Err(format!("not a curated macOS service: {}", id));
    }
    match id {
        "spotlight" => {
            let out = run("/usr/bin/mdutil", &["-a", "-i", if enable { "on" } else { "off" }]);
            if out.contains("Error") || out.contains("failed") {
                return Err(out);
            }
            Ok(format!("Spotlight indexing {}", if enable { "on" } else { "off" }))
        }
        "powernap" => {
            let out = run("/usr/bin/pmset", &["-a", "powernap", if enable { "1" } else { "0" }]);
            if out.to_lowercase().contains("error") {
                return Err(out);
            }
            Ok(format!("Power Nap {}", if enable { "on" } else { "off" }))
        }
        label => {
            let domain = format!("gui/{}", system::console_uid());
            let out = if enable {
                run("/bin/launchctl", &["enable", &format!("{}/{}", domain, label)])
            } else {
                run("/bin/launchctl", &["disable", &format!("{}/{}", domain, label)])
            };
            // Re-read the real state: SIP-protected services silently refuse.
            std::thread::sleep(std::time::Duration::from_millis(150));
            if disabled_set().iter().any(|d| d == label) == enable {
                return Err(format!("{} refused (SIP-protected). {}", label, out.trim()));
            }
            Ok(format!("{} {}", label, if enable { "enabled" } else { "disabled" }))
        }
    }
}

pub fn items_json(items: &[Item], note: &str) -> String {
    let rows: Vec<String> = items
        .iter()
        .map(|i| {
            json::obj(&[
                json::s("id", &i.id),
                json::s("desc", &i.desc),
                json::s("kind", &i.kind),
                json::b("enabled", i.enabled),
            ])
        })
        .collect();
    format!(
        "{{\"items\":[{}],\"note\":\"{}\"}}",
        rows.join(","),
        json::esc(note)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_only_curated_ids() {
        assert!(is_known("spotlight"));
        assert!(is_known("powernap"));
        assert!(is_known("com.apple.photoanalysisd"));
    }

    #[test]
    fn arbitrary_apple_labels_are_rejected() {
        for bad in [
            "com.apple.WindowServer",
            "com.apple.securityd",
            "com.apple.Safari",
            "com.apple.",
            "",
            "../../bin/sh",
            "com.apple.photoanalysisd ",
        ] {
            assert!(!is_known(bad), "{} must be rejected", bad);
        }
    }

    #[test]
    fn set_refuses_unknown_ids_before_running_anything() {
        assert!(set("com.apple.WindowServer", false).is_err());
        assert!(set("rm -rf /", false).is_err());
    }
}
