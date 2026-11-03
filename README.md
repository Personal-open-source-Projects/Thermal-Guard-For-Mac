# ThermalGuard

Menu-bar app for **MacBook Air 9,1 (2020, Intel i5, T2)** that stops the
sustained overheating / fan-blast cycle at its source.

Built as: **Rust** privileged thermal core (`thermalguardd`) + **SwiftUI**
menu-bar UI + `.dmg` installer. No third‑party crates, no kexts, **no SIP
changes**.

## The popup

A frosted panel that hangs off the menu-bar item, sized to the screen it opens
on and dismissed by clicking anywhere outside it — no separate title-bar
window, and the bottom of the content can never fall off-screen. Inside: a
thermal ring
that fills with the die temperature, live fan / clock / battery / SMC readings,
a status strip of chips (mode, cap, OS throttling, call shield, T2 offload, RAM
pressure, auto-trim), then mode and fan pickers, the video & call guard, the
T2, Chrome and memory cards, a live per-process CPU card, and recent events. One accent colour runs through the whole window and follows the
machine: teal → green → orange → red. Software and macOS-service management
open as sheets, with a confirmation before anything is disabled.

The data layer is unchanged: the window is a pure view over the same daemon
tick, so every safety rule above still holds no matter what the UI shows.

---

## Root causes on this exact model

Verified on the target machine (`MacBookAir9,1`, macOS 15.7.5):

| # | Cause | Evidence | Handled by |
|---|-------|----------|-----------|
| 1 | **Package power exceeds what the chassis dissipates.** 10 W‑class Ice Lake chip allowed to boost to 3.5 GHz (ratio 35) into a ~10–13 W chassis. | `machdep.xcpm.soft_plimit_max_100mhz_ratio: 35` vs `lpm_plimit_max: 24` | Power cap (`pmset lowpowermode`) → ceiling drops to 2.4 GHz |
| 2 | **Fan is not thermally coupled to the heat pipe.** The SMC fan curve is acoustic‑first and has no user‑addressable target. So "fans at full blast" ≠ "cooler". | SMC `F0Ac` readable; `F0Tg` write is verified by read‑back and reported as unsupported when ignored | App detects this and says so instead of pretending |
| 3 | **Constant background CPU bursts** keep the chip in boost so it never idles. | Live: load avg 5.00 / 8 threads, `XprotectService` 95 % CPU | Reversible QoS demotion (Darwin background class) |
| 4 | **Security + vendor agents** (third-party system extensions and peripheral drivers) are resident and always expensive. | `systemextensionsctl list` → third-party `.sp` extensions active | **Explicitly protected — never throttled** |
| 5 | **Charging while hot** stacks battery/VRM heat on SoC heat. | Battery temp reported separately | Shown in UI so you can act |

Reinstalling macOS does not change #1 or #2 — they are design, not corruption.

## What the app does

- Reads real SMC sensors as root (`TC0P`/`TC0D`, `F0Ac`, battery temp) via
  direct IOKit FFI in Rust.
- Applies macOS' own supported power ceiling (`pmset -a lowpowermode 1`) —
  removing heat at the source, not after the fact.
- Demotes only *unprotected* CPU hogs to background QoS. **Nothing is killed,
  suspended or signalled**, and a process gets its normal priority back as soon
  as it stops hogging.
- **Never touches:** third-party security agents and peripheral drivers,
  XProtect, `syspolicyd`, `securityd`, MDM, and the window server / kernel.
- Chrome background guard (opt-in, default on, user-toggleable): when Chrome is
  genuinely idle it is asked to quit gracefully, its own background mode is
  switched off, the three known Google updater/Keystone agents are disabled, and
  leftover helper processes are sent SIGTERM only after their executable path is
  re-verified. Everything is restored when Chrome is used again or the toggle
  is turned off.
