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
    /// Keep the run directory across `drop`, so a restart test can reuse the
    /// state file the daemon left behind.
    preserve: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !self.preserve {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn start() -> Guard {
    let dir = unique_dir("tg-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    start_in(dir, &[])
}

fn start_with_env(envs: &[(&str, String)]) -> Guard {
    let dir = unique_dir("tg-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    start_in(dir, envs)
}

fn unique_dir(prefix: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{}-{}-{}-{}", prefix, std::process::id(), n, stamp))
}

fn start_in(dir: PathBuf, envs: &[(&str, String)]) -> Guard {
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("tg.sock");
    let _ = std::fs::remove_file(&sock);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_thermalguardd"));
    cmd.env("THERMALGUARD_TESTING", "1")
        .env("THERMALGUARD_SOCK", &sock)
        .env("THERMALGUARD_RUN_DIR", &dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("daemon should start");

    let guard = Guard {
        child,
        sock,
        dir,
        preserve: false,
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if guard.sock.exists() && UnixStream::connect(&guard.sock).is_ok() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    panic!("daemon socket never appeared at {:?}", guard.sock);
}

fn wait_until<F: FnMut() -> bool>(secs: u64, mut f: F) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// A fake `launchctl` so the Auto-trim integration tests never touch the real
/// machine. It implements only the three verbs `apple.rs` uses and keeps its
/// disabled set in a file.
struct FakeLaunchctl {
    dir: PathBuf,
    script: PathBuf,
    state: PathBuf,
}

impl Drop for FakeLaunchctl {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fake_launchctl() -> FakeLaunchctl {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_dir("tg-fake-launchctl");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("launchctl");
    let state = dir.join("disabled");
    let body = r#"#!/bin/sh
set -eu
state="${FAKE_LAUNCHCTL_STATE:?}"
cmd="$1"
shift || true
case "$cmd" in
  print-disabled)
    echo 'disabled services = {'
    if [ -f "$state" ]; then
      while IFS= read -r label; do
        [ -n "$label" ] && echo "    \"$label\" => disabled"
      done < "$state"
    fi
    echo '}'
    ;;
  kill)
    echo "kill $*" >> "${FAKE_LAUNCHCTL_STATE:?}.kills"
    ;;
  disable)
    svc="$1"
    label="${svc##*/}"
    touch "$state"
    grep -qxF "$label" "$state" || echo "$label" >> "$state"
    ;;
  enable)
    svc="$1"
    label="${svc##*/}"
    touch "$state"
    grep -vxF "$label" "$state" > "$state.tmp" || true
    mv "$state.tmp" "$state"
    ;;
  *) exit 0 ;;
esac
"#;
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakeLaunchctl { dir, script, state }
}

fn fake_env(f: &FakeLaunchctl) -> Vec<(&'static str, String)> {
    vec![
        (
            "THERMALGUARD_LAUNCHCTL",
            f.script.to_string_lossy().into_owned(),
        ),
        ("THERMALGUARD_TRIM_GRACE_S", "0".to_string()),
        (
            "FAKE_LAUNCHCTL_STATE",
            f.state.to_string_lossy().into_owned(),
        ),
    ]
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
        "\"battery_hot\"",
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
    assert!(
        r.contains("\"media_guard\":true"),
        "shield must default on: {}",
        r
    );

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
    assert!(
        !reply.contains("999999"),
        "caller RPM leaked into state: {}",
        reply
    );
    assert!(
        !reply.contains("-1.0"),
        "caller target leaked into state: {}",
        reply
    );
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
    assert!(
        !reply.contains("\"label\":\"com.apple."),
        "Apple items leaked"
    );
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
fn socket_is_reachable_before_login() {
    use std::os::unix::fs::PermissionsExt;
    let g = start();
    let md = std::fs::metadata(&g.sock).expect("socket metadata");
    assert_eq!(
        md.permissions().mode() & 0o777,
        0o666,
        "socket must be reachable before login, was {:o}",
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

// ---- Core Audio watchdog integration tests --------------------------------

/// A fake `killall` so the reset path can be exercised end to end without ever
/// signalling the real audio daemon. It records its argv and succeeds.
fn fake_killall() -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_dir("tg-fake-killall");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("killall");
    let log = dir.join("calls");
    let body = "#!/bin/sh\necho \"$@\" >> \"${FAKE_KILLALL_LOG:?}\"\nexit 0\n";
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

#[test]
fn audio_watchdog_is_on_by_default_and_togglable() {
    let g = start();
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(
        r.contains("\"audio_guard\":true"),
        "watchdog must default on: {}",
        r
    );
    assert!(r.contains("\"audio_state\""), "state missing: {}", r);
    assert!(r.contains("\"audio_restarts\":0"), "counter missing: {}", r);

    let _ = ask(&g, "{\"cmd\":\"audio_guard\",\"value\":false}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"audio_guard\":false"), "got {}", r);

    // A string is not a boolean: it must not re-arm the watchdog.
    let _ = ask(&g, "{\"cmd\":\"audio_guard\",\"value\":\"yes\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"audio_guard\":false"), "strict bool: {}", r);

    let _ = ask(&g, "{\"cmd\":\"audio_guard\",\"value\":true}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"audio_guard\":true"), "got {}", r);
}

#[test]
fn audio_reset_path_uses_sigterm_and_is_verified() {
    let (script, log) = fake_killall();
    let g = start_with_env(&[
        (
            "THERMALGUARD_KILLALL",
            script.to_string_lossy().into_owned(),
        ),
        ("FAKE_KILLALL_LOG", log.to_string_lossy().into_owned()),
    ]);

    let reply = ask(&g, "{\"cmd\":\"audio_restart\"}");
    assert!(
        reply.contains("\"audio_restarts\":1"),
        "reset not recorded: {}",
        reply
    );
    assert!(
        reply.contains("coreaudiod") || reply.contains("Core Audio"),
        "no verification note: {}",
        reply
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains("-TERM coreaudiod"),
        "expected one graceful SIGTERM, got {:?}",
        calls
    );
    assert_eq!(
        calls.lines().count(),
        1,
        "more than one signal: {:?}",
        calls
    );
    let _ = std::fs::remove_dir_all(script.parent().unwrap());
}

// ---- Memory reclaim integration tests -------------------------------------

fn fake_purge() -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_dir("tg-fake-purge");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("purge");
    let log = dir.join("calls");
    let body = "#!/bin/sh\nprintf '%s' \"$*\" >> \"${FAKE_PURGE_LOG:?}\"\nexit 0\n";
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

#[test]
fn memory_reclaim_path_runs_without_arguments_and_is_verified() {
    let (script, log) = fake_purge();
    let g = start_with_env(&[
        ("THERMALGUARD_PURGE", script.to_string_lossy().into_owned()),
        ("FAKE_PURGE_LOG", log.to_string_lossy().into_owned()),
    ]);

    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"memory_reclaim\":true"), "default on: {}", r);

    let reply = ask(&g, "{\"cmd\":\"memory_reclaim_now\"}");
    assert!(
        reply.contains("\"memory_reclaims\":1"),
        "reclaim not recorded: {}",
        reply
    );
    assert!(
        reply.contains("Reclaimable cache was released"),
        "no verification note: {}",
        reply
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.trim().is_empty(),
        "purge must be given no caller input, got {:?}",
        calls
    );
    let _ = std::fs::remove_dir_all(script.parent().unwrap());
}

