//! Discovery and user-controlled enable/disable of third-party background items.
//!
//! Security notes (see SECURITY.md):
//!   * the caller never supplies a filesystem path — the label is resolved
//!     against a fresh scan, so a compromised peer cannot bootstrap an
//!     arbitrary plist as root
//!   * only launchd directories are accepted, and Apple/ThermalGuard labels are
//!     refused at this layer regardless of who asks

use crate::json;
use crate::system;
use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub path: String,
    pub scope: String, // "system" | "gui"
    pub loaded: bool,
}

/// A path we are willing to hand to `launchctl bootstrap` as root.
pub fn path_allowed(p: &str) -> bool {
    if !p.ends_with(".plist") || p.contains("..") {
        return false;
    }
    if p.starts_with("/Library/LaunchDaemons/") || p.starts_with("/Library/LaunchAgents/") {
        return true;
    }
    p.starts_with("/Users/") && p.contains("/Library/LaunchAgents/")
}

fn label_of(plist_path: &str) -> Option<String> {
    let label = run("/usr/bin/plutil", &["-extract", "Label", "raw", "-o", "-", plist_path]);
    if !label.is_empty() && !label.contains("Could not") {
        return Some(label);
    }
    let file = plist_path.rsplit('/').next()?;
    Some(file.trim_end_matches(".plist").to_string())
}

fn is_loaded(scope: &str, label: &str) -> bool {
    let domain = domain_for(scope);
    let out = run("/bin/launchctl", &["print", &format!("{}/{}", domain, label)]);
    !out.is_empty() && !out.contains("Could not find service")
}

fn domain_for(scope: &str) -> String {
    if scope == "system" {
        "system".to_string()
    } else {
        format!("gui/{}", system::console_uid())
    }
}

pub fn scan() -> Vec<Item> {
    let user = system::console_user();
    let mut dirs: Vec<(&str, String)> = vec![
        ("system", "/Library/LaunchDaemons".to_string()),
        ("gui", "/Library/LaunchAgents".to_string()),
    ];
    if !user.is_empty() && user != "root" && !user.contains('/') {
        dirs.push(("gui", format!("/Users/{}/Library/LaunchAgents", user)));
    }

    let mut items = Vec::new();
    for (scope, dir) in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.ends_with(".plist") || name.starts_with("com.apple.") {
                continue;
            }
            let p = path.to_string_lossy().to_string();
            if !path_allowed(&p) {
                continue;
            }
            let label = match label_of(&p) {
                Some(l) => l,
                None => continue,
            };
            if label.starts_with("com.apple.") || label == "com.thermalguard.daemon" {
                continue;
            }
            let loaded = is_loaded(scope, &label);
            items.push(Item { label, path: p, scope: scope.to_string(), loaded });
        }
    }
    items.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    items
}

/// Enable/disable by label only. The path and scope are re-derived internally,
/// so a caller cannot redirect this to an arbitrary plist.
pub fn set_enabled(label: &str, enable: bool) -> Result<(), String> {
    if label.is_empty() || label.len() > 200 || label.contains(char::is_control) {
        return Err("invalid label".into());
    }
    if label.starts_with("com.apple.") || label == "com.thermalguard.daemon" {
        return Err("refused: protected service".into());
    }

    let items = scan();
    let item = items
        .iter()
        .find(|i| i.label == label)
        .ok_or_else(|| format!("unknown background item: {}", label))?;

    if !path_allowed(&item.path) {
        return Err("refused: path outside launchd directories".into());
    }

    let domain = domain_for(&item.scope);
    let out = if enable {
        run("/bin/launchctl", &["bootstrap", &domain, &item.path])
    } else {
        run("/bin/launchctl", &["bootout", &format!("{}/{}", domain, label)])
    };
    if out.to_lowercase().contains("error") || out.contains("failed") {
        return Err(out);
    }
    Ok(())
}

pub fn items_json(items: &[Item]) -> String {
    let rows: Vec<String> = items
        .iter()
        .map(|i| {
            json::obj(&[
                json::s("label", &i.label),
                json::s("path", &i.path),
                json::s("scope", &i.scope),
                json::b("loaded", i.loaded),
            ])
        })
        .collect();
    format!("{{\"items\":[{}]}}", rows.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_launchd_locations_are_bootstrappable() {
        assert!(path_allowed("/Library/LaunchDaemons/com.wacom.UpdateHelper.plist"));
        assert!(path_allowed("/Library/LaunchAgents/com.wacom.IOManager.plist"));
        assert!(path_allowed("/Users/admin/Library/LaunchAgents/x.plist"));
    }

    #[test]
    fn attacker_controlled_paths_are_rejected() {
        assert!(!path_allowed("/tmp/evil.plist"));
        assert!(!path_allowed("/Library/LaunchDaemons/../../tmp/evil.plist"));
        assert!(!path_allowed("/Library/LaunchDaemons/evil.sh"));
        assert!(!path_allowed("/etc/passwd"));
        assert!(!path_allowed(""));
    }
}
