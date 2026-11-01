//! User-controlled switches for macOS' own background services.
//!
//! Security notes (see SECURITY.md):
//!   * only the curated allowlist below can be toggled; arbitrary labels are
//!     rejected, so a peer cannot disable unrelated Apple services
//!   * `auto_safe` is the *only* subset the Auto-mode trimmer may touch; the
//!     list is compiled into the daemon, never supplied over IPC
//!   * every child process is invoked by absolute path (no PATH hijack); the
//!     test-only override is ignored when the daemon runs as root
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

/// `/bin/launchctl` in production. The override exists only for the integration
/// test suite and is refused whenever the process is root (see
/// `overrides_allowed()`), so a planted environment variable can never redirect
/// the installed daemon.
fn launchctl() -> String {
    if crate::overrides_allowed() {
        if let Ok(v) = std::env::var("THERMALGUARD_LAUNCHCTL") {
            return v;
        }
    }
    "/bin/launchctl".to_string()
}

/// One curated macOS background service.
pub struct ServiceDef {
    pub id: &'static str,
    pub desc: &'static str,
    /// Background-only work that is safe to trim automatically while the user
    /// has opted in and the cooling mode is Auto. Never includes security,
    /// MDM, update, backup, iCloud-sync or user-facing communication services.
    pub auto_safe: bool,
}

/// Curated: the Apple services that actually move the CPU needle on a 10 W Mac,
/// with an honest one-line description each. `auto_safe` services are the
/// subset the Auto-mode trimmer may disable on the user's behalf.
const SERVICES: &[ServiceDef] = &[
    // ---- Auto-safe: on-device ML, indexing, Siri/Apple Intelligence and
    // ---- telemetry. All are background-only and reversible.
    ServiceDef {
        id: "com.apple.photoanalysisd",
        desc: "Photos analysis — scans the library in the background; the single heaviest Apple background job.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.mediaanalysisd",
        desc: "Media analysis — on-device ML over images and video.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.photolibraryd",
        desc: "Photos library — background maintenance. Photos itself still opens.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.siriknowledged",
        desc: "Siri knowledge — builds the on-device suggestion index.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.assistant_cdmd",
        desc: "Siri language understanding — NLU work for Siri and Apple Intelligence.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.siriinferenced",
        desc: "Siri inference — Apple Intelligence inference work.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.SiriTTSTrainingAgent",
        desc: "Siri voice training — heavy on-device training job.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.parsecd",
        desc: "Siri/Safari parsing — background text parsing.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.tipsd",
        desc: "Tips — background refresh for the Tips app.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.quicklook.ThumbnailsAgent",
        desc: "Quick Look thumbnails — generates previews in the background.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.knowledgeconstructiond",
        desc: "Knowledge graph — analyses on-device content to build a knowledge graph.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.knowledge-agent",
        desc: "Knowledge agent — background knowledge extraction.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.spotlightknowledged",
        desc: "Spotlight knowledge — enriches Spotlight results in the background.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.spotlightknowledged.importer",
        desc: "Spotlight knowledge importer — background import work.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.spotlightknowledged.updater",
        desc: "Spotlight knowledge updater — background index updates.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.proactiveeventtrackerd",
        desc: "Proactive event tracking — learns from app usage in the background.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.proactived",
        desc: "Proactive suggestions — on-device prediction work.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.analyticsagent",
        desc: "Analytics — background usage analytics.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.inputanalyticsd",
        desc: "Input analytics — diagnostics over keyboard and pointer usage.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.geoanalyticsd",
        desc: "Location analytics — background geolocation analytics.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.ap.adprivacyd",
        desc: "Ad privacy — background advertising-privacy services.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.ap.promotedcontentd",
        desc: "Promoted content — background ad-content services.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.duetexpertd",
        desc: "Personalised experiences — background on-device learning.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.naturallanguaged",
        desc: "Natural language — on-device language analysis.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.generativeexperiencesd",
        desc: "Apple Intelligence generative experiences — background model work.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.intelligenceflowd",
        desc: "Apple Intelligence flow — background orchestration.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.intelligencecontextd",
        desc: "Apple Intelligence context — background context building.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.intelligenceplatformd",
        desc: "Apple Intelligence platform — background platform services.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.suggestd",
        desc: "Suggestions — background suggestion indexing.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.UsageTrackingAgent",
        desc: "Usage tracking — background app-usage accounting.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.triald",
        desc: "Trial/experiment daemon — background experiment bookkeeping.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.BiomeAgent",
        desc: "Biome agent — background on-device behaviour graph work.",
        auto_safe: true,
    },
    ServiceDef {
        id: "com.apple.biomesyncd",
        desc: "Biome sync — synchronises behaviour data. Manual only: sync service.",
        auto_safe: false,
    },
    // ---- Manual only: user-facing sync, media, search, screen time and
    // ---- legacy labels. These are never touched by the Auto trimmer.
    ServiceDef {
        id: "com.apple.Siri",
        desc: "Legacy Siri label — kept for manual toggling only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.Siri.agent",
        desc: "Siri agent — interactive assistant. Manual only: user-facing.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.assistant_service",
        desc: "Siri service — interactive assistant plumbing. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.assistantd",
        desc: "Siri daemon — Siri is unavailable while this is disabled. Manual only: user-facing.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.siriactionsd",
        desc: "Siri actions — resolves interactive Siri actions. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.sirittsd",
        desc: "Siri speech — text-to-speech for Siri. Manual only: user-facing.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.corespeechd",
        desc: "Siri speech recognition — dictation and Siri voice. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.bird",
        desc: "iCloud Drive — uploads and syncs files. Manual only: your data.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.cloudd",
        desc: "iCloud sync — account and key-value syncing. Manual only: your data.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.cloudphotod",
        desc: "iCloud Photos — syncs the photo library. Manual only: your data.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.cloudpaird",
        desc: "Continuity pairing — pairs Apple devices. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.icloud.searchpartyuseragent",
        desc: "Find My network — locating and reporting. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.rapportd",
        desc: "Handoff/Continuity — nearby-device features. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.sharingd",
        desc: "AirDrop/Handoff sharing — manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.avconferenced",
        desc: "Video conferencing — calls. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.mediaremoteagent",
        desc: "Media remote — Now Playing controls. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.mediastream.mstreamd",
        desc: "Media streaming — AirPlay/media streams. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.itunescloudd",
        desc: "Apple Music sync — manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.parsec-fbf",
        desc: "Safari/Spotlight parsing — manual only; search suggestions use it.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.corespotlightd",
        desc: "Spotlight core — manual only; use the Spotlight switch instead.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.Spotlight",
        desc: "Spotlight UI — manual only; use the Spotlight switch instead.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.managedcorespotlightd",
        desc: "Managed Spotlight — MDM-controlled; manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.ScreenTimeAgent",
        desc: "Screen Time — parental-control policy. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.familycontrols.useragent",
        desc: "Family controls — parental-control policy. Manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.familynotificationd",
        desc: "Family notifications — manual only.",
        auto_safe: false,
    },
    ServiceDef {
        id: "com.apple.familycircled",
        desc: "Family circle — manual only.",
        auto_safe: false,
    },
];