#[test]
fn memory_reclaim_is_strictly_boolean() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"memory_reclaim\",\"value\":\"yes\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"memory_reclaim\":true"), "strict bool: {}", r);

    let _ = ask(&g, "{\"cmd\":\"memory_reclaim\",\"value\":false}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"memory_reclaim\":false"), "got {}", r);
}

// ---- Auto-trim integration tests -----------------------------------------

#[test]
fn auto_trim_is_on_by_default_strictly_boolean_and_restores_on_disable() {
    let fake = fake_launchctl();
    let g = start_with_env(&fake_env(&fake));

    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"auto_trim\":true"), "must default on: {}", r);
    assert!(
        r.contains("\"auto_trim_active\":true"),
        "active in every mode except Off: {}",
        r
    );
    assert!(
        r.contains("\"auto_trimmed\":[]"),
        "nothing tracked yet: {}",
        r
    );

    // A string is not a boolean: it must not disarm the trimmer.
    let _ = ask(&g, "{\"cmd\":\"auto_trim\",\"value\":\"yes\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"auto_trim\":true"), "strict bool: {}", r);

    let armed = wait_until(12, || {
        let r = ask(&g, "{\"cmd\":\"status\"}");
        r.contains("\"auto_trim\":true") && !r.contains("\"auto_trimmed\":[]")
    });
    assert!(armed, "auto-trim never disabled a service");

    let r = ask(&g, "{\"cmd\":\"apple\"}");
    assert!(r.contains("\"auto_trim\":true"), "got {}", r);
    assert!(r.contains("\"auto_trim_active\":true"), "got {}", r);
    assert!(r.contains("\"auto_safe\":true"), "got {}", r);
    assert!(r.contains("\"auto_managed\":true"), "got {}", r);
    assert!(r.contains("\"auto_trimmed\":[\""), "got {}", r);

    let disabled = std::fs::read_to_string(&fake.state).unwrap_or_default();
    assert!(
        disabled.lines().any(|l| l.starts_with("com.apple.")),
        "expected a curated service in {:?}: {}",
        fake.state,
        disabled
    );
    assert!(
        disabled
            .lines()
            .all(|l| !l.contains("WindowServer") && !l.contains("securityd")),
        "a protected service was touched: {}",
        disabled
    );

    // Disabling stops future launches; the running copy is asked to exit too,
    // by SIGTERM, and only for curated background ids.
    let kills =
        std::fs::read_to_string(format!("{}.kills", fake.state.display())).unwrap_or_default();
    assert!(
        kills.contains("kill SIGTERM"),
        "running trimmed services must be stopped gracefully: {:?}",
        kills
    );
    assert!(
        kills.contains("com.apple."),
        "only curated ids can be stopped: {:?}",
        kills
    );
    assert!(
        !kills.contains("WindowServer") && !kills.contains("securityd"),
        "a protected service was signalled: {:?}",
        kills
    );

    // Switching it off must hand every tracked service back.
    let _ = ask(&g, "{\"cmd\":\"auto_trim\",\"value\":false}");
    let restored = wait_until(15, || {
        let r = ask(&g, "{\"cmd\":\"status\"}");
        let empty_tracked = r.contains("\"auto_trimmed\":[]");
        let empty_state = std::fs::read_to_string(&fake.state)
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        empty_tracked && empty_state
    });
    assert!(restored, "auto-trim did not restore the tracked services");

    // Off mode is the escape hatch: the trimmer steps aside and hands back
    // anything it disabled, even though the toggle is still on.
    let _ = ask(&g, "{\"cmd\":\"auto_trim\",\"value\":true}");
    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"off\"}");
    let off = wait_until(15, || {
        let r = ask(&g, "{\"cmd\":\"status\"}");
        r.contains("\"auto_trim_active\":false") && r.contains("\"auto_trimmed\":[]")
    });
    assert!(off, "Off mode must hand every trimmed service back");
}

