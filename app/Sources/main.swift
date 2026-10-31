import AppKit
import Combine
import CoreMediaIO
import Foundation
import SwiftUI

let SOCK_PATH = "/var/run/thermalguard.sock"
let HELPER_LABEL = "com.thermalguard.daemon"

// MARK: - IPC with the privileged daemon

func sendRequest(_ payload: String) -> String? {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    if fd < 0 { return nil }
    defer { close(fd) }

    // Never let a stalled daemon freeze the UI's 2 s refresh loop.
    var tv = timeval(tv_sec: 3, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let pathBytes = Array(SOCK_PATH.utf8)
    withUnsafeMutableBytes(of: &addr.sun_path) { raw in
        for i in 0..<min(pathBytes.count, raw.count - 1) {
            raw[i] = pathBytes[i]
        }
        raw[min(pathBytes.count, raw.count - 1)] = 0
    }

    let connected = withUnsafePointer(to: &addr) { ptr -> Bool in
        ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
            connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size)) == 0
        }
    }
    if !connected { return nil }

    var out = Array((payload + "\n").utf8)
    let wrote = out.withUnsafeMutableBytes { buf -> Bool in
        write(fd, buf.baseAddress, buf.count) == buf.count
    }
    if !wrote { return nil }

    var data = Data()
    var buf = [UInt8](repeating: 0, count: 65536)
    while data.count < 262_144 {
        let n = read(fd, &buf, buf.count)
        if n <= 0 { break }
        data.append(contentsOf: buf[0..<n])
    }
    return String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines)
}

func requestStatus() -> [String: Any]? {
    guard let raw = sendRequest("{\"cmd\":\"status\"}"), let d = raw.data(using: .utf8) else { return nil }
    return (try? JSONSerialization.jsonObject(with: d)) as? [String: Any]
}

func sendMode(_ value: String) {
    _ = sendRequest("{\"cmd\":\"mode\",\"value\":\"\(value)\"}")
}

func sendFan(_ value: String) {
    _ = sendRequest("{\"cmd\":\"fan\",\"value\":\"\(value)\"}")
}

// MARK: - shell helper

@discardableResult
func shell(_ path: String, _ args: [String]) -> (Int32, String) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: path)
    p.arguments = args
    let pipe = Pipe()
    p.standardOutput = pipe
    p.standardError = pipe
    do { try p.run() } catch { return (-1, "\(error)") }
    p.waitUntilExit()
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    return (p.terminationStatus, String(data: data, encoding: .utf8) ?? "")
}

func helperInstalled() -> Bool {
    let out = shell("/bin/launchctl", ["print", "system/\(HELPER_LABEL)"]).1
    return !out.contains("Could not find service")
}

// MARK: - Hardware-acceleration guard for video & call apps
//
// Chromium-family browsers keep "Use hardware acceleration when available" in
// their own Local State file, and an update can put it back to
// `enabled: false`. With acceleration off, a 1.1 GHz Intel Air decodes video on
// the CPU, both cores saturate, the chip heats and playback hangs.
//
// ThermalGuard watches every installed Chromium browser and repairs the
// setting, but only while the app is NOT in use: the instant a call or video
// is detected we stop (no config writes, no restarts mid-call) and resume as
// soon as it goes idle. Calls apps that ship no such toggle (Zoom, Teams,
// FaceTime, Safari) always decode through VideoToolbox, so for them the only
// lever is the media shield in the daemon — the guard reports that instead of
// pretending to fix a setting that does not exist.

struct AcceleratedApp {
    let name: String
    let bundleID: String
    let appPath: String
    /// Path below the home folder.
    let statePath: String
    /// How this app's processes appear in `ps -Ao comm=`.
    let processTag: String
    /// The GPU helper, where a *hardware*-decoded video does its work.
    let gpuHelperTag: String
}

let acceleratedApps: [AcceleratedApp] = [
    .init(name: "Google Chrome", bundleID: "com.google.Chrome",
          appPath: "/Applications/Google Chrome.app",
          statePath: "Library/Application Support/Google/Chrome/Local State",
          processTag: "Google Chrome", gpuHelperTag: "Google Chrome Helper (GPU)"),
    .init(name: "Microsoft Edge", bundleID: "com.microsoft.edgemac",
          appPath: "/Applications/Microsoft Edge.app",
          statePath: "Library/Application Support/Microsoft Edge/Local State",
          processTag: "Microsoft Edge", gpuHelperTag: "Microsoft Edge Helper (GPU)"),
    .init(name: "Brave Browser", bundleID: "com.brave.Browser",
          appPath: "/Applications/Brave Browser.app",
          statePath: "Library/Application Support/BraveSoftware/Brave-Browser/Local State",
          processTag: "Brave Browser", gpuHelperTag: "Brave Browser Helper (GPU)"),
    .init(name: "Vivaldi", bundleID: "com.vivaldi.Vivaldi",
          appPath: "/Applications/Vivaldi.app",
          statePath: "Library/Application Support/Vivaldi/Local State",
          processTag: "Vivaldi", gpuHelperTag: "Vivaldi Helper (GPU)"),
    .init(name: "Chromium", bundleID: "org.chromium.Chromium",
          appPath: "/Applications/Chromium.app",
          statePath: "Library/Application Support/Chromium/Local State",
          processTag: "Chromium", gpuHelperTag: "Chromium Helper (GPU)"),
]

/// What a video/call app is doing right now. `inUse` means its own processes
/// are busy, i.e. something is actually being decoded — the moment we must not
/// restart it or write its settings.
enum AppUsage: String {
    case closed, idle, inUse
}

struct AccelStatus {
    var app: AcceleratedApp
    var installed = false
    var running = false
    var enabled = true
    var readable = true
    var usage: AppUsage = .closed
    var cpu: Double = 0
}

/// Threshold that separates "sitting there with a tab open" from "decoding".
let APP_IN_USE_CPU: Double = 8.0

func appURL(_ app: AcceleratedApp) -> URL? {
    FileManager.default.fileExists(atPath: app.appPath) ? URL(fileURLWithPath: app.appPath) : nil
}

func appStateURL(_ app: AcceleratedApp) -> URL {
    FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(app.statePath)
}

func appRunning(_ app: AcceleratedApp) -> Bool {
    !NSRunningApplication.runningApplications(withBundleIdentifier: app.bundleID).isEmpty
}

