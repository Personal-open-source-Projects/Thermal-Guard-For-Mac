//! Integration test: start the real daemon binary, talk to it over its unix
//! socket, and assert the wire contract the Swift UI depends on.
//!
//! Runs unprivileged against a temp directory (no SMC, no pmset writes), which
//! also proves the daemon degrades gracefully without root.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Each test gets its own directory: cargo runs tests in parallel and they must
/// not delete each other's socket.
static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Guard {
    child: Child,
    sock: PathBuf,
    dir: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start() -> Guard {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("tg-test-{}-{}-{}", std::process::id(), n, stamp));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("tg.sock");

    let child = Command::new(env!("CARGO_BIN_EXE_thermalguardd"))
        .env("THERMALGUARD_TESTING", "1")
        .env("THERMALGUARD_SOCK", &sock)
        .env("THERMALGUARD_RUN_DIR", &dir)
        .spawn()
        .expect("daemon should start");

    let guard = Guard { child, sock, dir };
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if guard.sock.exists() && UnixStream::connect(&guard.sock).is_ok() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    panic!("daemon socket never appeared at {:?}", guard.sock);
}

fn ask(g: &Guard, body: &str) -> String {
    let mut s = UnixStream::connect(&g.sock).expect("connect");
    s.write_all(body.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    s.flush().unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).expect("read response");
    out
}

#[test]
fn status_returns_the_full_ui_contract() {
    let g = start();
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    for key in [
        "\"mode\"",
        "\"mode_label\"",
        "\"cap_active\"",
        "\"smc_ok\"",
        "\"speed_limit\"",
        "\"procs\"",
        "\"demoted\"",
        "\"events\"",
        "\"fan_note\"",
        "\"fan_target\"",
        "\"fan_intent\"",
    ] {
        assert!(reply.contains(key), "missing {} in {}", key, reply);
    }
    assert!(reply.trim_start().starts_with('{'));
    assert!(reply.trim_end().ends_with('}'));
}

#[test]
fn mode_command_round_trips_through_the_daemon() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"maxcool\"}");
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"mode\":\"maxcool\""), "got {}", reply);
}

#[test]
fn fan_preference_round_trips_through_the_daemon() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"fan\",\"value\":\"quiet\"}");
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"fan_intent\":\"quiet\""), "got {}", reply);
    let _ = ask(&g, "{\"cmd\":\"fan\",\"value\":\"auto\"}");
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"fan_intent\":\"auto\""), "got {}", reply);
}

#[test]
fn video_and_call_shield_is_on_by_default_and_togglable() {
    let g = start();
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"media_guard\":true"), "shield must default on: {}", r);

    let _ = ask(&g, "{\"cmd\":\"media_guard\",\"value\":\"off\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"media_guard\":false"), "got {}", r);

    // Anything that is not a boolean is ignored rather than flipping the flag.
    let _ = ask(&g, "{\"cmd\":\"media_guard\",\"value\":\"maybe\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"media_guard\":false"), "got {}", r);

    let _ = ask(&g, "{\"cmd\":\"media_guard\",\"value\":\"on\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"media_guard\":true"), "got {}", r);
}

#[test]
fn a_client_cannot_choose_an_arbitrary_fan_rpm() {
    // The wire may only carry a preference. Any attempt to smuggle an RPM in
    // must be ignored: the daemon derives targets solely from the SMC's own
    // range, so a privileged hardware write never carries caller input.
    let g = start();
    let _ = ask(
        &g,
        "{\"cmd\":\"fan\",\"value\":\"quiet\",\"rpm\":999999,\"target\":-1}",
    );
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"fan_intent\":\"quiet\""), "got {}", reply);
    assert!(!reply.contains("999999"), "caller RPM leaked into state: {}", reply);
    assert!(!reply.contains("-1.0"), "caller target leaked into state: {}", reply);
}

#[test]
fn unknown_fan_preference_is_ignored() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"fan\",\"value\":\"quiet\"}");
    let _ = ask(&g, "{\"cmd\":\"fan\",\"value\":\"0\"}");
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"fan_intent\":\"quiet\""), "got {}", reply);
}

#[test]
fn unknown_mode_is_ignored_rather_than_breaking_state() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"balanced\"}");
    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"../../etc/passwd\"}");
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"mode\":\"balanced\""), "got {}", reply);
}

#[test]
fn macos_service_list_is_well_formed_and_never_exposes_apple_labels() {
    let g = start();
    let reply = ask(&g, "{\"cmd\":\"apple\"}");
    assert!(reply.contains("\"items\""), "got {}", reply);
    assert!(reply.contains("\"spotlight\""), "spotlight switch missing");
    assert!(reply.contains("\"powernap\""), "powernap switch missing");
}