- Thermal-aware RAM governor (default on, every mode except Off): memory pressure
  is treated as a **heat** problem, not just a latency problem, because
  compressor churn and swap-in are CPU and disk work that turns straight into
  die temperature. The daemon measures the kernel's own counters
  (`compressions`, `decompressions`, `swapins`, `swapouts`) every 2 s and turns
  them into a churn rate, then walks an explicit ladder:
  *drop reclaimable cache → ask idle apps to quit* — and stops at the first rung
  that is both safe and justified. Nothing is force-killed.
  - **Cache first, always.** A purge is one bounded disk burst with no
    user-visible loss. Under pressure it goes before any app is touched, and it
    also runs *pre-emptively* when the compressor is climbing (200 pages/s, or
    +8 MB/s) while the chip still has headroom, because cheap I/O done early
    replaces expensive churn done late.
  - **Heat is a veto, not a trigger.** At ≥ 85 °C a purge is deferred unless
    pressure is critical or the compressor is already burning more CPU than the
    purge would — an extra I/O burst on a saturated chassis is not "help".
    A hot chip does not get an idle app quit for free either: it must be
    *churning* as well, so a hot machine with quiet memory is left alone.
  - **A futile purge demotes itself.** The result is measured (`freed MB`); if
    it freed under 128 MB it is not repeated for 15 minutes, and the ladder
    escalates to an app instead of burning the same I/O twice.
  - **Per-mode tolerances.** Auto and Balanced wait for critical pressure or a
    genuinely hot, churning chip. Cool & Quiet and Max Cool act at high
    pressure, treat a merely warm chip as enough, and get a shorter purge
    cooldown (15 min / 7.5 min) with a slightly larger hourly cap — still
    bounded, so the daemon can never purge in a loop.
  - **One decision, one executor.** The daemon publishes the rung
    (`memory_plan`), its reason (`memory_plan_reason`) and a sentence for the UI
    (`memory_plan_detail`). The unprivileged UI only executes the surviving
    `trim` rung; it no longer re-derives pressure from its own `vm_stat` fork,
    which removes a child process from the 15 s guard tick.
  - **"Free" means available, not idle.** The card reports free plus reclaimable
    (purgeable and file-backed) memory — what the kernel can hand out without
    swapping. Raw free pages sit near zero on a healthy Mac by design, which is
    what made the old card read 1 % and "unreadable"; the daemon publishes the
    available figure and the UI no longer computes a second one.
  - Sampling is Mach-only (`host_statistics64` + `sysctlbyname`, no `vm_stat`
    or `sysctl` children) and costs tens of microseconds, so it runs on every
    2 s tick. The compressor/swap **trend** is tracked over a 20 s window, so a
    machine climbing into pressure is visible before it is critical.
- **Reduced animations & visual effects** (default on, every mode except Off):
  writes the three macOS switches that cost the Intel Iris Plus the most —
  Reduce Motion, Reduce Transparency and automatic window animations — and puts
  the user's own values back (including "the key was never set") when the
  toggle is switched off or the mode is Off. These are per-user preferences, so
  the unprivileged UI writes them and the root daemon never does.
- **Core Audio watchdog** (default on, every mode except Off): when
  `coreaudiod` stays above 20 % CPU for two consecutive samples — the shape of
  a stuck audio engine behind stuttering playback and a fan spike — the daemon
  sends exactly one SIGTERM, launchd replaces the engine in about a second, and
  the new PID is verified before the reset is reported. Ten-minute cooldown,
  three resets per rolling hour, and a plain boolean to switch it off.
- **Background-service trimmer** (default on, every mode except Off):
  disables and gracefully stops (SIGTERM) only the compiled-in background-only
  Apple jobs — on-device ML, indexing, Siri analysis and telemetry — never
  security, MDM, backup or sync. 30 s grace before the first change, one service
  per tick, SIP refusals backed off, and a service you re-enable is forgotten
  rather than fought over. Everything is restored when the toggle goes off, the
  mode is Off, or the daemon restarts.
- **Network health** (read-only): measures the active link every 30 s — wired vs
  Wi-Fi, RSSI/noise, negotiated rate, latency to the router and packet loss —
  and scores it 0–100 with concrete advice. It deliberately never rewrites TCP
  tunables, DNS servers or radio settings: macOS already auto-tunes them, and a
  blind change is the quickest way to break connectivity.
- T2 / VideoToolbox offload guard (default on, every mode except Off): checks
  VideoToolbox directly, so it works on every Mac model, and reports whether
  H.264 and HEVC decode/encode are hardware-accelerated. When a video falls
  back to software decode it offers a link to h264ify; it never installs a
  browser extension.
- Reports honestly when a fan target is ignored (MacBookAir9,1).

### Modes

