# Security review — ThermalGuard

Scope: the privileged daemon (`thermalguardd`, runs as root), its IPC surface,
the Swift UI that drives it, and the installer script. Reviewed adversarially
before shipping; every finding below was fixed in code and is covered by a
regression test.

## Threat model

- **Local attacker** (another user account, or a compromised process running as
  the logged-in user) with access to `/var/run/thermalguard.sock`.
- **Malicious input** through the IPC JSON (labels, ids, paths).
- **Supply chain**: a swapped helper payload or a modified `PATH`.
- Out of scope: an attacker who already has root; physical access with an
  unlocked firmware password; notarisation/Developer-ID trust.

## Findings and fixes

| # | Severity | Finding | Fix | Test |
|---|----------|---------|-----|------|
| 1 | **High** | Root daemon socket was mode `0666` and accepted commands from *any* local uid — e.g. `agent` could stop a critical third-party service, or disable Spotlight. | Peer credential check with `getpeereid(2)`; only `uid 0` and the console user are served. Unauthorised peers get `{"error":"unauthorised"}` and the attempt is logged. | manual + code path guard |
| 2 | **High** | `agent` took `path` and `scope` from the JSON and passed them to `launchctl bootstrap` **as root** → arbitrary root code execution. | The caller supplies a *label* only; the daemon re-scans and resolves path/scope itself, then re-validates with `path_allowed()` (only `/Library/LaunchDaemons`, `/Library/LaunchAgents`, `/Users/*/Library/LaunchAgents`; no `..`, must be `.plist`). | `caller_supplied_path_cannot_bootstrap_an_arbitrary_plist`, `attacker_controlled_paths_are_rejected` |
| 3 | **Medium** | `apple_set` accepted any `com.apple.*` label, so a caller could disable unrelated Apple services (WindowServer, securityd). | Toggle limited to a curated allowlist (`is_known`); unknown ids return `not a curated macOS service` before any process is spawned. | `arbitrary_apple_service_cannot_be_toggled`, `arbitrary_apple_labels_are_rejected` |
| 4 | **Medium** | `short_name()` truncated at a *byte* index → a process named with multi-byte UTF-8 aborted the daemon (`panic = "abort"` + `KeepAlive` = crash loop → thermal protection silently lost). | Truncation is char-boundary safe and strips control characters. | `multibyte_names_never_panic_the_daemon`, `control_characters_are_stripped_from_names` |
| 5 | **Medium** | Log/JSON injection: attacker-supplied labels were written to `/var/log/thermalguard.log` verbatim (forged log lines) and echoed back in state. | `safe_label()` restricts to `[A-Za-z0-9._-/ ]` and 120 chars; names are control-character filtered. | `labels_with_control_characters_are_neutralised` |
| 6 | **Low** | Root daemon resolved child binaries via `PATH` (`ps`, `pmset`, `launchctl`, …) → PATH-hijack to attacker binary if the environment were ever altered. | Every child process is invoked by absolute path. | code review |
| 7 | **Low** | Installer script copied `$1` to `/Library/PrivilegedHelperTools` with root rights and interpolated the path into a plist. | Input must be `*.app` with `Contents/`, no newline/quote/backslash; source must be a regular file (not a symlink) whose first 4 bytes are a Mach-O magic; copied as `root:wheel 0755`. | manual review + runtime guard |
| 8 | **Low** | UI built the login-item plist and the admin shell command by string concatenation. | Plist is written with `PropertyListSerialization` (no XML injection) and shell arguments use POSIX single-quote escaping. | manual review |
| 9 | **Low** | The `enable` flag was read with a substring match (`req.contains("\"enable\":true")`), so the text could be smuggled inside another field and flip a service toggle the user never asked for. | `json::get_bool()` accepts only a real `true`/`false` literal directly after `"enable":`. | `enable_is_only_read_from_a_real_boolean_field` |