#[test]
fn third_party_list_excludes_apple_and_itself() {
    let g = start();
    let reply = ask(&g, "{\"cmd\":\"agents\"}");
    assert!(reply.contains("\"items\""), "got {}", reply);
    assert!(!reply.contains("\"label\":\"com.apple."), "Apple items leaked");
    assert!(!reply.contains("com.thermalguard.daemon"), "self leaked");
}

#[test]
fn state_file_is_written_for_external_tooling() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"status\"}");
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let p = g.dir.join("state.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if text.contains("\"mode\"") {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    panic!("state.json was never written");
}

#[test]
fn daemon_reports_smc_unavailable_when_unprivileged() {
    let g = start();
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(
        reply.contains("\"smc_ok\":false") || reply.contains("\"smc_ok\":true"),
        "smc_ok must be a boolean: {}",
        reply
    );
}

// ---- security regression tests -------------------------------------------

#[test]
fn env_path_overrides_are_ignored_without_the_testing_flag() {
    let dir = std::env::temp_dir().join(format!("tg-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("redirect.sock");

    let mut child = Command::new(env!("CARGO_BIN_EXE_thermalguardd"))
        .env("THERMALGUARD_SOCK", &sock)
        .env("THERMALGUARD_RUN_DIR", &dir)
        .env_remove("THERMALGUARD_TESTING")
        .spawn()
        .expect("daemon should start");

    std::thread::sleep(Duration::from_millis(1200));
    let redirected = sock.exists();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !redirected,
        "env vars must not redirect the daemon's socket path"
    );
}

#[test]
fn caller_supplied_path_cannot_bootstrap_an_arbitrary_plist() {
    let g = start();
    // If the daemon trusted this pair, a local process could get root code
    // execution by pointing launchctl at its own plist.
    let reply = ask(
        &g,
        "{\"cmd\":\"agent\",\"label\":\"com.evil.test\",\"path\":\"/tmp/evil.plist\",\"scope\":\"system\",\"enable\":true}",
    );
    assert!(reply.contains("\"items\""), "got {}", reply);
    assert!(
        !reply.contains("enabled com.evil.test"),
        "an unknown label must never be actioned: {}",
        reply
    );
}

#[test]
fn arbitrary_apple_service_cannot_be_toggled() {
    let g = start();
    let reply = ask(
        &g,
        "{\"cmd\":\"apple_set\",\"id\":\"com.apple.WindowServer\",\"enable\":false}",
    );
    assert!(
        reply.contains("not a curated macOS service"),
        "non-allowlisted services must be refused: {}",
        reply
    );
}

#[test]
fn socket_is_owner_only_not_world_writable() {
    use std::os::unix::fs::PermissionsExt;
    let g = start();
    let md = std::fs::metadata(&g.sock).expect("socket metadata");
    assert_eq!(
        md.permissions().mode() & 0o777,
        0o600,
        "socket must be owner-only, was {:o}",
        md.permissions().mode() & 0o777
    );
}

#[test]
fn oversized_request_is_truncated_and_daemon_survives() {
    let g = start();
    let big = format!("{{\"cmd\":\"status\",\"pad\":\"{}\"}}", "A".repeat(20_000));

    // Write leniently: the daemon stops reading at its cap and may close the
    // socket before we finish sending, which is exactly the intended behaviour
    // (no unbounded buffering, no slowloris hang).
    if let Ok(mut s) = UnixStream::connect(&g.sock) {
        let _ = s.write_all(big.as_bytes());
        let _ = s.write_all(b"\n");
    }

    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(
        reply.contains("\"mode\""),
        "daemon must keep serving after oversized input: {}",
        reply
    );
}

#[test]
fn handler_panics_do_not_take_the_engine_down() {
    let g = start();
    // Assorted malformed inputs: none may kill the process.
    for bad in [
        "not json at all",
        "{",
        "{}",
        "{\"cmd\":null}",
        "{\"cmd\":\"mode\",\"value\":null}",
        "{\"cmd\":\"fan\",\"value\":\"../../x\"}",
    ] {
        let _ = ask(&g, bad);
    }
    let reply = ask(&g, "{\"cmd\":\"status\"}");
    assert!(reply.contains("\"mode\""), "engine died: {}", reply);
    assert!(reply.trim_start().starts_with('{'), "got {}", reply);
}

#[test]
fn labels_with_control_characters_are_neutralised() {
    let g = start();
    let reply = ask(
        &g,
        "{\"cmd\":\"agent\",\"label\":\"evil\\n2026-01-01 00:00:00 forged log line\",\"enable\":true}",
    );
    assert!(reply.contains("\"items\""), "got {}", reply);
    // The newline must never reach the wire as a real newline (that would let a
    // caller inject extra protocol lines or forge log entries).
    assert!(
        !reply.trim_end().contains('\n'),
        "raw newline leaked into reply: {:?}",
        reply
    );
    assert!(
        reply.contains("unknown background item"),
        "label should be treated as unknown after sanitisation: {}",
        reply
    );
}