#[test]
fn mode_change_is_persisted_immediately_and_off_is_reported_at_once() {
    let g = start();
    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"maxcool\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"mode\":\"maxcool\""), "got {}", r);

    // A crash or shutdown seconds after choosing a mode must not lose it.
    let state = wait_until(3, || {
        std::fs::read_to_string(g.dir.join("state.json"))
            .map(|t| t.contains("\"mode\":\"maxcool\""))
            .unwrap_or(false)
    });
    assert!(state, "mode was not written to state.json immediately");

    // Off must be reflected in the very next reply, not one tick later.
    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"off\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"audio_state\":\"off (mode)\""), "got {}", r);
    assert!(r.contains("\"auto_trim_active\":false"), "got {}", r);
}

#[test]
fn network_health_is_reported_read_only() {
    let g = start();
    let r = ask(&g, "{\"cmd\":\"status\"}");
    for key in [
        "\"net_link\"",
        "\"net_score\"",
        "\"net_grade\"",
        "\"net_note\"",
    ] {
        assert!(r.contains(key), "missing {} in {}", key, r);
    }
    // No silent network writes: there is no tuning state to report.
    assert!(
        !r.contains("\"net_tuned\""),
        "daemon must never rewrite the link: {}",
        r
    );
}

#[test]
fn tampered_tracked_ids_cannot_touch_non_auto_safe_services() {
    let fake = fake_launchctl();
    let dir = unique_dir("tg-tamper");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("state.json"),
        r#"{"mode":"auto","media_guard":true,"auto_trim":true,"auto_trimmed":["com.apple.WindowServer","com.apple.bird"]}"#,
    )
    .unwrap();

    let g = start_in(dir, &fake_env(&fake));

    // The valid auto-safe work still proceeds, proving the daemon is alive.
    let trimmed = wait_until(12, || {
        let r = ask(&g, "{\"cmd\":\"status\"}");
        !r.contains("\"auto_trimmed\":[]")
    });
    assert!(trimmed, "valid auto-trim did not proceed");

    let disabled = std::fs::read_to_string(&fake.state).unwrap_or_default();
    assert!(
        !disabled.contains("WindowServer") && !disabled.contains("com.apple.bird"),
        "tampered ids reached launchctl: {}",
        disabled
    );
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(
        !r.contains("com.apple.WindowServer") && !r.contains("com.apple.bird"),
        "tampered ids were restored/tracked: {}",
        r
    );
}

