# ThermalGuard 1.9.1

Memory is now managed as a **heat** problem, the memory card reports real
numbers again, and a new app can update the privileged helper it inherits from
an older install.

## Thermal-aware RAM governor

Compressor churn and swap-in are CPU and disk work that turns straight into die
temperature, so the daemon treats memory pressure as a thermal signal as well as
a latency one. Every 2 s it reads the kernel's own counters (`compressions`,
`decompressions`, `swapins`, `swapouts`) and turns the deltas into a churn rate,
with a 20 s trend so a machine climbing into pressure is visible before it is
critical. `memory.rs` measures; `ram.rs` decides.

The ladder is explicit, and the daemon stops at the first rung that is both safe
and justified:

**drop reclaimable cache → ask an idle app to quit** — never a force-kill.

- **Cache first, always.** A purge is one bounded disk burst with no
  user-visible loss. Under pressure it runs before any app is touched, and it
  also runs pre-emptively when the compressor is climbing (200 pages/s, or
  +8 MB/s) while the chip still has thermal headroom.
- **Heat is a veto, not a trigger.** At 85 °C or above a purge is deferred
  unless pressure is critical or the compressor is already burning more CPU than
  the purge would. A hot chip does not by itself justify quitting an app: it
  must also be churning, so a hot machine with quiet memory is left alone.
- **A futile purge demotes itself.** The result is measured in megabytes freed;
  under 128 MB and the purge is not repeated for 15 minutes, and the ladder
  escalates to an app instead of burning the same I/O twice.
- **Every brake still applies.** A purge needs at least 1 GB of provably
  reclaimable cache, swap below 70 %, an available budget, no caller input, an
  empty argv and an absolute path. A failed purge counts against the cooldown,
  so a broken `purge` can never respawn every tick.
- **No loops.** One action per tick, with the mode's own cooldown and hourly cap.

### Per-mode tolerances

| Mode | Purge cooldown | Purges/hour | Asks an idle app to quit when | Chip must also be |
|------|----------------|-------------|-------------------------------|-------------------|
| **Auto** (default) | 30 min | 2 | pressure is critical | hot (≥ 78 °C) **and** churning |
| **Balanced** | 30 min | 2 | pressure is critical | hot (≥ 78 °C) **and** churning |
| **Cool & Quiet** | 15 min | 3 | pressure is high | warm (≥ 70 °C) **and** churning |
| **Max Cool** | 7.5 min | 4 | pressure is high | warm (≥ 70 °C) **and** churning |
| **Off** | — | — | never — monitoring only | — |

Cache is dropped before any app in every mode, and a machine that is merely
comfortable is never acted on, including in Max Cool. Quit requests remain
graceful (`terminate()` only); a refusal is backed off for 30 minutes.

### New in the status contract and CLI

The daemon is the single decision-maker and publishes the rung
(`memory_plan`), its reason (`memory_plan_reason`), a sentence for the UI
(`memory_plan_detail`), the thermal band (`memory_band`), the churn rate
(`memory_heat_load`, `memory_churn_decomp_s`, `memory_churn_swapin_s`) and the
VM totals (`memory_total_mb`, `memory_available_mb`, `memory_reclaimable_mb`,
swap and compressed figures). The unprivileged UI executes only the surviving
`trim` rung.

`ThermalGuard --ram-log <n>` samples die temperature and RAM churn every 2 s for
`<n>` seconds (1–7200), read-only, so the thresholds can be checked against this
machine instead of trusted.

## Memory card fixed: no more “unreadable” or “Free 1 %”

The card could show `Free 2 %`, blank swap/compressed values and “unreadable”
even while the machine was healthy. Two causes, both fixed:

- **The card trusted the wrong sample.** It only accepted the daemon's numbers
  when a `memory_pressure` string parsed, and otherwise fell back to its own
  `vm_stat` read. With a helper that predates the VM fields, it rendered zeros
  as “unreadable”.
- **“Free” was raw free pages.** macOS deliberately keeps free pages near zero,
  so a healthy Mac looked out of memory.

The UI now trusts a daemon sample only when it carries a VM total, and otherwise
reads memory itself; “Free” means **free + reclaimable** (purgeable and
file-backed) memory, which is what the kernel can hand out without swapping. The
card also shows the governor's own sentence and the compressor churn line, so the
numbers and the decision can no longer tell different stories. While the helper
answers, the UI no longer forks `vm_stat`/`sysctl` at all.

`--memory-check` now reports the path taken (`source`: `daemon` or `local`), the
running helper's version, and whether the helper is stale.

## Updating the privileged helper after a new DMG

The app and its root helper are installed separately: a DMG replaces
`ThermalGuard.app`, while launchd keeps running the helper it loaded at boot. A
freshly installed app could therefore talk to an older `thermalguardd` — which
is exactly what produced the missing memory fields above.

- The app compares the helper's reported `daemon_version` with its own bundle
  version. A mismatch — including a helper too old to report a version at all —
  is treated as stale.
- The popup shows **“Thermal helper is out of date (older build)”** with an
  **Update Thermal Helper…** button, also available from the **More (⋯)** menu.
- The button runs the bundled installer: it replaces the helper binary, reloads
  the launchd job and verifies that the job came back. One admin prompt total.
- The update is **always a click**; the app never asks for a password on its
  own. A second install cannot start while one is running, and the banner clears
  once the reloaded helper answers.
- Nothing breaks in the meantime: a stale helper degrades to the local memory
  read instead of a broken card.

## Testing

- Daemon: **115 unit tests + 33 IPC integration tests** (`cd daemon && cargo test`).
  The RAM ladder is covered in `ram::*` (thermal bands, churn math, per-mode
  tolerances, every brake and escalation).
- App: **84 self-tests** (`ThermalGuard --self-test`), including the memory
  snapshot resolution and helper-version comparison.
- Headless checks: `--memory-check`, `--ram-plan`, `--ram-log <n>`.
- The power-cap IPC tests now switch the media shield off through the daemon's
  own IPC before asserting the cap, so a real video call on the test machine
  cannot make them measure the shield instead of the power policy.

## Install

Download `ThermalGuard-1.9.1.dmg`, verify it against `SHA256SUMS` (or `MD5SUMS`),
then drag the app into `/Applications`. After upgrading, click **Update Thermal
Helper…** in the popup — that step is what actually delivers the new daemon
(the RAM governor and the memory fields live there).

## Known limits

- The helper update requires an admin prompt; it cannot be silent by design.
- The governor never force-kills anything; an app that refuses to quit is left
  alone and backed off for 30 minutes.
- One purge is a bounded I/O burst, not a background process; its cooldown and
  hourly cap are per mode and cannot be raised from the UI or IPC.