| 10 | **Medium** | Auto-trim could have disabled arbitrary Apple services if a caller could widen the allowlist or the persisted tracked set. | Auto-trim is opt-in (default off), active only in Auto mode, and only compiled-in `auto_safe` ids are ever eligible. The persisted tracked set is re-validated against the catalog on every tick and on load; tampered ids are ignored. Only one service changes per daemon tick, and SIP-refused services are backed off for 24 h. | `auto_trim_is_opt_in_strictly_boolean_and_restores_on_disable`, `tampered_tracked_ids_cannot_touch_non_auto_safe_services`, `trim::*` (11 tests) |
| 11 | **Medium** | Chrome's background mode and Google agents could be touched while Chrome is running, racing the app and losing the original value. | `disableChromeBackground` only runs once Chrome has quit; the original `background_mode` is stored (as JSON) before overwriting, and `restoreChromeBackground` only writes when the guard is off or Chrome is closed. Google agents are disabled/enabled by exact label against a compiled-in list; the stored list is re-filtered to that catalog on restore. | `chromeGuardAction` pure policy (11 tests), `isChromeHelperPath` (4 tests) |
| 12 | **Medium** | Chrome helpers could be killed by PID reuse if a PID was recycled between listing and signalling. | The executable path is re-read with `ps -p <pid> -o comm=` immediately before `kill`, and `isChromeHelperPath` is re-applied; the main Chrome binary is explicitly excluded. | `isChromeHelperPath` tests |
| 13 | **Low** | Graceful memory unloading could have force-killed or purged memory. | Only `terminate()` is ever used (graceful, app can refuse); `purge` runs only through the daemon's own bounded reclaim policy (finding 17), never from the UI; protected, frontmost, windowed, audio, camera and busy apps are excluded, and a refusal is backed off for 30 minutes. | `shouldQuitIdleApp` (13 tests), `memoryPressureLevel` (6 tests) |
| 14 | **Low** | The T2/codec guard could have claimed hardware support on models without it, or installed a browser extension silently. | Support is read from VideoToolbox (`VTIsHardwareDecodeSupported` plus `VTCopyVideoEncoderList` for encode), which is model-generic. h264ify is detected read-only and only a Web Store link is offered; nothing is installed or enabled. | `codecVerdict` (5 tests) |
| 15 | **Low** | Auto-trim leaves services disabled if the app is uninstalled while trimmed. | Documented; the daemon restores tracked services on leaving Auto / toggling off / startup. Uninstall instructions now tell the user to turn Auto-trim off first. | manual |
| 16 | **Medium** | An automatic `coreaudiod` restart is a root signal to a shared system daemon: unthrottled it would be a denial-of-service on audio, and a caller-supplied target would be arbitrary process signalling. | The watchdog only fires on two consecutive samples above 20 % CPU, at most once per 10 min and 3×/hour, sending one SIGTERM (never SIGKILL) to the compiled-in name `coreaudiod` via absolute `/usr/bin/killall` with an argv array; the relaunch is verified by re-reading the PID. The `audio_restart` IPC hook exists only when `overrides_allowed()` (non-root + `THERMALGUARD_TESTING=1`) and is refused by the installed daemon. | `audio::*` (6 unit tests), `audio_watchdog_is_on_by_default_and_togglable`, `audio_reset_path_uses_sigterm_and_is_verified` |
| 17 | **Medium** | An automatic `purge` driven by IPC input could be turned into a repeated machine-wide stall, or accept caller-supplied flags. | The daemon decides on its own measured numbers; the IPC setting is a strict boolean only. `purge` is called with an empty argv and absolute path, only when the kernel reports warning/critical pressure, ≥ 1 GB is provably reclaimable, swap is below 70 %, and the 30 min / 2-per-hour budget allows it. The `memory_reclaim_now` hook exists only under `overrides_allowed()` and is refused by the installed daemon. | `memory::*` (6 unit tests), `memory_reclaim_path_runs_without_arguments_and_is_verified`, `memory_reclaim_is_strictly_boolean` |

## Auto-trim safety invariants

- Opt-in only (default off); active only in Auto cooling mode.
- Only the compiled-in `auto_safe` subset (33 services) is ever eligible; manual-only
  entries (security, MDM, updates, backup, iCloud sync, communications, Siri's
  interactive services) are never touched automatically.
- Grace period before the first disable; one action per daemon tick.
- SIP-refused services are backed off for 24 h and never retried in a loop.
- Tracked ids are validated against the catalog on every read; a tampered state
  file cannot widen the set.
- Manually re-enabled services are forgotten, never fought over.
- Leaving Auto, switching the toggle off, or restarting the daemon restores every
  tracked service; manuallly disabled services are never re-enabled.

## Chrome guard invariants

- Default on, user-toggleable, persisted in `UserDefaults`.
- Chrome is only touched when it has no normal window, is not frontmost, is not
  launching, holds no audio assertion and is not doing media work (GPU helper CPU
  or a live camera).