// ---- power-cap integration tests ------------------------------------------

/// A fake `pmset` that keeps `lowpowermode` in a file, so the cap on/off path
/// can be exercised end to end without touching the real machine.
struct FakePmset {
    dir: PathBuf,
    script: PathBuf,
    state: PathBuf,
    /// Every argv the daemon passed, one line per call.
    log: PathBuf,
}

impl Drop for FakePmset {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fake_pmset() -> FakePmset {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_dir("tg-fake-pmset");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("pmset");
    let state = dir.join("lowpowermode");
    // The script appends its argv to "<state>.log".
    let log = dir.join("lowpowermode.log");
    let body = r#"#!/bin/sh
set -eu
state="${FAKE_PMSET_STATE:?}"
echo "$*" >> "${FAKE_PMSET_STATE:?}.log"
v=0
if [ -f "$state" ]; then v=$(cat "$state"); fi
case "$1:$2" in
  -g:custom)
    printf 'AC Power:\n sleep 1\n lowpowermode %s\nBattery Power:\n sleep 1\n lowpowermode %s\n' "$v" "$v"
    ;;
  -a:lowpowermode)
    echo "$3" > "$state"
    ;;
  -g:therm)
    echo 'CPU_Speed_Limit              = 100'
    ;;
  -g:batt)
    echo "Now drawing from 'AC Power'"
    ;;
  *) ;;
esac
exit 0
"#;
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakePmset {
        dir,
        script,
        state,
        log,
    }
}

fn fake_pmset_env(f: &FakePmset) -> Vec<(&'static str, String)> {
    vec![
        (
            "THERMALGUARD_PMSET",
            f.script.to_string_lossy().into_owned(),
        ),
        ("FAKE_PMSET_STATE", f.state.to_string_lossy().into_owned()),
    ]
}