/// One `ps` sample for the whole guard pass: five apps must not cost five forks.
func cpuSample() -> String {
    shell("/bin/ps", ["-Ao", "pcpu=,comm="]).1
}

func cpuFor(_ tag: String, in sample: String) -> Double {
    var total = 0.0
    for line in sample.split(separator: "\n") {
        let parts = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true)
        guard parts.count == 2, parts[1].contains(tag) else { continue }
        total += Double(parts[0]) ?? 0
    }
    return total
}

func accelStatus(_ app: AcceleratedApp, sample: String) -> AccelStatus {
    var s = AccelStatus(app: app)
    s.installed = appURL(app) != nil
    guard s.installed else { return s }
    s.running = appRunning(app)
    s.cpu = s.running ? cpuFor(app.processTag, in: sample) : 0
    s.usage = !s.running ? .closed : (s.cpu >= APP_IN_USE_CPU ? .inUse : .idle)
    guard let data = try? Data(contentsOf: appStateURL(app)),
          let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        s.readable = false
        return s
    }
    if let mode = obj["hardware_acceleration_mode"] as? [String: Any],
       let on = mode["enabled"] as? Bool {
        s.enabled = on
    }
    return s
}

// MARK: - Is real media actually running?
//
// "The browser is open" is not the same as "a video is playing". The guard
// only acts on the real thing: a decode pipeline that is working, or a camera
// that is powered on somewhere in the system. Both are read without asking for
// any permission, and both are cheap.

struct MediaPulse {
    var video = false
    var camera = false
    var active: Bool { video || camera }
}

/// Processes that only burn CPU when a decode/encode pipeline is really running.
let videoPipelineTags = ["VTDecoderXPCService", "VTEncoderXPCService", "videotoolbox", "AVConference"]
let VIDEO_DECODE_CPU: Double = 2.0

/// `kCMIODevicePropertyDeviceIsRunningSomewhere` is the system's own "is the
/// camera on" flag. It needs no TCC grant, so asking it never prompts the user.
func cameraInUse() -> Bool {
    var devicesAddr = CMIOObjectPropertyAddress(
        mSelector: CMIOObjectPropertySelector(kCMIOHardwarePropertyDevices),
        mScope: CMIOObjectPropertyScope(kCMIOObjectPropertyScopeGlobal),
        mElement: CMIOObjectPropertyElement(kCMIOObjectPropertyElementMain))
    var size: UInt32 = 0
    guard CMIOObjectGetPropertyDataSize(CMIOObjectID(kCMIOObjectSystemObject), &devicesAddr,
                                        0, nil, &size) == noErr, size > 0 else { return false }
    let count = Int(size) / MemoryLayout<CMIODeviceID>.size
    var devices = [CMIODeviceID](repeating: 0, count: max(count, 1))
    guard CMIOObjectGetPropertyData(CMIOObjectID(kCMIOObjectSystemObject), &devicesAddr,
                                    0, nil, size, &size, &devices) == noErr else { return false }

    for id in devices.prefix(count) {
        var running: UInt32 = 0
        var addr = CMIOObjectPropertyAddress(
            mSelector: CMIOObjectPropertySelector(kCMIODevicePropertyDeviceIsRunningSomewhere),
            mScope: CMIOObjectPropertyScope(kCMIOObjectPropertyScopeGlobal),
            mElement: CMIOObjectPropertyElement(kCMIOObjectPropertyElementMain))
        var len = UInt32(MemoryLayout<UInt32>.size)
        if CMIOObjectGetPropertyData(id, &addr, 0, nil, len, &len, &running) == noErr, running != 0 {
            return true
        }
    }
    return false
}

func mediaPulse(sample: String) -> MediaPulse {
    var p = MediaPulse()
    p.video = videoPipelineTags.contains { cpuFor($0, in: sample) >= VIDEO_DECODE_CPU }
        // A Chromium GPU helper doing real work means a frame is being decoded.
        || acceleratedApps.contains { cpuFor($0.gpuHelperTag, in: sample) >= APP_IN_USE_CPU }
    p.camera = cameraInUse()
    return p
}

// MARK: - The realtime decision

enum AccelAction: Equatable {
    case nothing
    case enableNow(relaunch: Bool)
    case hold(String)
}

/// Pure function: every input is data, so the whole policy is inspectable.
///
/// `pending` means a real video or camera session was seen while acceleration
/// was off, so the repair is owed. Nothing happens until that is true — the
/// guard never restarts a browser just because it is open.
func accelAction(_ s: AccelStatus, pulse: MediaPulse, pending: Bool) -> AccelAction {
    guard s.installed else { return .nothing }
    guard s.readable else { return .hold("settings readable once it quits") }
    guard !s.enabled else { return .nothing }

    // A live session right now: hold. Restarting a decoder mid-call costs more
    // than the heat it saves, so the repair waits for the call to end.
    if pulse.active { return .hold("live video/call in progress") }
    guard pending else { return .nothing }
    return .enableNow(relaunch: s.running)
}

/// Quit the app if needed, re-enable acceleration, drop the stale GPU cache and
/// relaunch. Only ever touches the app's own config; nothing here needs root.
@discardableResult
func enableAcceleration(_ app: AcceleratedApp, relaunch: Bool) -> (Bool, String) {
    guard appURL(app) != nil else { return (false, "\(app.name) is not installed.") }

    let wasRunning = appRunning(app)
    if wasRunning {
        for instance in NSRunningApplication.runningApplications(withBundleIdentifier: app.bundleID) {
            instance.terminate()
        }
        for _ in 0..<60 where appRunning(app) {
            Thread.sleep(forTimeInterval: 0.1)
        }
        if appRunning(app) {
            return (false, "\(app.name) did not quit; close it and try again.")
        }
    }

    let url = appStateURL(app)
    guard let data = try? Data(contentsOf: url),
          var obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        return (false, "\(app.name)'s settings file could not be read.")
    }
    obj["hardware_acceleration_mode"] = ["enabled": true]
    obj["hardware_acceleration_mode_previous"] = true
    guard let out = try? JSONSerialization.data(withJSONObject: obj, options: []) else {
        return (false, "\(app.name)'s settings could not be re-encoded.")
    }
    let tmp = url.appendingPathExtension("thermalguard-tmp")
    do {
        try out.write(to: tmp, options: .atomic)
        _ = try FileManager.default.replaceItemAt(url, withItemAt: tmp)
    } catch {
        return (false, "Could not write \(app.name)'s settings: \(error.localizedDescription)")
    }

    let fm = FileManager.default
    let base = url.deletingLastPathComponent()
    for profile in ["Default", "Profile 1", "Profile 2", "System Profile"] {
        let cache = base.appendingPathComponent(profile).appendingPathComponent("GPUCache")
        if fm.fileExists(atPath: cache.path) { try? fm.removeItem(at: cache) }
    }

    if relaunch && wasRunning, let u = appURL(app) {
        NSWorkspace.shared.openApplication(at: u, configuration: NSWorkspace.OpenConfiguration()) { _, _ in }
    }
    return (true, relaunch && wasRunning ? "\(app.name) fixed and relaunched." : "\(app.name) fixed.")
}