| Mode | Power cap | Demote threshold | RAM ladder |
|------|-----------|------------------|------------|
| **Auto** (default) | only above 78 °C (or when macOS starts throttling), released below 74 °C with 20 s of hysteresis | never — preserves foreground responsiveness | critical pressure, or a hot **and** churning chip |
| Off | never | – | monitoring only |
| Balanced | above 82 °C, release at 72 °C | ≥ 70 % CPU | critical pressure, or a hot **and** churning chip |
| Cool & Quiet | always | ≥ 45 % CPU | high pressure, or a warm churning chip |
| Max Cool | always | ≥ 30 % CPU | high pressure, or a warm churning chip |

The RAM column is the pressure at which the governor will consider asking an
idle app to quit. Cache is always dropped first, in every mode, and each mode
has its own purge cooldown (30 min / 30 min / 15 min / 7.5 min) and hourly cap
(2 / 2 / 3 / 4).

### Hardware acceleration, in auto mode

On this Intel Air, a browser with **hardware acceleration off** decodes video in
software, pegs both cores and hangs playback (an M1 hides this because it has
the headroom). Chromium browsers keep that flag in their own `Local State`, and
an update can put it back to `false`.

Watched automatically: Google Chrome, Microsoft Edge, Brave, Vivaldi, Chromium.

When it acts — and only then:

1. **Detection is real, not "the app is open".** A media session means a decode
   pipeline is working (`VTDecoderXPCService`, `VideoToolbox`, `AVConference`),
   or a camera is live anywhere in the system (`kCMIODeviceProperty-
   DeviceIsRunningSomewhere`, read without any permission prompt), or an
   unaccelerated browser is burning CPU — that last one *is* software decode.
2. **Never mid-session.** If a video or camera call is live, the guard holds.
   Restarting a decoder mid-call costs more than the heat it would save. It
   surfaces a **Fix now…** button if you would rather take the interruption.
3. **Repair once the session ends.** The app is marked as owed a fix the first
   time it is caught decoding with acceleration off, and repaired on the next
   quiet pass — restarted only if it is still running.

If acceleration is already on and nothing is playing, it does nothing at all:
no restarts, no config writes, no polls beyond the existing 2 s dashboard tick.

Headless entry points, also used to test it:

```bash
ThermalGuard --self-test      # 84 unit tests over the decision tables
ThermalGuard --media-pulse    # {"video":false,"camera":true}, exit 0 while a session is live
ThermalGuard --accel-check    # every watch app + its usage, exit 3 if any is off
ThermalGuard --accel-fix      # repairs what is owed, holding during a live session
ThermalGuard --chrome-check    # Chrome background guard state and intended action
ThermalGuard --memory-check    # pressure, RAM plan, idle-apps, plus source (daemon/local) and helper_stale
ThermalGuard --codec-check     # VideoToolbox H.264/HEVC capability and verdict
ThermalGuard --effects-check   # reduce-motion/transparency/animation switches and their values
ThermalGuard --ram-log <n>     # sample temp + RAM churn every 2 s for <n> seconds (1–7200), read-only
```

### Calls and video are protected (automatic)

Detected automatically — no setup: browsers (Chrome, Safari, Firefox, Edge),
call apps (Teams, Zoom, Meet, Webex, Skype, Discord, Slack, FaceTime,
RingCentral, BlueJeans, GoToMeeting, Jitsi), local players (QuickTime, IINA,
VLC, mpv, OBS) and the encoders/decoders behind them (VTDecoder/VTEncoder,
avconference, VideoToolbox). It runs on every tick, so opening a call or a
video file is shielded the moment it starts.

While one of those is genuinely busy (≥ 6 % CPU), ThermalGuard:

- refuses to demote **any** browser/media process, even in Max Cool,
- releases the power cap entirely unless the chip reaches 84 °C, and
- hands the fan back to the SMC, so "Silent" never costs you a call.