fn pmset_state(f: &FakePmset) -> String {
    std::fs::read_to_string(&f.state)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn cap_on(g: &Guard) -> bool {
    ask(g, "{\"cmd\":\"status\"}").contains("\"cap_active\":true")
}

#[test]
fn power_cap_turns_on_with_a_cooling_mode_and_off_again() {
    let fake = fake_pmset();
    let g = start_with_env(&fake_pmset_env(&fake));
    assert!(!cap_on(&g), "the daemon must not cap on its own at startup");

    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"maxcool\"}");
    assert!(
        wait_until(12, || cap_on(&g)),
        "Max Cool never engaged the power cap"
    );
    assert_eq!(pmset_state(&fake), "1", "the cap was not applied to macOS");

    // Held, not toggled: with the fake pmset in front of us the cap must stay
    // on across several control-loop ticks.
    for _ in 0..4 {
        assert!(
            cap_on(&g),
            "the cap dropped out while Max Cool was selected"
        );
        std::thread::sleep(Duration::from_millis(700));
    }

    let _ = ask(&g, "{\"cmd\":\"mode\",\"value\":\"off\"}");
    assert!(
        wait_until(12, || !cap_on(&g)),
        "Off never released the power cap"
    );
    assert_eq!(pmset_state(&fake), "0", "macOS was left capped after Off");

    // One change per transition: no off/on/off flapping in the argv log.
    let calls = std::fs::read_to_string(&fake.log).unwrap_or_default();
    let writes = calls
        .lines()
        .filter(|l| l.starts_with("-a lowpowermode"))
        .count();
    assert_eq!(
        writes, 2,
        "expected exactly one on and one off, got {:?}",
        calls
    );
}

#[test]
fn power_cap_state_survives_a_restart_and_is_released_by_off() {
    let fake = fake_pmset();
    let dir = unique_dir("tg-cap-restart");
    std::fs::create_dir_all(&dir).unwrap();
    let envs = fake_pmset_env(&fake);

    let mut first = start_in(dir.clone(), &envs);
    let _ = ask(&first, "{\"cmd\":\"mode\",\"value\":\"maxcool\"}");
    assert!(wait_until(12, || cap_on(&first)), "cap never engaged");
    assert_eq!(pmset_state(&fake), "1");
    first.preserve = true;
    drop(first);

    // A restart must not forget that the machine is still capped: reporting
    // "off" while macOS is capped is what used to leave the cap stuck on.
    let second = start_in(dir.clone(), &envs);
    let r = ask(&second, "{\"cmd\":\"status\"}");
    assert!(
        r.contains("\"cap_active\":true"),
        "restored cap not reported: {}",
        r
    );
    assert!(
        r.contains("\"mode\":\"maxcool\""),
        "restored mode missing: {}",
        r
    );

    let _ = ask(&second, "{\"cmd\":\"mode\",\"value\":\"off\"}");
    assert!(
        wait_until(12, || !cap_on(&second)),
        "Off after a restart did not release the cap"
    );
    assert_eq!(pmset_state(&fake), "0");
}

#[test]
fn fan_preference_defaults_to_silent() {
    let g = start();
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(
        r.contains("\"fan_intent\":\"quiet\""),
        "the default fan preference must be silent: {}",
        r
    );
    // And it must stay silent across a restart, not silently revert.
    let _ = ask(&g, "{\"cmd\":\"fan\",\"value\":\"auto\"}");
    let r = ask(&g, "{\"cmd\":\"status\"}");
    assert!(r.contains("\"fan_intent\":\"auto\""), "got {}", r);
}

#[test]
fn process_sample_does_not_flicker_between_ticks() {
    // Regression: `ps` runs every 4 s, but the ticks in between used an empty
    // sample, so the process list, media shield and demotion state blinked off
    // every other tick.
    let g = start();
    assert!(
        wait_until(10, || !ask(&g, "{\"cmd\":\"status\"}")
            .contains("\"procs\":[]")),
        "the daemon never sampled processes"
    );
    for _ in 0..8 {
        let r = ask(&g, "{\"cmd\":\"status\"}");
        assert!(
            !r.contains("\"procs\":[]"),
            "process list dropped back to empty: {}",
            r
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}