// MARK: - Design system
//
// Everything visual lives here. The engineering is unchanged: the same IPC,
// the same acceleration guard, the same reversible software toggles. The look
// follows current macOS: system materials, SF symbols, rounded cards, one
// accent colour that follows the machine's actual thermal state.

private let cardRadius: CGFloat = 14

private extension View {
    /// A frosted card: the material lets the desktop show through the window,
    /// the hairline keeps it legible on a bright wallpaper.
    func cardSurface() -> some View {
        self
            .padding(14)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: cardRadius, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: cardRadius, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.07), lineWidth: 1)
            )
    }
}

private struct CardTitle: View {
    let icon: String
    let text: String
    var trailing: String? = nil

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: icon)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(.secondary)
            Text(text.uppercased())
                .font(.system(size: 10, weight: .semibold))
                .tracking(0.7)
                .foregroundStyle(.secondary)
            Spacer(minLength: 8)
            if let t = trailing, !t.isEmpty {
                Text(t)
                    .font(.system(size: 10))
                    .foregroundStyle(Color.secondary.opacity(0.85))
                    .lineLimit(1)
            }
        }
    }
}

private struct Chip: View {
    let text: String
    var tint: Color = .secondary
    var active = false

    var body: some View {
        Text(text)
            .font(.system(size: 10.5, weight: .medium))
            .lineLimit(1)
            .padding(.horizontal, 8)
            .padding(.vertical, 3.5)
            .background(
                Capsule().fill(active ? tint.opacity(0.16) : Color.primary.opacity(0.05))
            )
            .overlay(
                Capsule().strokeBorder(active ? tint.opacity(0.35) : Color.clear, lineWidth: 1)
            )
            .foregroundStyle(active ? tint : Color.secondary)
    }
}

private struct MetricRow: View {
    let icon: String
    let label: String
    let value: String
    var tint: Color = .primary

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: icon)
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .frame(width: 15, alignment: .center)
            Text(label)
                .font(.system(size: 11.5))
                .foregroundStyle(.secondary)
            Spacer(minLength: 6)
            Text(value)
                .font(.system(size: 11.5, weight: .medium))
                .monospacedDigit()
                .foregroundStyle(tint)
                .lineLimit(1)
        }
    }
}

/// Half-open ring that fills as the die heats up. 45-100 °C is the useful
/// span on this hardware: below 45 is idle-cold, above 100 is an emergency.
private struct TempRing: View {
    let temp: Double?
    let tint: Color

    private var fraction: Double {
        guard let t = temp else { return 0 }
        return min(max((t - 45) / 55, 0.02), 1)
    }

    var body: some View {
        ZStack {
            Circle()
                .stroke(Color.primary.opacity(0.07), lineWidth: 9)
            Circle()
                .trim(from: 0, to: fraction)
                .stroke(
                    AngularGradient(
                        gradient: Gradient(colors: [tint.opacity(0.45), tint]),
                        center: .center,
                        startAngle: .degrees(-90),
                        endAngle: .degrees(270)
                    ),
                    style: StrokeStyle(lineWidth: 9, lineCap: .round)
                )
                .rotationEffect(.degrees(-90))
                .animation(.easeInOut(duration: 0.35), value: fraction)
            VStack(spacing: -2) {
                Text(temp.map { "\(Int($0.rounded()))" } ?? "–")
                    .font(.system(size: 38, weight: .semibold, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(temp == nil ? Color.secondary : Color.primary)
                Text("°C")
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(.secondary)
            }
        }
        .frame(width: 108, height: 108)
    }
}

private struct ToggleRow: View {
    let title: String
    let subtitle: String
    let isOn: Bool
    let onChange: (Bool) -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 12, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                if !subtitle.isEmpty {
                    Text(subtitle)
                        .font(.system(size: 10))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .truncationMode(.middle)
                }
            }
            Spacer(minLength: 12)
            Toggle("", isOn: Binding(get: { isOn }, set: onChange))
                .labelsHidden()
                .toggleStyle(.switch)
                .controlSize(.mini)
        }
        .padding(.vertical, 7)
        .padding(.horizontal, 10)
        .background(Color.primary.opacity(0.03), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
    }
}

private struct ConfirmAction: Identifiable {
    let id = UUID()
    let title: String
    let message: String
    let confirmLabel: String
    let run: () -> Void
}

private struct SheetHeader: View {
    let icon: String
    let title: String
    let subtitle: String

    var body: some View {
        HStack(spacing: 11) {
            ZStack {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.accentColor.opacity(0.15))
                Image(systemName: icon)
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Color.accentColor)
            }
            .frame(width: 30, height: 30)

            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                Text(subtitle)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
            Spacer()
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
    }
}

// MARK: - Main window

private let modeValues = ["auto", "off", "balanced", "coolquiet", "maxcool"]
private let fanValues = ["auto", "quiet", "max"]

private func modeTitle(_ v: String) -> String {
    switch v {
    case "off": return "Off"
    case "balanced": return "Balanced"
    case "coolquiet": return "Cool"
    case "maxcool": return "Max"
    default: return "Auto"
    }
}

private func modeBlurb(_ v: String) -> String {
    switch v {
    case "off": return "ThermalGuard steps aside. macOS alone manages clock and fan."
    case "balanced": return "Keeps clocks up and only caps near the top of the safe range."
    case "coolquiet": return "Caps earlier and asks the fan for a gentle curve."
    case "maxcool": return "Prioritises the lowest possible temperature over silence."
    default: return "Reads the die every two seconds and caps only when it actually gets hot."
    }
}