The behaviour is on by default and can be switched off in the UI ("Shield
video & call apps"). The choice is persisted across reboots.

An unreadable temperature is treated as "keep the cap" — relaxing blindly is
never the safe choice. Detection and the guard are unit-tested, and were
exercised end to end against the live daemon with a stand-in media process.

### Fan preference (independent of mode)

| Preference | What it does |
|---|---|
| **SMC curve** (default) | Leaves the fan entirely to Apple's firmware; asks for `F0Md = 0` if we ever held it |
| **Silent** | Asks the SMC for 18 % of its own min→max range, and — because less airflow means less heat removed — clamps power 6 °C earlier and demotes at ≥ 70 % instead of 80 %. Released back to the SMC above 88 °C, re-applied below 80 °C |
| **Maximum** | Pins the fan at the SMC's reported maximum |

A refused fan write is retried at most once every 2 minutes (not every tick),
and the app then relies on heat reduction alone. On MacBookAir9,1 the firmware
ignores `F0Tg` writes: asked for 3654 RPM it reads back 8000 RPM. Measured
side-by-side: pinning the power cap dropped the die from 74.6 °C to 65.8 °C in
20 s and the fan still read ~8000 RPM — so on this chassis the fan is driven by
something other than CPU die temperature and cannot be slowed from userspace.

## Download

Every release is published as a **GitHub Release**, and the current installer
plus its checksums are *also committed* at the repository root so the DMG can be
cloned offline:

<https://github.com/procoder317/Thermal-Guard-For-Mac/releases/latest>

That means three checksummed files are tracked in git, refreshed on every version
bump: `ThermalGuard-<version>.dmg`, `SHA256SUMS` and `MD5SUMS`. Older installers
are removed from the tree (they stay in git history and in their release). Verify
the download before opening it:

```bash
cd ~/Downloads
shasum -a 256 -c SHA256SUMS     # or: shasum -a 256 ThermalGuard-1.9.1.dmg
md5 -c MD5SUMS                  # or: md5 -r ThermalGuard-1.9.1.dmg
```

### Updating the helper

The app and its privileged helper are installed separately: a DMG install
replaces `ThermalGuard.app`, while launchd keeps running the helper it loaded at
boot. A freshly installed app can therefore be talking to an older
`thermalguardd`. The app compares `daemon_version` with its own bundle version
and, on a mismatch, shows **“Thermal helper is out of date”** with an **Update
Thermal Helper…** button (also in the “More” menu) — one admin prompt reloads
it. The app never asks for a password on its own.

Nothing else depends on that update. The daemon is the single source of truth
for memory, and the UI trusts a daemon sample only when it carries a VM total;
if the running helper predates those fields, the app reads memory itself rather
than rendering a zeroed card as “unreadable”. `--memory-check` reports which
path was taken (`source`) and whether the running helper is `helper_stale`.

## Local setup

Requirements: macOS 13+, Xcode Command Line Tools (`xcode-select --install`)
and a Rust toolchain. The app is built for `x86_64-apple-darwin` (Intel).

```bash
git clone git@github.com:procoder317/Thermal-Guard-For-Mac.git
cd Thermal-Guard-For-Mac

# only if rustup is not installed yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add x86_64-apple-darwin

bash scripts/build.sh     # -> build/ThermalGuard.app
open build/ThermalGuard.app
```

`build/`, `*.dmg` and the checksum files are git-ignored: they are build
outputs, not sources.

## Build

| Command | Output |
|---|---|
| `bash scripts/build.sh` | `build/ThermalGuard.app` — Rust daemon + SwiftUI UI, ad-hoc signed with hardened runtime |
| `bash scripts/make_dmg.sh` | `ThermalGuard-<version>.dmg` — version read from `app/Info.plist`, so the file name cannot drift |
| `bash scripts/checksums.sh` | `MD5SUMS` and `SHA256SUMS` covering every `.dmg` in the repo root |

Typical release build, from a clean checkout:

```bash
bash scripts/build.sh && bash scripts/make_dmg.sh && bash scripts/checksums.sh
```

Publishing is tag-driven (`.github/workflows/release.yml`). Bump
`app/Info.plist`, then:

```bash
git tag -a v1.9.1 -m "ThermalGuard 1.9.1"   # the tag message becomes the release notes
git push origin v1.9.1
```

CI runs the test suite, rebuilds from that commit, and attaches the DMG,
`MD5SUMS` and `SHA256SUMS` to the release. Nothing binary is committed.

## Tests

```bash
cd daemon
cargo test              # 115 unit + 33 integration (IPC) tests
cargo test --release    # same, optimised build
cargo run --release -- --memory-probe   # one Mach VM sample + µs per sample
cargo run --release -- --ram-plan       # live churn + the ladder decision for every mode
```

### Idle cost

Measured on MacBookAir9,1 over a 60 s window with `ps`: the daemon uses about
0.3 s CPU per minute (≈0.5 % of one core) and 2.6 MB RSS. Temperature, fan and
SMC readings are sampled every 2 s; `pmset` readings are cached (6 s for the
speed limit, 30 s for AC), the process scan runs every 4 s, the ioreg battery
fallback every 15 s, memory is a tens-of-µs Mach call every 2 s, the network probe
every 30 s, and the state file is rewritten every 10 s. Log timestamps no longer
spawn `date`. The UI redraws every 2 s but only forks for its guards on 8–15 s
cadences, and checks for the helper every 30 s.

What the suite covers: policy hysteresis and mode switching, the
never-demote/protected-process rules, the curated Apple-service allowlist,
launchd path validation (no caller-supplied paths), socket permissions,
oversized/truncated requests, handler-panic isolation, label sanitisation, the
Core Audio watchdog's rate limits and SIGTERM-only reset path (against a stub
`killall`, never the real engine), the RAM ladder (`ram::*`: thermal bands,
churn math, per-mode tolerances, every brake and every escalation), and the
full JSON contract the UI depends on.