- Quit is requested with `terminate()` (graceful); SIGTERM is sent only to
  helpers whose executable path is re-verified.
- The original `background_mode` value is stored before overwriting and restored
  on disable/uninstall-of-the-guard.
- Only the three compiled-in Google agent labels are ever disabled/enabled.

## Memory guard invariants

- Default on, runs in every mode except Off.
- Pressure is derived from `vm_stat` and `vm.swapusage`, exactly like the UI, so
  both sides agree on `normal` / `high` / `critical`.
- The pressure band is `kern.memorystatus_vm_pressure_level` (1 normal,
  2 warning, 4 critical), the kernel's own verdict; free-page thresholds are
  only a fallback when it is unreadable. macOS keeps free pages low on purpose,
  so a free-percentage-only rule would purge a healthy machine.
- `purge` runs only under high/critical kernel pressure, with at least 1 GB of
  measured reclaimable (purgeable + file-backed) cache, and swap use below 70 %.
  It is rate-limited to once per 30 minutes and twice per hour, takes **no
  arguments**, is invoked by absolute path, and its effect is measured
  afterwards rather than assumed.
- Candidates must be regular user apps under `/Applications` or `~/Applications`,
  not protected, with no window, not frontmost, no audio, no camera, CPU below
  3 %, and idle for 10 min (5 min at critical pressure).
- Quit is requested with `terminate()` only; a refusal is backed off 30 min.

## Codec guard invariants

- Default on, runs in every mode except Off.
- Hardware support comes from VideoToolbox, so it is model-generic.
- h264ify is detected read-only; only a Web Store link is offered.


## Core Audio watchdog invariants

- Default on, runs in every mode except Off; user-toggleable and persisted in
  the daemon's state file.
- Only `coreaudiod` is ever signalled, by compiled-in name, with SIGTERM — never
  SIGKILL, never a caller-supplied pid or process name.
- Two consecutive samples above 20 % CPU are required, so one scheduling blip
  cannot tear down the audio stack.
- Ten-minute cooldown and a hard cap of three resets per rolling hour bound the
  worst case; a failed reset is reported and backed off by the same cooldown.
- The result is verified by re-reading the PID and logged; the UI is only a view
  over the daemon's reported state and never runs `killall` itself.

## Verified properties (updated)

- Apple services cannot be disabled by the daemon even if asked (two layers:
  exact allowlist in `apple.rs`, plus `com.apple.` refusal in `agents.rs`).
- The Auto-trim tracked set is validated against the compiled-in catalog on
  load and on every tick.
- Third-party security agents and peripheral drivers, XProtect, `syspolicyd`,
  `securityd` are in the never-demote allowlist and covered by tests.
- No `Command` in the daemon is built from a shell string: all invocations are
  argv arrays with absolute paths (no `sh -c`).
- The daemon performs no network I/O of any kind.
- All changes the app makes are reversible (`pmset`, `mdutil`, `launchctl
  disable/enable`, Darwin QoS priority, Chrome background mode, Google agents);
  nothing is deleted. The single exception is a Chrome helper that is asked to
  exit via SIGTERM after its executable path is re-verified.
- `purge` is invoked only by the daemon's measured reclaim policy (finding 17), with an empty argv, after the kernel itself reports memory pressure.
- Apple services cannot be disabled by the daemon even if asked (two layers:
  exact allowlist in `apple.rs`, plus `com.apple.` refusal in `agents.rs`).
- Third-party security agents and peripheral drivers, XProtect, `syspolicyd`,
  `securityd` are in the never-demote allowlist and covered by tests.
- No `Command` in the daemon is built from a shell string: all invocations are
  argv arrays with absolute paths (no `sh -c`).
- The daemon performs no network I/O of any kind.
- All changes the app makes are reversible (`pmset`, `mdutil`, `launchctl
  disable/enable`, Darwin QoS priority); nothing is deleted.

## Beyond the fixes: hardening measures