private func fanTitle(_ v: String) -> String {
    switch v {
    case "quiet": return "Silent"
    case "max": return "Max"
    default: return "SMC curve"
    }
}

struct RootView: View {
    @ObservedObject var d: AppDelegate
    @State private var showSoftware = false
    @State private var showMacOS = false

    var body: some View {
        ZStack(alignment: .top) {
            LinearGradient(
                colors: [d.accent.opacity(0.22), d.accent.opacity(0.04), .clear],
                startPoint: .top,
                endPoint: .bottom
            )
            .frame(height: 260)
            .allowsHitTesting(false)

            ScrollView(.vertical, showsIndicators: false) {
                VStack(spacing: 11) {
                    hero
                    if !d.helperRunning { helperBanner }
                    modeCard
                    fanCard
                    guardCard
                    cpuCard
                    if !d.events.isEmpty { eventsCard }
                    footer
                }
                .padding(.horizontal, 18)
                .padding(.top, 16)
                .padding(.bottom, 18)
            }
        }
        .frame(width: 560)
        .frame(maxHeight: .infinity, alignment: .top)
        .background(Color.clear)
        .sheet(isPresented: $showSoftware) { SoftwareSheet(d: d) }
        .sheet(isPresented: $showMacOS) { MacOSSheet(d: d) }
    }

    // MARK: header

    // MARK: hero

    private var hero: some View {
        VStack(spacing: 13) {
            HStack(alignment: .center, spacing: 16) {
                TempRing(temp: d.temp, tint: d.accent)

                VStack(alignment: .leading, spacing: 9) {
                    MetricRow(icon: "fanblades",
                              label: "Fan",
                              value: d.fan.map { "\(Int($0)) RPM" } ?? "—")
                    MetricRow(icon: "speedometer",
                              label: "Clock ceiling",
                              value: d.clockText,
                              tint: d.capActive ? Color.orange : Color.primary)
                    MetricRow(icon: "battery.75",
                              label: "Battery",
                              value: d.batteryText)
                    MetricRow(icon: "memorychip",
                              label: "SMC",
                              value: d.smcOK ? "connected" : "unavailable")
                }
                Spacer(minLength: 0)
            }

            HStack(spacing: 6) {
                Chip(text: d.modeLabel, tint: d.accent, active: true)
                Chip(text: d.capActive ? "Power cap on" : "Power cap off",
                     tint: .orange, active: d.capActive)
                if let l = d.limit, l < 100 {
                    Chip(text: "macOS throttled \(l)%", tint: .red, active: true)
                }
                if d.mediaActive {
                    Chip(text: d.mediaGuard ? "Call shielded" : "Call active",
                         tint: .blue, active: true)
                }
                Spacer(minLength: 0)
            }
        }
        .cardSurface()
    }