The Swift side adds 84 unit tests over the acceleration, Chrome, memory, codec
and reduced-effects decision tables (`ThermalGuard --self-test`), plus read-only
headless checks (`--chrome-check`, `--memory-check`, `--codec-check`,
`--effects-check`). One of those tests runs the defaults write / read-back /
restore loop against a scratch preference domain, so the plumbing is covered
without touching a real system setting.

## Security testing

`SECURITY.md` is the threat model: every finding, its fix and the test that
locks the fix in. To re-check the guarantees locally:

1. **Automated** — `cd daemon && cargo test --release`; the security-relevant
   cases are `caller_supplied_path_cannot_bootstrap_an_arbitrary_plist`,
   `arbitrary_apple_service_cannot_be_toggled`,
   `labels_with_control_characters_are_neutralised`,
   `oversized_request_is_truncated_and_daemon_survives`,
   `handler_panics_do_not_take_the_engine_down` and
   `socket_is_reachable_before_login` and the peer-authorization unit test.
2. **Socket exposure** — after installing the helper,
   `ls -l /var/run/thermalguard.sock` shows a root-owned socket; connecting as
   system-service UIDs below 500 must return
   `{"error":"unauthorised"}` and be logged.
3. **Privilege boundary** — the UI never runs as root: only the daemon does,
   and it accepts a service *label*, resolving and re-validating the plist path
   itself (never a caller-supplied path).
4. **No network** — `lsof -i -n -P | grep thermalguardd` prints nothing; the
   daemon performs no network I/O and needs no entitlements to do so.
5. **Static review** — `grep -rn 'sh -c\|Command::new("' daemon/src` must show
   absolute-path argv arrays only, never a shell string.

## Install

1. Open the DMG, drag **ThermalGuard** to `/Applications`.
2. First launch: right‑click → **Open** (ad‑hoc signed, not notarised).
3. Menu bar thermometer → **Install Thermal Helper…** → admin password once.
   Installs `/Library/PrivilegedHelperTools/com.thermalguard.daemon` +
   LaunchDaemon `com.thermalguard.daemon`.
4. Pick a mode. Optional: **Start ThermalGuard at login**.

Uninstall: `sudo launchctl bootout system/com.thermalguard.daemon && sudo rm -f /Library/LaunchDaemons/com.thermalguard.daemon.plist /Library/PrivilegedHelperTools/com.thermalguard.daemon`
If the background-service trimmer was active, switch it off (or select Off)
first so the services are restored; otherwise re-enable them with
`sudo launchctl enable gui/$(id -u)/<label>` for each affected label.

## Manual wins the app deliberately does not force

Small wins that are worth doing by hand:

- Spotlight → System Settings → Spotlight → *Search Privacy*: add the same
  build folders (`mdutil -a -i off` disables indexing globally — reversible, but
  too blunt to do automatically).
- Avoid the worst‑case combo: charging + external display + video call. The UI
  shows AC state and battery temperature so you can see it happening.
- `pmset -g therm` → if `CPU_Speed_Limit` drops below 100, the OS is already
  throttling; that is the signal this app is designed to prevent.

## Design notes (LLD)