| Measure | What it buys | Verified by |
|---|---|---|
| Socket created `0600` via `umask(0177)` at bind, then `chown`ed to the console user + chmod `0600` | no window where another local user could connect; no world access at all | `socket_is_owner_only_not_world_writable`; live `stat` → `srw------- admin` |
| `getpeereid(2)` authorisation (root or console uid only) | defence in depth if permissions are ever wrong | live probes |
| Bounded request read (8 KiB cap, newline-terminated, 5 s read/write timeouts) | no heap growth, no slowloris thread pile-up | `oversized_request_is_truncated_and_daemon_survives` |
| Connection cap (16) with RAII counter | no thread-exhaustion from a connect loop | code path |
| `panic = "unwind"` + `catch_unwind` per handler | a panic drops one connection instead of killing thermal protection | `handler_panics_do_not_take_the_engine_down` |
| Log opened append-only, symlink-refused, forced `0600` | no symlink-follow write as root; log contents not world-readable | live `stat` → `-rw------- root` |
| State written to temp file + `rename()` | atomic, and rename replaces a planted symlink instead of following it | code review |
| Hardened runtime (`--options runtime`) on both binaries | code-injection / debugger hardening | `codesign -dv` → `flags=0x10002(runtime,adhoc)` |
| Installer verifies `codesign --verify --strict` on the bundle before copying as root | a tampered bundle cannot install a root payload | installer guard |
| LaunchDaemon `Umask 77` | daemon-created files are owner-only by default | plist |
| Fan target clamped to the SMC-reported min/max | no out-of-range SMC writes | code review |
| `THERMALGUARD_*` path overrides ignored when running as root (test-only, gated) | a planted env var cannot redirect the privileged daemon's socket or state | `env_path_overrides_are_ignored_without_the_testing_flag` |
| Log rotation above 512 KiB, trimmed at a line boundary, `0600`, symlink-refused | no unbounded root-owned log growth filling the disk | code review |
| Priority restore verifies the process identity (`ps -o comm=`) before acting | a recycled PID cannot be the wrong process we hand priority back to | `identity_check_rejects_dead_pids` |
| UI socket has 3 s send/receive timeouts and a 256 KiB response cap | a stalled daemon cannot freeze the menu-bar UI | code review |

## Residual risks (accepted, documented)

1. **Ad-hoc code signature, not notarised.** Gatekeeper prompts on first launch.
   Anyone who can write to `/Applications` could replace the binary; the helper
   is only installed by an explicit admin-authorised run of the bundled script.
   Shipping properly requires a Developer ID certificate + notarisation.
2. **Socket is still mode `0666`.** Acceptable because authorisation is by peer
   credential, not filesystem permission; the file lives in root-owned
   `/var/run`.
3. **PID reuse.** A demoted process that exits could have its PID reused within
   one 2 s sampling window; the worst case is a background QoS hint applied to
   an unrelated process, which is reversible and non-destructive.
4. **Uninstall leaves launchd's disabled-state entries** for any Apple service
   you disabled; re-enabling in the app or `launchctl enable` reverses them.

## The fan-write path (added with the Silent preference)

A privileged write to the SMC is hardware control, so it is treated like any
other privileged sink:

- **No value from the wire reaches a write.** The client may only send a
  preference string (`auto` / `quiet` / `max`), parsed against a whitelist; the
  RPM is always derived inside the daemon from the SMC's own reported `F0Mn` /
  `F0Mx`. There is no `rpm` field to abuse, and a smuggled one is ignored
  (`a_client_cannot_choose_an_arbitrary_fan_rpm`).
- **Bounds are sanity-checked first.** An implausible maximum (≤ 0, NaN, or
  > 20 000 RPM) means the SMC answer cannot be trusted, so nothing is written
  (`quiet_fan_target_refuses_implausible_ranges`).
- **Every write is verified by read-back** and only reported as supported when
  the SMC echoes the value.
- **A refusal is not retried in a loop** — 2-minute backoff, so a model without
  fan control cannot be hammered once per tick.
- **Safety over silence:** a silent target is dropped at 88 °C and only
  re-applied below 80 °C, and an unreadable temperature counts as unsafe
  (`quiet_fan_is_released_for_heat_and_only_resumes_once_cool`).
- **Release never forces the fan loud**: returning to the SMC curve writes
  `F0Md = 0` only. An earlier fallback wrote the fan *maximum*, which would
  have pinned the fan on a model that ignores targets — removed.

## Reproduce the checks

```bash
cargo test --manifest-path daemon/Cargo.toml   # 65 unit + 20 integration tests

# live checks against the installed daemon
stat -f "%Sp %Su" /var/run/thermalguard.sock      # expect srw------- <you>
stat -f "%Sp %Su" /var/log/thermalguard.log       # expect -rw------- root
codesign -dv --verbose=2 /Applications/ThermalGuard.app 2>&1 | grep flags
```
