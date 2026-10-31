# ThermalGuard

Menu-bar app for **MacBook Air 9,1 (2020, Intel i5‑1030NG7, T2)** that stops the
sustained overheating / fan-blast cycle at its source.

Built as: **Rust** privileged thermal core (`thermalguardd`) + **Swift/AppKit**
menu-bar UI + `.dmg` installer. No third‑party crates, no kexts, **no SIP
changes**.

---

## Root causes on this exact model

Verified on the target machine (`MacBookAir9,1`, macOS 15.7.5):

| # | Cause | Evidence | Handled by |
|---|-------|----------|-----------|
| 1 | **Package power exceeds what the chassis dissipates.** 10 W‑class Ice Lake chip allowed to boost to 3.5 GHz (ratio 35) into a ~10–13 W chassis. | `machdep.xcpm.soft_plimit_max_100mhz_ratio: 35` vs `lpm_plimit_max: 24` | Power cap (`pmset lowpowermode`) → ceiling drops to 2.4 GHz |
| 2 | **Fan is not thermally coupled to the heat pipe.** The SMC fan curve is acoustic‑first and has no user‑addressable target. So "fans at full blast" ≠ "cooler". | SMC `F0Ac` readable; `F0Tg` write is verified by read‑back and reported as unsupported when ignored | App detects this and says so instead of pretending |
| 3 | **Constant background CPU bursts** keep the chip in boost so it never idles. | Live: load avg 5.00 / 8 threads, `XprotectService` 95 % CPU | Reversible QoS demotion (Darwin background class) |
| 4 | **Security + vendor agents** (Bitdefender ES system extension, Wacom, Epson) are resident and always expensive. | `systemextensionsctl list` → `com.bitdefender.agentui.sp` active | **Explicitly protected — never throttled** |
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
- **Never touches:** Bitdefender (`bitdefender`, `bdservicehost`,
  `vshieldscanner`, `trufos`), Wacom (`wacom`), XProtect, `syspolicyd`,
  `securityd`, MDM, and the window server / kernel.
- Reports honestly when a fan target is ignored (MacBookAir9,1).

### Modes

| Mode | Power cap | Demote threshold |
|------|-----------|------------------|
| **Auto** (default) | only above 78 °C (or when macOS starts throttling), released below 74 °C with 20 s of hysteresis | ≥ 80 % CPU |
| Off | never | – |
| Balanced | above 82 °C, release at 72 °C | ≥ 70 % CPU |
| Cool & Quiet | always | ≥ 45 % CPU |
| Max Cool | always | ≥ 30 % CPU |

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
ThermalGuard --self-test      # 8 unit tests over the decision table
ThermalGuard --media-pulse    # {"video":false,"camera":true}, exit 0 while a session is live
ThermalGuard --accel-check    # every watch app + its usage, exit 3 if any is off
ThermalGuard --accel-fix      # repairs what is owed, holding during a live session
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

## Build

```bash
bash scripts/build.sh      # -> build/ThermalGuard.app
bash scripts/make_dmg.sh   # -> ThermalGuard-1.1.0.dmg
```

Requires `cargo` (rustup, `x86_64-apple-darwin`) and Command Line Tools.

## Install

1. Open the DMG, drag **ThermalGuard** to `/Applications`.
2. First launch: right‑click → **Open** (ad‑hoc signed, not notarised).
3. Menu bar thermometer → **Install Thermal Helper…** → admin password once.
   Installs `/Library/PrivilegedHelperTools/com.thermalguard.daemon` +
   LaunchDaemon `com.thermalguard.daemon`.
4. Pick a mode. Optional: **Start ThermalGuard at login**.

Uninstall: `sudo launchctl bootout system/com.thermalguard.daemon && sudo rm -f /Library/LaunchDaemons/com.thermalguard.daemon.plist /Library/PrivilegedHelperTools/com.thermalguard.daemon`

## Manual wins the app deliberately does not force

These keep Bitdefender and Wacom fully functional:

- Bitdefender → *Protection → Scan schedule*: one nightly scan, and add build /
  node_modules / VM folders to exclusions. On‑access scanning of huge trees is
  the biggest hidden CPU cost on this chip.
- Spotlight → System Settings → Spotlight → *Search Privacy*: add the same
  build folders (`mdutil -a -i off` disables indexing globally — reversible, but
  too blunt to do automatically).
- Wacom → keep the driver; it is protected here. If the tablet is unused for
  weeks, quit **Wacom Center** (the driver keeps working without it).
- Avoid the worst‑case combo: charging + external display + video call. The UI
  shows AC state and battery temperature so you can see it happening.
- `pmset -g therm` → if `CPU_Speed_Limit` drops below 100, the OS is already
  throttling; that is the signal this app is designed to prevent.

## Design notes (LLD)

- One privileged daemon, one unprivileged UI. IPC = newline‑delimited JSON over
  `/var/run/thermalguard.sock` (mode 0666, root‑owned service).
- `policy.rs` is a pure function; all side effects live in `system.rs` /
  `procs.rs`, so the decision logic is testable without hardware.
- Hysteresis on the cap (82 → 72 °C) prevents mode flapping.
- Every write is verified: fan target by read‑back, power cap by re‑reading
  `pmset -g custom`, demotion by return value. Failures are logged, not hidden.
- Log: `/var/log/thermalguard.log`; state: `/var/run/thermalguard/state.json`.

## Known limits

- No fan RPM *control* on MacBookAir9,1 — hardware, not software. The app tells
  you, and reduces the heat that makes the fan spin.
- Ad‑hoc signed: Gatekeeper requires right‑click → Open the first time.
- Intel build (`x86_64`) — correct for this machine; Apple Silicon needs a
  native rebuild with different sensors.
# Thermal-Guard-For-Mac
# Thermal-Guard-For-Mac