    private var helperBanner: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 7) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 12))
                    .foregroundStyle(.orange)
                Text("Install the thermal helper to begin")
                    .font(.system(size: 12, weight: .semibold))
            }
            Text("One privileged install. It reads the SMC thermometer and applies the power cap; nothing else on this Mac is touched.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if !d.hint.isEmpty {
                Text(d.hint)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.orange)
            }
            Button {
                d.installHelper()
            } label: {
                Text("Install Thermal Helper…").frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.regular)
        }
        .cardSurface()
    }

    // MARK: controls

    private var modeCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            CardTitle(icon: "thermometer.medium", text: "Cooling mode", trailing: d.modeLabel)
            Picker("", selection: Binding(get: { d.mode }, set: { d.setMode($0) })) {
                ForEach(modeValues, id: \.self) { v in
                    Text(modeTitle(v)).tag(v)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            Text(modeBlurb(d.mode))
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .cardSurface()
    }

    private var fanCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            CardTitle(icon: "fanblades", text: "Fan", trailing: fanTitle(d.fanIntent))
            Picker("", selection: Binding(get: { d.fanIntent }, set: { d.setFan($0) })) {
                ForEach(fanValues, id: \.self) { v in
                    Text(fanTitle(v)).tag(v)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            // Never locked: the preference is always the user's to change. On
            // models whose firmware ignores fan targets the daemon simply says
            // so in fanNote, and picking "SMC curve" still hands the fan back.
            .help(d.fanSupported ? "Fan preference" : "This model's firmware ignores fan targets; the power cap does the cooling.")

            if !d.fanNote.isEmpty {
                Text(d.fanNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .cardSurface()
    }

    private var guardCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "video.badge.waveform", text: "Video & calls", trailing: d.guardTrailing)
            Toggle("Shield video & call apps", isOn: Binding(get: { d.mediaGuard }, set: { d.setMediaGuard($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            Text("While a real video or call runs, the power cap is released and nothing is demoted. Safety still wins above 84 °C.")
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if !d.accelLines.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(Array(d.accelLines.enumerated()), id: \.offset) { _, line in
                        HStack(alignment: .top, spacing: 6) {
                            Image(systemName: line.hasPrefix("Fixed") ? "checkmark.circle.fill"
                                  : (line.contains("hardware decode on") ? "checkmark.circle" : "exclamationmark.circle"))
                                .font(.system(size: 10))
                                .foregroundStyle(line.hasPrefix("Fixed") ? Color.green
                                                 : (line.contains("hardware decode on") ? Color.secondary : Color.orange))
                            Text(line)
                                .font(.system(size: 10.5))
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
                .padding(.top, 2)
            }

            if d.offerFix {
                Button {
                    d.fixAccelerationNow()
                } label: {
                    Text("Fix acceleration now…").frame(maxWidth: .infinity)
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
            }
        }
        .cardSurface()
    }

    private var cpuCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "chart.bar.fill", text: "Using the CPU",
                      trailing: d.demoted.isEmpty ? "" : "\(d.demoted.count) demoted")
            if d.procs.isEmpty {
                Text("Nothing significant is running.")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            } else {
                ForEach(Array(d.procs.prefix(5).enumerated()), id: \.offset) { _, p in
                    let name = p["name"] as? String ?? "?"
                    let cpu = p["cpu"] as? Double ?? 0
                    let pid = p["pid"] as? Int ?? 0
                    VStack(alignment: .leading, spacing: 3) {
                        HStack(spacing: 6) {
                            if d.demoted.contains(pid) {
                                Image(systemName: "arrow.down.circle.fill")
                                    .font(.system(size: 9))
                                    .foregroundStyle(.blue)
                                    .help("Demoted to background QoS — reversible, nothing is killed")
                            }
                            Text(name)
                                .font(.system(size: 11))
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Spacer(minLength: 6)
                            Text(String(format: "%.1f%%", cpu))
                                .font(.system(size: 10.5, weight: .medium))
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                        GeometryReader { geo in
                            ZStack(alignment: .leading) {
                                Capsule().fill(Color.primary.opacity(0.07))
                                Capsule()
                                    .fill(d.accent.opacity(0.75))
                                    .frame(width: max(3, geo.size.width * min(cpu / 100.0, 1)))
                            }
                        }
                        .frame(height: 4)
                    }
                    .padding(.vertical, 1)
                }
            }
        }
        .cardSurface()
    }

    private var eventsCard: some View {
        VStack(alignment: .leading, spacing: 7) {
            CardTitle(icon: "clock.arrow.circlepath", text: "Recent activity")
            ForEach(Array(d.events.suffix(4).enumerated()), id: \.offset) { _, e in
                Text(e)
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .cardSurface()
    }

    private var footer: some View {
        HStack(spacing: 8) {
            Button {
                showSoftware = true
            } label: {
                Label("Software", systemImage: "shippingbox")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)

            Button {
                showMacOS = true
            } label: {
                Label("macOS services", systemImage: "gearshape.2")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)

            Spacer(minLength: 0)

            Menu {
                Toggle("Start at login", isOn: Binding(get: { d.loginOn }, set: { d.setLogin($0) }))
                if !d.helperPresent {
                    Button("Install Thermal Helper…") { d.installHelper() }
                }
                Divider()
                Button("Quit ThermalGuard") { NSApp.terminate(nil) }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .font(.system(size: 13))
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .frame(width: 30)
            .help("More")
        }
        .padding(.top, 2)
        .padding(.horizontal, 2)
    }
}

// MARK: - Sheets

struct SoftwareSheet: View {
    @ObservedObject var d: AppDelegate
    @Environment(\.dismiss) private var dismiss
    @State private var confirm: ConfirmAction?

    var body: some View {
        VStack(spacing: 0) {
            SheetHeader(icon: "shippingbox",
                        title: "Installed software",
                        subtitle: "Third-party background items. Ticked = running. Apple services are hidden and never listed.")
            Divider()
            ScrollView {
                VStack(spacing: 6) {
                    if d.software.isEmpty {
                        Text("No third-party background items found.")
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                            .padding(.top, 20)
                    }
                    ForEach(Array(d.software.enumerated()), id: \.offset) { _, it in
                        let label = it["label"] as? String ?? "?"
                        let loaded = it["loaded"] as? Bool ?? false
                        let scope = it["scope"] as? String ?? "system"
                        ToggleRow(title: label,
                                  subtitle: "\(scope) · \(it["path"] as? String ?? "")",
                                  isOn: loaded) { on in
                            if on {
                                d.setAgent(label, enable: true)
                            } else {
                                confirm = ConfirmAction(
                                    title: "Stop \(label)?",
                                    message: "The vendor's background item is disabled until you re-tick it here. Reversible; nothing is deleted.",
                                    confirmLabel: "Disable"
                                ) { d.setAgent(label, enable: false) }
                            }
                        }
                    }
                }
                .padding(16)
            }
            Divider()
            HStack {
                Text("Nothing is changed until you click.")
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
        }
        .frame(width: 580, height: 540)
        .onAppear { d.loadSoftware() }
        .alert(item: $confirm) { c in
            Alert(title: Text(c.title),
                  message: Text(c.message),
                  primaryButton: .destructive(Text(c.confirmLabel)) { c.run() },
                  secondaryButton: .cancel())
        }
    }
}

struct MacOSSheet: View {
    @ObservedObject var d: AppDelegate
    @Environment(\.dismiss) private var dismiss
    @State private var confirm: ConfirmAction?

    var body: some View {
        VStack(spacing: 0) {
            SheetHeader(icon: "gearshape.2",
                        title: "macOS background services",
                        subtitle: "macOS' own background work. Unticking is reversible: launchd services re-enable in one click, Spotlight and Power Nap use Apple's switches.")
            Divider()
            ScrollView {
                VStack(spacing: 6) {
                    if !d.macosNote.isEmpty {
                        HStack(alignment: .top, spacing: 6) {
                            Image(systemName: "info.circle")
                                .font(.system(size: 10))
                                .foregroundStyle(Color.accentColor)
                            Text(d.macosNote)
                                .font(.system(size: 10.5))
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(10)
                        .background(Color.accentColor.opacity(0.08), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    }
                    ForEach(Array(d.macosItems.enumerated()), id: \.offset) { _, it in
                        let id = it["id"] as? String ?? "?"
                        let enabled = it["enabled"] as? Bool ?? false
                        ToggleRow(title: id,
                                  subtitle: it["desc"] as? String ?? "",
                                  isOn: enabled) { on in
                            if on {
                                d.setMacOS(id, enable: true)
                            } else {
                                confirm = ConfirmAction(
                                    title: "Turn off \(id)?",
                                    message: "This stops that background job until you turn it back on here. Nothing is deleted and no system files are modified.",
                                    confirmLabel: "Turn off"
                                ) { d.setMacOS(id, enable: false) }
                            }
                        }
                    }
                }
                .padding(16)
            }
            Divider()
            HStack {
                Text("Services protected by SIP will refuse and say so.")
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
        }
        .frame(width: 600, height: 560)
        .onAppear { d.loadMacOS() }
        .alert(item: $confirm) { c in
            Alert(title: Text(c.title),
                  message: Text(c.message),
                  primaryButton: .destructive(Text(c.confirmLabel)) { c.run() },
                  secondaryButton: .cancel())
        }
    }
}

// MARK: - App

/// A borderless status-bar panel: it behaves like a popover hanging off the menu
/// bar item, not like a regular app window. It can still take key focus so the
/// pickers and buttons inside it work.
final class PopupPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }

    /// Escape closes the popup, the way a tooltip or menu behaves.
    var onCancel: (() -> Void)?

    override func cancelOperation(_ sender: Any?) {
        onCancel?()
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, ObservableObject {
    static let preferredWidth: CGFloat = 560
    static let preferredHeight: CGFloat = 720
    static let minimumHeight: CGFloat = 380

    var statusItem: NSStatusItem!
    var window: NSWindow?
    var timer: Timer?
    private var outsideClick: Any?

    // Live machine state, published straight into SwiftUI.
    @Published var helperRunning = false
    @Published var helperPresent = false
    @Published var loginOn = false
    @Published var temp: Double?
    @Published var fan: Double?
    @Published var batt: Double?
    @Published var ac = false
    @Published var limit: Int?
    @Published var turbo: Int?
    @Published var lpm: Int?
    @Published var capActive = false
    @Published var mode = "auto"
    @Published var modeLabel = "Auto"
    @Published var fanIntent = "auto"
    @Published var fanSupported = true
    @Published var fanNote = ""
    @Published var smcOK = false
    @Published var mediaActive = false
    @Published var mediaGuard = true
    @Published var procs: [[String: Any]] = []
    @Published var demoted: Set<Int> = []
    @Published var events: [String] = []

    // Acceleration guard
    @Published var accelLines: [String] = []
    @Published var offerFix = false
    var accelPending: Set<String> = []
    var accelNote = ""

    @Published var hint = ""
    @Published var software: [[String: Any]] = []
    @Published var macosItems: [[String: Any]] = []
    @Published var macosNote = ""

    /// One accent colour for the whole window, derived from the thermal state
    /// so the UI reads at a glance: calm, warm, hot.
    var accent: Color {
        guard let t = temp else { return .gray }
        if t >= 88 { return .red }
        if t >= 80 { return .orange }
        if t >= 68 { return .green }
        return .teal
    }

    var headline: String {
        guard let t = temp else { return "Waiting for the first reading…" }
        if t >= 90 { return "Running hot — protecting the chip" }
        if t >= 78 { return "Warm — shedding background load" }
        if capActive { return "Capped to stay under the limit" }
        return "Cool and running at full speed"
    }

    var clockText: String {
        if capActive, let l = lpm { return String(format: "%.1f GHz capped", Double(l) / 1000) }
        if let t = turbo { return String(format: "%.1f GHz turbo", Double(t) / 1000) }
        return "—"
    }

    var batteryText: String {
        let b = batt.map { String(format: "%.1f °C", $0) } ?? "—"
        return "\(b) · \(ac ? "AC" : "battery")"
    }

    var guardTrailing: String {
        if !mediaGuard { return "off" }
        return mediaActive ? "protecting" : "watching"
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildStatusItem()
        buildWindow()
        refresh()
        showPanel()
        timer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.refresh()
        }
        RunLoop.main.add(timer!, forMode: .common)
    }

    /// Clicking anywhere outside the app closes the popup, like a menu.
    func applicationDidResignActive(_ notification: Notification) {
        hidePanel()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    // MARK: status item

    func buildStatusItem() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        statusItem.button?.title = "—°"
        statusItem.button?.font = NSFont.monospacedDigitSystemFont(ofSize: 12, weight: .medium)
        statusItem.button?.target = self
        statusItem.button?.action = #selector(toggleWindow)
    }

    @objc func toggleWindow() {
        guard let w = window else { return }
        if w.isVisible {
            hidePanel()
        } else {
            showPanel()
        }
    }

    func showPanel() {
        guard let w = window else { return }
        refresh()
        positionPanel()
        statusItem.button?.highlight(true)
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        if outsideClick == nil {
            outsideClick = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in
                self?.hidePanel()
            }
        }
    }

    func hidePanel() {
        window?.orderOut(nil)
        statusItem.button?.highlight(false)
        if let m = outsideClick {
            NSEvent.removeMonitor(m)
            outsideClick = nil
        }
    }

    // MARK: window

    func buildWindow() {
        let visible = NSScreen.main?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1280, height: 800)
        // Never taller than the space the screen actually offers, so the bottom
        // of the panel can never end up off-screen. The content scrolls.
        let height = min(Self.preferredHeight, max(Self.minimumHeight, visible.height - 24))

        let w = PopupPanel(contentRect: NSRect(x: 0, y: 0, width: Self.preferredWidth, height: height),
                           styleMask: [.borderless, .nonactivatingPanel],
                           backing: .buffered, defer: false)
        w.level = .statusBar
        w.isFloatingPanel = true
        w.hidesOnDeactivate = true
        w.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        w.backgroundColor = .clear
        w.isOpaque = false
        w.hasShadow = true
        w.isMovable = false
        w.isReleasedWhenClosed = false
        w.animationBehavior = .utilityWindow
        // No window chrome of any kind: borderless already means no close,
        // minimise, zoom or resize controls, and it must not be resizable.
        w.styleMask = [.borderless, .nonactivatingPanel]
        w.isMovableByWindowBackground = false

        // Rounded, clipped shell so the panel reads as a popover even though it
        // is a borderless window.
        let shell = NSView()
        shell.wantsLayer = true
        shell.layer?.cornerRadius = 14
        shell.layer?.masksToBounds = true
        shell.layer?.borderWidth = 1
        shell.layer?.borderColor = NSColor.white.withAlphaComponent(0.10).cgColor

        let fx = NSVisualEffectView(frame: shell.bounds)
        fx.material = .popover
        fx.blendingMode = .behindWindow
        fx.state = .active
        fx.autoresizingMask = [.width, .height]

        let host = NSHostingView(rootView: RootView(d: self))
        host.frame = fx.bounds
        host.autoresizingMask = [.width, .height]
        fx.addSubview(host)

        shell.addSubview(fx)
        w.contentView = shell
        w.onCancel = { [weak self] in self?.hidePanel() }
        window = w
    }

    /// Hang the panel under the status item, clamped to that screen.
    private func positionPanel() {
        guard let w = window,
              let button = statusItem.button,
              let bar = button.window else { return }
        let anchor = bar.convertToScreen(button.convert(button.bounds, to: nil))
        let screen = bar.screen?.visibleFrame ?? NSScreen.main?.visibleFrame ?? .zero

        let width = Self.preferredWidth
        // Flush against the menu bar so the panel reads as attached to the
        // status item rather than as a floating window.
        let top = anchor.minY
        let height = min(Self.preferredHeight, max(Self.minimumHeight, top - screen.minY - 8))
        var x = anchor.midX - width / 2
        x = min(max(x, screen.minX + 8), screen.maxX - width - 8)

        w.setFrame(NSRect(x: x, y: top - height, width: width, height: height), display: false)
    }

    // MARK: actions from the UI

    func setMode(_ value: String) {
        sendMode(value)
        mode = value
        refresh()
    }

    func setFan(_ value: String) {
        sendFan(value)
        fanIntent = value
        refresh()
    }

    func setMediaGuard(_ on: Bool) {
        mediaGuard = on
        _ = sendRequest("{\"cmd\":\"media_guard\",\"value\":\"\(on ? "on" : "off")\"}")
        refresh()
    }

    /// User override for the one case the guard must not take itself: a repair
    /// that needs the app restarted while a call or video is running.
    func fixAccelerationNow() {
        let sample = cpuSample()
        for app in acceleratedApps {
            let s = accelStatus(app, sample: sample)
            guard s.installed, !s.enabled else { continue }
            let (ok, msg) = enableAcceleration(app, relaunch: s.running)
            accelNote = msg
            if ok { accelPending.remove(app.bundleID) }
        }
        refresh()
    }

    /// Runs on every refresh tick. Layered, in this order:
    ///   1. what the apps are doing (closed / idle / decoding)
    ///   2. what the machine is doing (real video decode, live camera)
    ///   3. the pure policy in `accelAction`
    ///   4. only then, a side effect
    /// The guard is deliberately silent until a real video or camera call has
    /// actually happened with acceleration off — it never restarts a browser
    /// just because it is open, and never mid-session.
    func refreshAccelerationGuard() {
        let sample = cpuSample()
        let pulse = mediaPulse(sample: sample)
        var lines: [String] = []
        var offer = false

        for app in acceleratedApps {
            let s = accelStatus(app, sample: sample)
            guard s.installed else { continue }

            // With acceleration off there is no GPU helper to watch: the decode
            // happens in the renderer, which is exactly why the app is burning
            // CPU. A busy, unaccelerated browser therefore *is* a media session.
            let liveSession = pulse.active || (s.usage == .inUse && !s.enabled)
            let pulseNow = MediaPulse(video: liveSession, camera: pulse.camera)
            if !s.enabled, liveSession { accelPending.insert(app.bundleID) }

            switch accelAction(s, pulse: pulseNow, pending: accelPending.contains(app.bundleID)) {
            case .nothing:
                if s.enabled {
                    lines.append("\(app.name): hardware decode on")
                } else {
                    lines.append("\(app.name): acceleration off, waiting for a video or call")
                }
            case .hold(let why):
                lines.append("\(app.name): acceleration off — \(why)")
                offer = pulse.active
            case .enableNow(let relaunch):
                let (ok, msg) = enableAcceleration(app, relaunch: relaunch)
                if ok {
                    accelPending.remove(app.bundleID)
                    lines.append("Fixed: \(msg)")
                } else {
                    lines.append("\(app.name): \(msg)")
                    offer = true
                }
            }
        }

        if !accelNote.isEmpty { lines.append(accelNote) }
        accelLines = lines
        offerFix = offer
        accelNote = ""
    }

    // MARK: installed software

    func loadSoftware() {
        guard let raw = sendRequest("{\"cmd\":\"agents\"}"),
              let d = raw.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any],
              let items = obj["items"] as? [[String: Any]]
        else { return }
        software = items
    }

    /// Label only: the daemon re-derives path and scope itself, so the UI
    /// cannot be used to point launchctl at an arbitrary file.
    func setAgent(_ label: String, enable: Bool) {
        _ = sendRequest("{\"cmd\":\"agent\",\"label\":\"\(label)\",\"enable\":\(enable)}")
        loadSoftware()
    }

    // MARK: macOS background services

    func loadMacOS() {
        guard let raw = sendRequest("{\"cmd\":\"apple\"}"),
              let d = raw.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any],
              let items = obj["items"] as? [[String: Any]]
        else { return }
        macosItems = items
        macosNote = obj["note"] as? String ?? ""
    }

    func setMacOS(_ id: String, enable: Bool) {
        _ = sendRequest("{\"cmd\":\"apple_set\",\"id\":\"\(id)\",\"enable\":\(enable)}")
        loadMacOS()
    }

    /// POSIX shell single-quote escaping, so a path can never break out of the
    /// command that is handed to `do shell script` as root.
    func shellQuote(_ s: String) -> String {
        return "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    func installHelper() {
        guard let script = Bundle.main.path(forResource: "install-helper", ofType: "sh") else {
            hint = "install-helper.sh missing from the app bundle."
            return
        }
        let cmd = "/bin/sh \(shellQuote(script)) \(shellQuote(Bundle.main.bundlePath))"
        let escaped = cmd.replacingOccurrences(of: "\\", with: "\\\\")
                           .replacingOccurrences(of: "\"", with: "\\\"")
        let source = "do shell script \"\(escaped)\" with administrator privileges"
        var err: NSDictionary?
        if let apple = NSAppleScript(source: source) {
            apple.executeAndReturnError(&err)
            if let e = err {
                hint = "Installer error: \(e[NSAppleScript.errorMessage] ?? "unknown")"
            } else {
                hint = "Helper installed. Thermal protection is active."
            }
        }
        refresh()
    }

    // MARK: login item

    func loginPlistPath() -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return "\(home)/Library/LaunchAgents/com.thermalguard.app.plist"
    }

    func loginEnabled() -> Bool {
        FileManager.default.fileExists(atPath: loginPlistPath())
    }

    func setLogin(_ on: Bool) {
        let path = loginPlistPath()
        if on {
            // Serialised, not string-built: no XML-injection surface in the path.
            let dictionary: [String: Any] = [
                "Label": "com.thermalguard.app",
                "ProgramArguments": ["/usr/bin/open", "-a", Bundle.main.bundlePath],
                "RunAtLoad": true,
            ]
            if let data = try? PropertyListSerialization.data(
                fromPropertyList: dictionary, format: .xml, options: 0) {
                try? data.write(to: URL(fileURLWithPath: path), options: .atomic)
            }
            _ = shell("/bin/launchctl", ["bootstrap", "gui/\(getuid())", path])
        } else {
            _ = shell("/bin/launchctl", ["bootout", "gui/\(getuid())/com.thermalguard.app"])
            try? FileManager.default.removeItem(atPath: path)
        }
        loginOn = loginEnabled()
    }

    // MARK: refresh

    func refresh() {
        refreshAccelerationGuard()
        helperPresent = helperInstalled()
        loginOn = loginEnabled()

        guard let s = requestStatus() else {
            helperRunning = false
            statusItem?.button?.title = "—°"
            temp = nil
            fan = nil
            return
        }
        helperRunning = true

        temp = s["cpu_temp"] as? Double
        fan = s["fan_rpm"] as? Double
        batt = s["battery_temp"] as? Double
        ac = s["ac_power"] as? Bool ?? false
        limit = s["speed_limit"] as? Int
        turbo = s["turbo_mhz"] as? Int
        lpm = s["lpm_mhz"] as? Int
        capActive = s["cap_active"] as? Bool ?? false
        modeLabel = s["mode_label"] as? String ?? "Auto"
        smcOK = s["smc_ok"] as? Bool ?? false
        mediaActive = s["media_active"] as? Bool ?? false
        if let g = s["media_guard"] as? Bool { mediaGuard = g }
        procs = s["procs"] as? [[String: Any]] ?? []
        events = s["events"] as? [String] ?? []
        fanSupported = s["fan_supported"] as? Bool ?? true
        fanNote = s["fan_note"] as? String ?? ""
        if let m = s["mode"] as? String, m != mode { mode = m }
        if let f = s["fan_intent"] as? String, f != fanIntent { fanIntent = f }

        // A disappeared PID must not linger in the demoted set — it would put a
        // stale down-arrow next to an unrelated process that reused the pid.
        let live = Set(procs.compactMap { $0["pid"] as? Int })
        var flagged = Set<Int>()
        for p in s["demoted"] as? [[String: Any]] ?? [] {
            if let pid = p["pid"] as? Int, live.contains(pid) { flagged.insert(pid) }
        }
        demoted = flagged

        if let t = temp {
            statusItem?.button?.title = "\(Int(t.rounded()))°"
        } else {
            statusItem?.button?.title = "–°"
        }
    }
}

// Unit tests for the guard's decision table. The policy is a pure function, so
// this needs no live browser and no privileges: `ThermalGuard --self-test`.
if CommandLine.arguments.contains("--self-test") {
    var failures = 0
    func expect(_ name: String, _ got: AccelAction, _ want: AccelAction) {
        if got != want {
            failures += 1
            print("FAIL \(name): got \(got), want \(want)")
        } else {
            print("ok   \(name)")
        }
    }

    let app = acceleratedApps[0]
    var base = AccelStatus(app: app)
    base.installed = true
    base.readable = true
    let quiet = MediaPulse(video: false, camera: false)
    let video = MediaPulse(video: true, camera: false)
    let camera = MediaPulse(video: false, camera: true)

    // Nothing to do when acceleration is already on, whatever is playing.
    var on = base; on.enabled = true
    expect("enabled app is left alone", accelAction(on, pulse: video, pending: true), .nothing)

    // Not installed / unreadable are benign, not errors.
    var missing = base; missing.installed = false; missing.enabled = false
    expect("missing app ignored", accelAction(missing, pulse: quiet, pending: true), .nothing)
    var locked = base; locked.readable = false; locked.enabled = false
    expect("unreadable holds", accelAction(locked, pulse: quiet, pending: true), .hold("settings readable once it quits"))

    // The core requirement: no media seen yet means no action at all.
    var off = base; off.enabled = false
    expect("idle app with nothing owed is untouched", accelAction(off, pulse: quiet, pending: false), .nothing)

    // Real video or a live camera during the session -> hold.
    expect("holds during video", accelAction(off, pulse: video, pending: true), .hold("live video/call in progress"))
    expect("holds during camera call", accelAction(off, pulse: camera, pending: true), .hold("live video/call in progress"))

    // Owed and the session ended -> repair, restarting only if it is running.
    expect("closed app fixed without relaunch", accelAction(off, pulse: quiet, pending: true), .enableNow(relaunch: false))
    var running = off; running.running = true; running.usage = .idle
    expect("idle running app restarted", accelAction(running, pulse: quiet, pending: true), .enableNow(relaunch: true))

    print(failures == 0 ? "PASS 8/8" : "FAILED \(failures)")
    exit(failures == 0 ? 0 : 1)
}

// Headless entry points, used by the test suite and for a quick check from a
// terminal: `ThermalGuard --accel-check` / `--accel-fix` / `--media-pulse`.
if CommandLine.arguments.contains("--media-pulse") {
    let p = mediaPulse(sample: cpuSample())
    print("{\"video\":\(p.video),\"camera\":\(p.camera)}")
    exit(p.active ? 0 : 3)
}
if CommandLine.arguments.contains("--accel-check") {
    let sample = cpuSample()
    let pulse = mediaPulse(sample: sample)
    var rows: [String] = []
    var bad = false
    for app in acceleratedApps {
        let s = accelStatus(app, sample: sample)
        guard s.installed else { continue }
        if !s.enabled { bad = true }
        rows.append("{\"name\":\"\(app.name)\",\"bundle\":\"\(app.bundleID)\",\"enabled\":\(s.enabled),\"running\":\(s.running),\"readable\":\(s.readable),\"usage\":\"\(s.usage.rawValue)\",\"cpu\":\(Int(s.cpu))}")
    }
    print("{\"media\":{\"video\":\(pulse.video),\"camera\":\(pulse.camera)},\"apps\":[\(rows.joined(separator: ","))]}")
    exit(bad ? 3 : 0)
}
if CommandLine.arguments.contains("--accel-fix") {
    let sample = cpuSample()
    let pulse = mediaPulse(sample: sample)
    for app in acceleratedApps {
        let s = accelStatus(app, sample: sample)
        guard s.installed, !s.enabled else { continue }
        if pulse.active { print("\(app.name): live media session, holding"); continue }
        let (ok, msg) = enableAcceleration(app, relaunch: s.running)
        print(msg)
        if !ok { exit(1) }
    }
    exit(0)
}
if CommandLine.arguments.contains("--chrome-fix-legacy") {
    let (ok, msg) = enableAcceleration(acceleratedApps[0], relaunch: false)
    print(msg)
    exit(ok ? 0 : 1)
}
let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