#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub desc: String,
    pub kind: String, // "service" | "spotlight" | "powernap"
    pub enabled: bool,
    pub auto_safe: bool,
}

pub fn is_known(id: &str) -> bool {
    id == "spotlight" || id == "powernap" || SERVICES.iter().any(|s| s.id == id)
}

/// The only services the Auto-mode trimmer may ever disable.
pub fn is_auto_safe(id: &str) -> bool {
    SERVICES.iter().any(|s| s.id == id && s.auto_safe)
}

pub fn auto_safe_ids() -> Vec<&'static str> {
    SERVICES
        .iter()
        .filter(|s| s.auto_safe)
        .map(|s| s.id)
        .collect()
}

/// `None` means launchd's disabled-state could not be read, so callers must not
/// guess. An empty `Some` is a valid answer: nothing is explicitly disabled.
fn disabled_set_raw() -> Option<Vec<String>> {
    let out = run(
        &launchctl(),
        &["print-disabled", &format!("gui/{}", system::console_uid())],
    );
    if out.is_empty() {
        return None;
    }
    Some(
        out.lines()
            .filter(|l| l.contains("=> disabled"))
            .filter_map(|l| l.split('"').nth(1).map(|s| s.to_string()))
            .collect(),
    )
}

fn disabled_set() -> Vec<String> {
    disabled_set_raw().unwrap_or_default()
}