- One privileged daemon, one unprivileged UI. IPC = newline‑delimited JSON over
  `/var/run/thermalguard.sock` (mode 0666, root‑owned service). Per-user
  preferences (animations, transparency) are therefore written by the UI only;
  the root daemon never needs to reach into a user's defaults.
- Every accepted setting (mode, fan, shields, trimmer, reclaim) is written to
  `state.json` immediately, not on the 10 s housekeeping tick, so a shutdown or
  crash seconds after a change can never resurrect an older mode.
- `policy.rs`, `trim.rs`, `audio.rs`, `memory.rs`, `ram.rs` and `net.rs` are
  pure functions; all side effects live in `system.rs` / `procs.rs`, so every
  decision (heat, audio reset, reclaim, network verdict) is testable without
  hardware, pressure or a live link.
- `memory.rs` measures (pressure band, trend, thresholds); `ram.rs` decides
  (the ladder). Splitting them means a new signal is one struct field in
  `ram::Inputs`, and a new rung is one early return with its own `Reason`
  variant — the UI gets the explanation for free instead of parsing numbers.
  `Tuning::for_mode` is a table, the same shape as `policy::Tuning`.
- The governor is fed **deltas**, not totals: `ram::Churn::between` turns two
  cumulative Mach counter samples into faults/s, and a counter that goes
  backwards (reboot, garbage read) yields no churn rather than a fake spike.
- The memory sampler is the one hot path written like driver code: direct Mach
  and `sysctlbyname` FFI, no process spawns, tens of microseconds per sample.
  It only ever **reads** kernel counters; the VM subsystem itself stays
  Apple's. The UI no longer forks `vm_stat`/`sysctl` at all while the helper is
  answering.
- Hysteresis on the cap (82 → 72 °C) prevents mode flapping.
- Every write is verified: fan target by read‑back, power cap by re‑reading
  `pmset -g custom`, demotion by return value. Failures are logged, not hidden.
- `visualEffectCatalog` is a data table, not a chain of conditionals: adding a
  fourth switch is one entry, and it automatically gets the record/restore
  handling. `effectChange` is pure; only `reconcileVisualEffects` writes.
- Log: `/var/log/thermalguard.log`; state: `/var/run/thermalguard/state.json`.

### Decision layer

`policy.rs` is deliberately a deterministic state machine — no model, no
network call, no randomness — so the same sensors always produce the same
action and every rule can be tested without hardware.

| Input | Auto | Balanced | Cool & Quiet / Max Cool |
|---|---|---|---|
| temperature ≥ engage (°C) | cap | cap | cap |
| warm **and** `CPU_Speed_Limit < 95` | cap | — | cap |
| temperature ≤ release (°C) **and** dwell elapsed | release | release | — |
| temperature unreadable / non‑finite | hold current state | hold current state | cap |
| OS throttling alone, chip cool | no cap | no cap | cap |

Invariants the code and tests enforce:

- **Releasing protection needs positive evidence.** A missing, `NaN` or infinite
  reading is never treated as "cool"; the cap is held.
- **Throttling is an engage signal only.** It is never required to release, so
  a machine whose `CPU_Speed_Limit` stays below 100 cannot deadlock capped.
- **Every transition has a dwell time** (20 s Auto, 10 s Balanced) measured
  from when the cap engaged, so a stray sample cannot flap turbo on and off.
- **Silent fan shifts the thresholds earlier** (−6 °C, and a lower demote bar
  in explicit cooling modes) rather
  than overriding safety, and is handed back to the SMC above 88 °C.
- **Media guard relaxes only the extra clamping** — never the safety limit —
  and only on a known temperature below 84 °C.
- Demotion only ever touches *unprotected* processes and is reversible; nothing
  is killed or suspended. The Chrome guard is the only path that sends a
  signal, and only SIGTERM to a Chrome helper whose executable path was
  re-verified immediately beforehand.

## Known limits

- No fan RPM *control* on MacBookAir9,1 — hardware, not software. The app tells
  you, and reduces the heat that makes the fan spin.
- Ad‑hoc signed: Gatekeeper requires right‑click → Open the first time.
- Intel build (`x86_64`) — correct for this machine; Apple Silicon needs a
  native rebuild with different sensors.
# Thermal-Guard-For-Mac
# Thermal-Guard-For-Mac