/// Current enabled/disabled state for every curated service. Used by the
/// Auto-mode trimmer; returns `None` rather than a made-up state when launchd
/// cannot be read.
pub fn service_states() -> Option<Vec<(String, bool)>> {
    let disabled = disabled_set_raw()?;
    Some(
        SERVICES
            .iter()
            .map(|s| {
                (
                    s.id.to_string(),
                    !disabled.iter().any(|d| d == s.id),
                )
            })
            .collect(),
    )
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
            auto_safe: false,
        },
        Item {
            id: "powernap".into(),
            desc: "Power Nap — keeps doing background work while the Mac sleeps/charges, which is pure heat while plugged in.".into(),
            kind: "powernap".into(),
            enabled: power_nap_enabled(),
            auto_safe: false,
        },
    ];
    for def in SERVICES {
        items.push(Item {
            id: def.id.to_string(),
            desc: def.desc.to_string(),
            kind: "service".into(),
            enabled: !disabled.iter().any(|d| d == def.id),
            auto_safe: def.auto_safe,
        });
    }
    // Auto-trimmable rows first, then the rest alphabetically. The sheet is
    // long by design; the ordering makes the safe subset obvious.
    items.sort_by(|a, b| {
        b.auto_safe
            .cmp(&a.auto_safe)
            .then_with(|| a.id.to_lowercase().cmp(&b.id.to_lowercase()))
    });
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
            let lc = launchctl();
            let out = if enable {
                run(&lc, &["enable", &format!("{}/{}", domain, label)])
            } else {
                run(&lc, &["disable", &format!("{}/{}", domain, label)])
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

pub fn items_json(
    items: &[Item],
    note: &str,
    auto_trim: bool,
    auto_trim_active: bool,
    auto_trimmed: &[String],
    auto_trim_note: &str,
) -> String {
    let rows: Vec<String> = items
        .iter()
        .map(|i| {
            let trimmed = auto_trimmed.iter().any(|id| id == &i.id);
            json::obj(&[
                json::s("id", &i.id),
                json::s("desc", &i.desc),
                json::s("kind", &i.kind),
                json::b("enabled", i.enabled),
                json::b("auto_safe", i.auto_safe),
                json::b("auto_managed", auto_trim_active && i.auto_safe),
                json::b("auto_trimmed", trimmed),
            ])
        })
        .collect();
    json::obj(&[
        format!("\"items\":[{}]", rows.join(",")),
        json::s("note", note),
        json::b("auto_trim", auto_trim),
        json::b("auto_trim_active", auto_trim_active),
        json::s("auto_trim_note", auto_trim_note),
        json::str_array("auto_trimmed", auto_trimmed),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_only_curated_ids() {
        assert!(is_known("spotlight"));
        assert!(is_known("powernap"));
        assert!(is_known("com.apple.photoanalysisd"));
        assert!(is_known("com.apple.knowledgeconstructiond"));
    }

    #[test]
    fn arbitrary_apple_labels_are_rejected() {
        for bad in [
            "com.apple.WindowServer",
            "com.apple.securityd",
            "com.apple.syspolicyd",
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

    #[test]
    fn auto_safe_ids_are_unique_and_known() {
        let ids = auto_safe_ids();
        assert!(!ids.is_empty(), "auto-trim must have a curated catalog");
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(*id), "duplicate auto-safe id {}", id);
            assert!(is_known(id), "auto-safe id {} is not in the catalog", id);
        }
    }

    #[test]
    fn auto_safe_catalog_covers_the_heavy_sequoia_work() {
        for id in [
            "com.apple.photoanalysisd",
            "com.apple.mediaanalysisd",
            "com.apple.siriknowledged",
            "com.apple.knowledgeconstructiond",
            "com.apple.spotlightknowledged",
            "com.apple.proactiveeventtrackerd",
            "com.apple.analyticsagent",
            "com.apple.intelligenceflowd",
            "com.apple.siriinferenced",
            "com.apple.tipsd",
            "com.apple.quicklook.ThumbnailsAgent",
        ] {
            assert!(is_auto_safe(id), "{} should be auto-trimmable", id);
        }
    }

    #[test]
    fn protected_and_user_facing_services_are_never_auto_safe() {
        // Security, MDM, updates, backup, iCloud sync, communications and
        // accessibility are deliberately manual-only or absent entirely.
        for id in [
            "com.apple.WindowServer",
            "com.apple.securityd",
            "com.apple.syspolicyd",
            "com.apple.XProtect",
            "com.apple.backupd",
            "com.apple.softwareupdated",
            "com.apple.ManagedClientAgent.enrollagent",
            "com.apple.mdmclient.daemon",
            "com.apple.ScreenTimeAgent",
            "com.apple.familycontrols.useragent",
            "com.apple.icloud.searchpartyuseragent",
            "com.apple.rapportd",
            "com.apple.cloudd",
            "com.apple.bird",
            "com.apple.cloudphotod",
            "com.apple.biomesyncd",
            "com.apple.avconferenced",
            "com.apple.security.cloudkeychainproxy3",
            "com.apple.protectedcloudstorage.protectedcloudkeysyncing",
            "com.apple.universalaccessd",
            "com.apple.Siri",
            "com.apple.Siri.agent",
            "com.apple.assistant_service",
            "com.apple.assistantd",
            "com.apple.siriactionsd",
            "com.apple.sirittsd",
            "com.apple.corespeechd",
        ] {
            assert!(!is_auto_safe(id), "{} must never be auto-trimmed", id);
        }
    }

    #[test]
    fn manual_only_services_are_still_known_and_toggleable() {
        for id in [
            "com.apple.bird",
            "com.apple.cloudd",
            "com.apple.cloudphotod",
            "com.apple.avconferenced",
            "com.apple.corespotlightd",
            "com.apple.ScreenTimeAgent",
        ] {
            assert!(is_known(id), "{} should remain manually toggleable", id);
            assert!(!is_auto_safe(id), "{} must stay out of auto-trim", id);
        }
    }

    #[test]
    fn service_states_are_derived_from_the_same_catalog() {
        let ids: Vec<&str> = SERVICES.iter().map(|s| s.id).collect();
        assert!(ids.contains(&"com.apple.photoanalysisd"));
        assert!(ids.contains(&"com.apple.bird"));
        assert!(!ids.contains(&"spotlight"));
        assert!(!ids.contains(&"powernap"));
    }
}
