import AppKit
import Combine
import CoreGraphics
import CoreMediaIO
import CoreMedia
import Darwin
import Foundation
import SwiftUI
import VideoToolbox

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

// MARK: - Shared process/window inspection

/// A normal, visible, non-zero window owned by `pid`. `.optionAll` includes
/// off-screen and minimised windows, so an app with a minimised document is not
/// mistaken for an app with nothing open.
func hasNormalWindow(pid: pid_t) -> Bool {
    guard let list = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as? [[String: Any]] else {
        return false
    }
    for w in list {
        guard let owner = w[kCGWindowOwnerPID as String] as? Int, pid_t(owner) == pid else { continue }
        let layer = w[kCGWindowLayer as String] as? Int ?? 0
        if layer != 0 { continue }
        if let alpha = w[kCGWindowAlpha as String] as? Double, alpha <= 0.01 { continue }
        if let bounds = w[kCGWindowBounds as String] as? [String: Any],
           let width = bounds["Width"] as? Double,
           let height = bounds["Height"] as? Double,
           width > 2, height > 2 {
            return true
        }
    }
    return false
}

func processPath(pid: pid_t) -> String? {
    let out = shell("/bin/ps", ["-p", "\(pid)", "-o", "comm="]).1
    let path = out.trimmingCharacters(in: .whitespacesAndNewlines)
    return path.isEmpty ? nil : path
}

/// PIDs that currently hold an audio assertion. Used to avoid quitting an app
/// that is playing music or is in a call.
func audioAssertionPids() -> Set<pid_t> {
    let out = shell("/usr/bin/pmset", ["-g", "assertions"]).1
    var pids = Set<pid_t>()
    for raw in out.split(separator: "\n") {
        let line = raw.trimmingCharacters(in: .whitespaces)
        guard line.hasPrefix("pid "), line.lowercased().contains("audio") else { continue }
        let rest = line.dropFirst(4)
        guard let open = rest.firstIndex(of: "("), let pid = pid_t(rest[..<open]) else { continue }
        pids.insert(pid)
    }
    return pids
}

func cpuByPid() -> [pid_t: Double] {
    let out = shell("/bin/ps", ["-Ao", "pid=,pcpu="]).1
    var map = [pid_t: Double]()
    for line in out.split(separator: "\n") {
        let parts = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true)
        guard parts.count == 2, let pid = pid_t(parts[0]), let cpu = Double(parts[1]) else { continue }
        map[pid] = cpu
    }
    return map
}

func topMemoryApps(_ limit: Int = 3) -> [[String: Any]] {
    let out = shell("/bin/ps", ["-Ao", "pid=,rss=,comm=", "-m"]).1
    var rows: [[String: Any]] = []
    for line in out.split(separator: "\n") {
        let parts = line.split(separator: " ", maxSplits: 2, omittingEmptySubsequences: true)
        guard parts.count == 3, let pid = Int(parts[0]), let rss = Double(parts[1]) else { continue }
        let name = (String(parts[2]) as NSString).lastPathComponent
        rows.append(["pid": pid, "mb": rss / 1024.0, "name": name])
        if rows.count >= limit { break }
    }
    return rows
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

// MARK: - Chrome background guard
//
// Chrome keeps a background mode and three Google launch agents alive even
// when no window is open. This guard is deliberately conservative about what
// "not in use" means: no window, not frontmost, no audio assertion, no GPU
// decode, and not still launching. Only then is Chrome asked to quit
// gracefully, its own background mode is switched off, and leftover helpers
// are sent SIGTERM after their executable path is re-verified.

let chromeBundleID = "com.google.Chrome"
let chromeAppPath = "/Applications/Google Chrome.app"
let chromeMainExecutable = chromeAppPath + "/Contents/MacOS/Google Chrome"
let googleBackgroundAgents = [
    "com.google.GoogleUpdater.wake",
    "com.google.keystone.agent",
    "com.google.keystone.xpcservice",
]

struct ChromeGuardState {
    var installed = false
    var running = false
    var launching = false
    var frontmost = false
    var hasWindow = false
    var audio = false
    var media = false
    var bgReadable = true
    var bgEnabled: Bool?
    var helperPids: [pid_t] = []
    var active: Bool { running && (launching || frontmost || hasWindow || audio || media) }
}

enum ChromeGuardAction: Equatable {
    case nothing
    case trim
    case restore
}

/// Pure policy: the only place that decides whether Chrome is fair game.
func chromeGuardAction(_ s: ChromeGuardState, enabled: Bool, managed: Bool) -> ChromeGuardAction {
    guard enabled else { return managed ? .restore : .nothing }
    guard s.installed else { return managed ? .restore : .nothing }
    if s.active { return managed ? .restore : .nothing }
    return .trim
}

func isChromeHelperPath(_ path: String) -> Bool {
    guard path.contains("/Google Chrome.app/Contents/") else { return false }
    if path == chromeMainExecutable { return false }
    let lower = path.lowercased()
    return lower.contains("helper") || lower.contains("chrome_crashpad_handler")
}

func chromeHelperPids() -> [pid_t] {
    let out = shell("/bin/ps", ["-Ao", "pid=,comm="]).1
    var pids: [pid_t] = []
    for line in out.split(separator: "\n") {
        let parts = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true)
        guard parts.count == 2, let pid = pid_t(parts[0]) else { continue }
        if isChromeHelperPath(String(parts[1])) { pids.append(pid) }
    }
    return pids
}

func chromeLocalStateURL() -> URL {
    appStateURL(acceleratedApps[0])
}

func chromeBackgroundMode() -> (readable: Bool, enabled: Bool?) {
    guard let data = try? Data(contentsOf: chromeLocalStateURL()),
          let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        return (false, nil)
    }
    if let bg = obj["background_mode"] as? [String: Any],
       let on = bg["enabled"] as? Bool {
        return (true, on)
    }
    return (true, nil)
}

func readChromeLocalState() -> (URL, [String: Any])? {
    let url = chromeLocalStateURL()
    guard let data = try? Data(contentsOf: url),
          let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        return nil
    }
    return (url, obj)
}

func writeChromeLocalState(_ obj: [String: Any], to url: URL) -> Bool {
    guard let out = try? JSONSerialization.data(withJSONObject: obj, options: []) else { return false }
    let tmp = url.appendingPathExtension("thermalguard-tmp")
    do {
        try out.write(to: tmp, options: .atomic)
        if FileManager.default.fileExists(atPath: url.path) {
            _ = try FileManager.default.replaceItemAt(url, withItemAt: tmp)
        } else {
            try FileManager.default.moveItem(at: tmp, to: url)
        }
        return true
    } catch {
        try? FileManager.default.removeItem(at: tmp)
        return false
    }
}

func chromeBgManaged() -> Bool {
    UserDefaults.standard.bool(forKey: "chromeBgManaged")
}

func setChromeBgManaged(_ on: Bool) {
    UserDefaults.standard.set(on, forKey: "chromeBgManaged")
}

func chromeManagedAgents() -> [String] {
    UserDefaults.standard.stringArray(forKey: "chromeManagedAgents") ?? []
}

func setChromeManagedAgents(_ labels: [String]) {
    UserDefaults.standard.set(labels, forKey: "chromeManagedAgents")
}

/// Disable only the three known Google background agents, and remember exactly
/// which ones were changed so restoration is exact. `launchctl disable` is
/// persistent and per-user, so this is fully reversible with `enable`.
func disableGoogleAgents() -> [String] {
    var managed = Set(chromeManagedAgents())
    var changed: [String] = []
    for label in googleBackgroundAgents {
        let target = "gui/\(getuid())/\(label)"
        if shell("/bin/launchctl", ["disable", target]).0 == 0 {
            managed.insert(label)
            changed.append(label)
        }
    }
    if !changed.isEmpty { setChromeManagedAgents(managed.sorted()) }
    return changed
}

/// Re-enable only the labels this app disabled. Anything the user disabled
/// independently is never in the managed set and is never touched. The stored
/// list is re-filtered against the compiled-in catalog so a tampered preference
/// cannot make us enable an arbitrary service.
func restoreGoogleAgents() -> Bool {
    var managed = Set(chromeManagedAgents().filter { googleBackgroundAgents.contains($0) })
    var ok = true
    for label in Array(managed) {        let target = "gui/\(getuid())/\(label)"
        if shell("/bin/launchctl", ["enable", target]).0 == 0 {
            managed.remove(label)
        } else {
            ok = false
        }
    }
    setChromeManagedAgents(managed.sorted())
    return ok
}

/// Switch Chrome's own background mode off, saving the original value first so
/// the change is fully reversible.
@discardableResult
func disableChromeBackground() -> Bool {
    guard let (url, state) = readChromeLocalState() else { return false }
    var obj = state
    let alreadyManaged = chromeBgManaged()
    let original = obj["background_mode"]
    obj["background_mode"] = ["enabled": false]
    guard writeChromeLocalState(obj, to: url) else { return false }
    if !alreadyManaged {
        if let original = original,
           let data = try? JSONSerialization.data(withJSONObject: original) {
            UserDefaults.standard.set(true, forKey: "chromeBgOriginalPresent")
            UserDefaults.standard.set(data, forKey: "chromeBgOriginal")
        } else {
            UserDefaults.standard.set(false, forKey: "chromeBgOriginalPresent")
            UserDefaults.standard.removeObject(forKey: "chromeBgOriginal")
        }
        setChromeBgManaged(true)
    }
    return true
}

@discardableResult
func restoreChromeBackground() -> Bool {
    guard chromeBgManaged() else { return true }
    guard let (url, state) = readChromeLocalState() else {
        setChromeBgManaged(false)
        return true
    }
    var obj = state
    if UserDefaults.standard.bool(forKey: "chromeBgOriginalPresent"),
       let data = UserDefaults.standard.data(forKey: "chromeBgOriginal"),
       let original = try? JSONSerialization.jsonObject(with: data) {
        obj["background_mode"] = original
    } else {
        obj.removeValue(forKey: "background_mode")
    }
    guard writeChromeLocalState(obj, to: url) else { return false }
    setChromeBgManaged(false)
    UserDefaults.standard.removeObject(forKey: "chromeBgOriginal")
    UserDefaults.standard.set(false, forKey: "chromeBgOriginalPresent")
    return true
}

func chromeGuardState(sample: String) -> ChromeGuardState {
    var s = ChromeGuardState()
    s.installed = FileManager.default.fileExists(atPath: chromeAppPath)
    let apps = NSRunningApplication.runningApplications(withBundleIdentifier: chromeBundleID)
    s.running = !apps.isEmpty
    s.launching = apps.contains { !$0.isFinishedLaunching }
    if let app = apps.first {
        let pid = app.processIdentifier
        s.frontmost = app.isActive
        s.hasWindow = hasNormalWindow(pid: pid)
        s.audio = audioAssertionPids().contains(pid)
    }
    // Attribute media work to Chrome specifically: its GPU helper doing real
    // work, or a live camera anywhere in the system.
    s.media = cpuFor("Google Chrome Helper (GPU)", in: sample) >= APP_IN_USE_CPU || cameraInUse()
    let bg = chromeBackgroundMode()
    s.bgReadable = bg.readable
    s.bgEnabled = bg.enabled
    s.helperPids = chromeHelperPids()
    return s
}

func stopChromeHelpers() -> Int {
    var stopped = 0
    for pid in chromeHelperPids() {
        // PID reuse guard: re-read the executable immediately before signalling.
        guard let path = processPath(pid: pid), isChromeHelperPath(path) else { continue }
        if kill(pid, SIGTERM) == 0 { stopped += 1 }
    }
    return stopped
}

// MARK: - Graceful memory guard
//
// macOS already compresses and swaps memory. The safe extra lever is to ask
// genuinely idle apps to quit gracefully under real pressure. Nothing is
// force-killed, and apps with windows, audio, camera, CPU work or a recent
// interaction are never candidates.

enum MemoryPressure: String {
    case normal, high, critical
}

struct MemoryStats {
    var readable = false
    var freePercent: Double = 0
    var swapUsedMB: Double = 0
    var swapTotalMB: Double = 0
    var compressedMB: Double = 0
    var totalMB: Double = 0
    var pressure: MemoryPressure = .normal
}

struct MemoryAppState {
    var bundleID: String
    var name: String
    var pid: pid_t
    var protectedApp = false
    var hasWindow = false
    var frontmost = false
    var audio = false
    var media = false
    var cpu: Double = 0
}

/// Pure pressure classification. Thresholds are deliberately conservative:
/// macOS is allowed to use most of RAM for cache before we act.
func memoryPressureLevel(freePercent: Double, swapUsedPercent: Double, compressedPercent: Double) -> MemoryPressure {
    guard freePercent.isFinite else { return .critical }
    if freePercent < 8 || swapUsedPercent >= 80 || compressedPercent >= 60 { return .critical }
    if freePercent < 15 || swapUsedPercent >= 50 || compressedPercent >= 45 { return .high }
    return .normal
}

/// Pure candidate policy. A high-pressure event waits 10 minutes of idleness;
/// a critical one waits 5. The app can still refuse to quit.
func shouldQuitIdleApp(
    _ app: MemoryAppState,
    pressure: MemoryPressure,
    enabled: Bool,
    idleFor: Double,
    refused: Bool
) -> Bool {
    guard enabled, pressure != .normal, !refused else { return false }
    guard !app.protectedApp, !app.hasWindow, !app.frontmost, !app.audio, !app.media else { return false }
    guard app.cpu < 3.0 else { return false }
    let threshold: Double = pressure == .critical ? 300 : 600
    return idleFor >= threshold
}

func megabytes(_ token: Substring) -> Double {
    var text = String(token)
    var multiplier = 1.0
    if text.hasSuffix("G") {
        multiplier = 1024
        text.removeLast()
    } else if text.hasSuffix("M") {
        text.removeLast()
    } else if text.hasSuffix("K") {
        multiplier = 1.0 / 1024.0
        text.removeLast()
    }
    return (Double(text) ?? 0) * multiplier
}

func swapValue(_ text: String, label: String) -> Double {
    guard let range = text.range(of: "\(label) = ") else { return 0 }
    let token = text[range.upperBound...].split(separator: " ", maxSplits: 1).first ?? ""
    return megabytes(token)
}

func memoryStats() -> MemoryStats {
    var s = MemoryStats()
    let out = shell("/usr/bin/vm_stat", []).1
    var pageSize = 4096.0
    var free = 0.0
    var active = 0.0
    var inactive = 0.0
    var speculative = 0.0
    var wired = 0.0
    var compressor = 0.0

    for raw in out.split(separator: "\n") {
        let line = raw.trimmingCharacters(in: .whitespaces)
        if line.hasPrefix("Mach Virtual Memory Statistics") {
            if let r = line.range(of: "page size of "),
               let end = line[r.upperBound...].firstIndex(of: " ") {
                pageSize = Double(line[r.upperBound..<end]) ?? pageSize
            }
            continue
        }
        let parts = line.split(separator: ":", maxSplits: 1)
        guard parts.count == 2 else { continue }
        let key = parts[0].trimmingCharacters(in: .whitespaces)
        let rawValue = parts[1].trimmingCharacters(in: .whitespaces).replacingOccurrences(of: ".", with: "")
        let value = Double(rawValue) ?? 0
        switch key {
        case "Pages free": free = value
        case "Pages active": active = value
        case "Pages inactive": inactive = value
        case "Pages speculative": speculative = value
        case "Pages wired down": wired = value
        case "Pages occupied by compressor": compressor = value
        default: break
        }
    }

    let total = free + active + inactive + speculative + wired + compressor
    guard total > 0 else { return s }
    s.readable = true
    s.totalMB = total * pageSize / 1_048_576
    s.compressedMB = compressor * pageSize / 1_048_576
    s.freePercent = (free + inactive + speculative) / total * 100

    let swap = shell("/usr/sbin/sysctl", ["vm.swapusage"]).1
    s.swapTotalMB = swapValue(swap, label: "total")
    s.swapUsedMB = swapValue(swap, label: "used")

    let swapUsedPercent = s.swapTotalMB > 0 ? s.swapUsedMB / s.swapTotalMB * 100 : 0
    let compressedPercent = s.totalMB > 0 ? s.compressedMB / s.totalMB * 100 : 0
    s.pressure = memoryPressureLevel(
        freePercent: s.freePercent,
        swapUsedPercent: swapUsedPercent,
        compressedPercent: compressedPercent
    )
    return s
}

let memoryProtectedBundleIDs: Set<String> = [
    "com.apple.finder",
    "com.apple.dock",
    "com.apple.SystemUIServer",
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
    "com.apple.loginwindow",
    "com.apple.SecurityAgent",
    "com.apple.CoreServicesUIAgent",
    "com.apple.universalaccessd",
    "com.apple.ScreenTimeAgent",
    "com.apple.familycontrols.useragent",
    "com.thermalguard.app",
    chromeBundleID,
]

let memoryProtectedFragments = [
    "bitdefender",
    "wacom",
    "xprotect",
    "mdm",
    "endpointsecurity",
    "security",
    "thermalguard",
]

func isProtectedMemoryApp(_ app: NSRunningApplication) -> Bool {
    let bundle = app.bundleIdentifier ?? ""
    if memoryProtectedBundleIDs.contains(bundle) { return true }
    let name = (app.localizedName ?? "").lowercased()
    let path = (app.bundleURL?.path ?? "").lowercased()
    return memoryProtectedFragments.contains { name.contains($0) || path.contains($0) }
}

func memoryAppStates(sample: String) -> [MemoryAppState] {
    let audio = audioAssertionPids()
    let cpu = cpuByPid()
    let camera = cameraInUse()
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    var rows: [MemoryAppState] = []

    for app in NSWorkspace.shared.runningApplications {
        guard app.activationPolicy == .regular,
              app.isFinishedLaunching,
              app.processIdentifier > 0,
              let bundleURL = app.bundleURL else { continue }
        let path = bundleURL.path
        // Only user-facing bundles. System UI and agents are not candidates.
        guard path.hasPrefix("/Applications/") || path.hasPrefix(home + "/Applications/") else { continue }
        let pid = app.processIdentifier
        let bundle = app.bundleIdentifier ?? "pid-\(pid)"
        rows.append(MemoryAppState(
            bundleID: bundle,
            name: app.localizedName ?? bundle,
            pid: pid,
            protectedApp: isProtectedMemoryApp(app),
            hasWindow: hasNormalWindow(pid: pid),
            frontmost: app.isActive,
            audio: audio.contains(pid),
            media: camera,
            cpu: cpu[pid] ?? 0
        ))
    }
    return rows
}

func doubleMap(_ key: String) -> [String: Double] {
    let raw = UserDefaults.standard.dictionary(forKey: key) ?? [:]
    var out: [String: Double] = [:]
    for (k, v) in raw {
        if let d = v as? Double {
            out[k] = d
        } else if let n = v as? NSNumber {
            out[k] = n.doubleValue
        }
    }
    return out
}

func saveDoubleMap(_ key: String, _ map: [String: Double]) {
    UserDefaults.standard.set(map, forKey: key)
}

// MARK: - T2 / VideoToolbox hardware offload guard
//
// VideoToolbox is the source of truth: it reports whether this Mac's hardware
// can decode/encode H.264 and HEVC. That makes the guard work on every model,
// not just MacBookAir9,1. When a video is playing we compare the hardware
// pipeline with the observed work; a software fallback is surfaced honestly.
// The one thing we cannot do safely is silently install a browser extension,
// so the h264ify recommendation is a link, never an automatic install.

enum CodecVerdict: Equatable {
    case unavailable
    case ready(h264: Bool, hevc: Bool)
    case hardwareOffload
    case softwareDecode
}

func codecVerdict(h264: Bool, hevc: Bool, videoActive: Bool, hardwareActive: Bool) -> CodecVerdict {
    if !h264 { return .unavailable }
    if videoActive { return hardwareActive ? .hardwareOffload : .softwareDecode }
    return .ready(h264: h264, hevc: hevc)
}

/// VideoToolbox has no public `VTIsHardwareEncodeSupported`, so ask its encoder
/// list directly. This is the same capability source the encoder itself uses and
/// works on every Mac model, including Macs without a T2.
func hardwareEncodeSupported(_ codec: CMVideoCodecType) -> Bool {
    var list: CFArray?
    guard VTCopyVideoEncoderList(nil, &list) == noErr,
          let encoders = list as? [[String: Any]] else { return false }
    for encoder in encoders {
        guard let raw = encoder[kVTVideoEncoderList_CodecType as String] as? Int,
              CMVideoCodecType(raw) == codec else { continue }
        if let hw = encoder[kVTVideoEncoderList_IsHardwareAccelerated as String] as? Bool, hw {
            return true
        }
    }
    return false
}

func codecHardwareSupport() -> (h264Decode: Bool, h264Encode: Bool, hevcDecode: Bool, hevcEncode: Bool) {
    (
        VTIsHardwareDecodeSupported(kCMVideoCodecType_H264),
        hardwareEncodeSupported(kCMVideoCodecType_H264),
        VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC),
        hardwareEncodeSupported(kCMVideoCodecType_HEVC)
    )
}

struct BrowserExtensionStatus {
    var installed = false
    var enabled = false
    var profiles: [String] = []
}

/// Detect h264ify/h26ify in Chromium profiles. This is read-only: ThermalGuard
/// never installs or enables a browser extension on its own.
func h264ifyStatus(for app: AcceleratedApp) -> BrowserExtensionStatus {
    var status = BrowserExtensionStatus()
    let base = appStateURL(app).deletingLastPathComponent()
    let fm = FileManager.default
    guard let profiles = try? fm.contentsOfDirectory(at: base, includingPropertiesForKeys: nil) else {
        return status
    }
    let h264ifyID = "aleakchihdccplidncghkekgioiakgal"

    for profile in profiles where profile.hasDirectoryPath {
        let extDir = profile.appendingPathComponent("Extensions")
        guard let extensions = try? fm.contentsOfDirectory(at: extDir, includingPropertiesForKeys: nil) else {
            continue
        }
        for ext in extensions {
            let extID = ext.lastPathComponent
            var matched = extID == h264ifyID
            if !matched, let versions = try? fm.contentsOfDirectory(at: ext, includingPropertiesForKeys: nil) {
                for version in versions {
                    let manifest = version.appendingPathComponent("manifest.json")
                    if let data = try? Data(contentsOf: manifest),
                       let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                       let name = obj["name"] as? String,
                       name.lowercased().contains("h264ify") || name.lowercased().contains("h26ify") {
                        matched = true
                    }
                }
            }
            if !matched { continue }
            status.installed = true
            status.profiles.append(profile.lastPathComponent)
            let prefs = profile.appendingPathComponent("Preferences")
            if let data = try? Data(contentsOf: prefs),
               let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
               let extensions = obj["extensions"] as? [String: Any],
               let settings = extensions["settings"] as? [String: Any],
               let entry = settings[extID] as? [String: Any],
               let state = entry["state"] as? Int,
               state == 1 {
                status.enabled = true
            }
        }
    }
    return status
}

func h264ifyStatusAll() -> BrowserExtensionStatus {
    var combined = BrowserExtensionStatus()
    for app in acceleratedApps {
        guard appURL(app) != nil else { continue }
        let s = h264ifyStatus(for: app)
        combined.installed = combined.installed || s.installed
        combined.enabled = combined.enabled || s.enabled
        combined.profiles.append(contentsOf: s.profiles)
    }
    combined.profiles = Array(Set(combined.profiles)).sorted()
    return combined
}

// MARK: - Reduced visual effects
//
// Sequoia's compositor draws live blurred materials, zoom/parallax motion and
// window animations on the GPU every frame, which is the heaviest steady load
// the Intel Iris Plus in this Air carries. The macOS switches that turn those
// off are per-user preferences, so they are written here in the unprivileged UI
// process and never by the root daemon.
//
// The design mirrors the Chrome guard: a compiled-in catalogue plus a pure
// decision function, with the original value of every key recorded before it is
// changed (including "the key did not exist"), so switching the guard off puts
// the machine back exactly as it was. Only known boolean keys are written; a key
// holding another type is reported and left alone rather than converted.

/// What a preference currently holds.
enum DefaultState: Equatable {
    /// The key does not exist in that domain.
    case missing
    /// A boolean, as macOS stores them (`defaults read` prints 0/1).
    case flag(Bool)
    /// Present, but not a boolean: never overwritten, never restored over.
    case other
}

/// The single change to make to one preference.
enum EffectChange: Equatable {
    case write(Bool)
    case restore(Bool)
    case restoreMissing
    case leave
}

/// One curated macOS preference that switches off a GPU-expensive effect.
struct VisualEffectSetting: Equatable {
    let id: String
    let domain: String
    let key: String
    let title: String
    /// The value that turns the effect off.
    let reduced: Bool
}

/// All three live in NSGlobalDomain and are read live by AppKit, so no logout
/// or Dock restart is needed for them to take effect.
let visualEffectCatalog: [VisualEffectSetting] = [
    .init(id: "reduceMotion", domain: "NSGlobalDomain", key: "reduceMotion",
          title: "Reduce Motion", reduced: true),
    .init(id: "reduceTransparency", domain: "NSGlobalDomain", key: "reduceTransparency",
          title: "Reduce Transparency", reduced: true),
    .init(id: "windowAnimations", domain: "NSGlobalDomain", key: "NSAutomaticWindowAnimationsEnabled",
          title: "Window animations", reduced: false),
]

/// Pure: should the reduced-effects profile be in force?
///
/// Same rule as the other user-space guards: on in every cooling mode except
/// Off. It is deliberately not temperature-driven — a preference change is not
/// free (apps reload it), so the profile must be stable rather than flap.
func effectsDesired(enabled: Bool, mode: String) -> Bool {
    enabled && mode != "off"
}

/// Pure decision for one preference.
///
/// `original` is what was recorded *before* the guard first changed the key;
/// `nil` means nothing was recorded, so there is nothing to hand back.
func effectChange(_ setting: VisualEffectSetting,
                  current: DefaultState,
                  reduce: Bool,
                  original: DefaultState?) -> EffectChange {
    if reduce {
        switch current {
        case .missing: return .write(setting.reduced)
        case .flag(let value): return value == setting.reduced ? .leave : .write(setting.reduced)
        case .other: return .leave
        }
    }
    // Someone else changed the type since we wrote it: that value is theirs.
    guard let original, current != .other else { return .leave }
    switch original {
    case .flag(let value): return current == .flag(value) ? .leave : .restore(value)
    case .missing: return current == .missing ? .leave : .restoreMissing
    case .other: return .leave
    }
}

/// Persisted form of a recorded original. `nil` means "do not record this".
func encodeDefaultState(_ state: DefaultState) -> String? {
    switch state {
    case .missing: return "missing"
    case .flag(let value): return value ? "true" : "false"
    case .other: return nil
    }
}

func decodeDefaultState(_ raw: String) -> DefaultState {
    switch raw {
    case "true": return .flag(true)
    case "false": return .flag(false)
    default: return .missing
    }
}

/// `defaults read` exits non-zero for a missing key or domain, which is exactly
/// the "absent" state a restore has to reproduce.
func readDefault(domain: String, key: String) -> DefaultState {
    let (code, out) = shell("/usr/bin/defaults", ["read", domain, key])
    guard code == 0 else { return .missing }
    switch out.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
    case "1", "true", "yes": return .flag(true)
    case "0", "false", "no": return .flag(false)
    default: return .other
    }
}

@discardableResult
func writeDefault(domain: String, key: String, value: Bool) -> Bool {
    shell("/usr/bin/defaults", ["write", domain, key, "-bool", value ? "true" : "false"]).0 == 0
}

@discardableResult
func deleteDefault(domain: String, key: String) -> Bool {
    shell("/usr/bin/defaults", ["delete", domain, key]).0 == 0
}

struct EffectsOutcome: Equatable {
    var changed = 0
    var active = false
    var failed: [String] = []
}

/// Reconcile the recorded state with the wanted one.
///
/// Reads and writes are injected so the decision table can be exercised without
/// touching a real preference; production call sites keep the defaults.
func reconcileVisualEffects(
    catalog: [VisualEffectSetting],
    reduce: Bool,
    read: (VisualEffectSetting) -> DefaultState = { readDefault(domain: $0.domain, key: $0.key) },
    write: (VisualEffectSetting, Bool) -> Bool = { writeDefault(domain: $0.domain, key: $0.key, value: $1) },
    remove: (VisualEffectSetting) -> Bool = { deleteDefault(domain: $0.domain, key: $0.key) },
    originals: inout [String: String]
) -> EffectsOutcome {
    var outcome = EffectsOutcome()
    for setting in catalog {
        let current = read(setting)
        let original = originals[setting.id].map(decodeDefaultState)
        switch effectChange(setting, current: current, reduce: reduce, original: original) {
        case .leave:
            // Handed back, or taken over by someone else: stop tracking it so a
            // stale record cannot make the guard look active forever.
            if !reduce, let original, current == original || current == .other {
                originals.removeValue(forKey: setting.id)
            }
        case .write(let value):
            if original == nil, let encoded = encodeDefaultState(current) {
                originals[setting.id] = encoded
            }
            if write(setting, value) {
                outcome.changed += 1
            } else {
                outcome.failed.append(setting.title)
            }
        case .restore(let value):
            if write(setting, value) {
                outcome.changed += 1
                originals.removeValue(forKey: setting.id)
            } else {
                outcome.failed.append(setting.title)
            }
        case .restoreMissing:
            if remove(setting) {
                outcome.changed += 1
                originals.removeValue(forKey: setting.id)
            } else {
                outcome.failed.append(setting.title)
            }
        }
    }
    outcome.active = reduce || !originals.isEmpty
    return outcome
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
                    effectsCard
                    audioCard
                    networkCard
                    codecCard
                    chromeCard
                    memoryCard
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
                if d.codecSoftware {
                    Chip(text: "Software decode", tint: .red, active: true)
                } else if d.codecHardwareActive {
                    Chip(text: "T2 offload", tint: .green, active: true)
                }
                if d.chromeGuard && d.chromeStatus == "background stopped" {
                    Chip(text: "Chrome quiet", tint: .indigo, active: true)
                }
                if d.memoryPressure == "critical" {
                    Chip(text: "RAM critical", tint: .red, active: true)
                } else if d.memoryPressure == "high" {
                    Chip(text: "RAM high", tint: .orange, active: true)
                }
                if d.autoTrim {
                    Chip(text: d.autoTrimActive ? "Auto-trim on" : "Auto-trim idle",
                         tint: .purple, active: true)
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

    private var effectsCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "sparkles", text: "Animations & visual effects", trailing: d.effectsStatus)
            Toggle("Reduce motion, blur and window animations",
                   isOn: Binding(get: { d.effectsGuard }, set: { d.setEffectsGuard($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            Text("On in every mode except Off. Switches off Reduce Motion, Reduce Transparency and automatic window animations — the compositing work the Intel Iris Plus would otherwise redraw every frame. Your original values are recorded and put back when this is turned off.")
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if !d.effectsNote.isEmpty {
                Text(d.effectsNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(d.effectsStatus == "partial" ? Color.orange : Color.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .cardSurface()
    }

    private var audioCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "waveform", text: "Audio engine", trailing: d.audioState)
            Toggle("Reset Core Audio when it spikes",
                   isOn: Binding(get: { d.audioGuard }, set: { d.setAudioGuard($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            Text("On in every mode except Off. If coreaudiod stays above 20 % CPU for two samples (about 4 s) — the signature of a stuck engine behind stuttering audio or video — it is asked to restart with SIGTERM and launchd brings it back in about a second. At most once every 10 minutes, three times an hour.")
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if !d.audioNote.isEmpty {
                Text(d.audioNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(d.audioState == "failed" ? Color.orange : Color.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .cardSurface()
    }

    private var networkCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "wifi", text: "Network", trailing: d.netGrade)

            HStack(spacing: 6) {
                Chip(text: d.netLink.capitalized,
                     tint: d.netScore >= 70 ? .green : .orange, active: true)
                Chip(text: "Score \(d.netScore)",
                     tint: d.netScore >= 70 ? .green : .orange, active: true)
                if !d.netInterface.isEmpty {
                    Chip(text: d.netInterface, tint: .secondary, active: false)
                }
                if let r = d.netRSSI {
                    Chip(text: String(format: "%.0f dBm", r), tint: .secondary, active: false)
                }
                if let t = d.netTx {
                    Chip(text: String(format: "%.0f Mbps", t), tint: .secondary, active: false)
                }
                Spacer(minLength: 0)
            }

            if let g = d.netGateway {
                Text(String(format: "Router %.1f ms · %.1f %% loss", g, d.netLoss))
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
            }

            Text(d.netNote)
                .font(.system(size: 10.5))
                .foregroundStyle(d.netScore >= 70 ? Color.secondary : Color.orange)
                .fixedSize(horizontal: false, vertical: true)

            Text("Measured every 30 s and read-only: macOS already auto-tunes TCP, so rewriting the link would risk connectivity for no real gain.")
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .cardSurface()
    }

    private var codecCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "film", text: "T2 video offload", trailing: d.codecStatus)
            Toggle("Prefer hardware H.264/HEVC", isOn: Binding(get: { d.codecGuard }, set: { d.setCodecGuard($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            HStack(spacing: 6) {
                Chip(text: d.codecH264 ? "H.264 HW" : "H.264 SW",
                     tint: d.codecH264 ? .green : .orange, active: true)
                Chip(text: d.codecHEVC ? "HEVC HW" : "HEVC SW",
                     tint: d.codecHEVC ? .green : .orange, active: true)
                Chip(text: d.codecH264ify ? "h264ify installed" : "h264ify missing",
                     tint: d.codecH264ify ? .green : .secondary, active: d.codecH264ify)
                Spacer(minLength: 0)
            }

            Text("Checks VideoToolbox directly, so it works on every Mac model. H.264 and HEVC are decoded/encoded by the T2 on Intel Macs that have one. Runs in every mode except Off.")
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if d.codecOfferH264ify {
                Button {
                    d.openH264ify()
                } label: {
                    Text("Get h264ify…").frame(maxWidth: .infinity)
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
            }

            if !d.codecNote.isEmpty {
                Text(d.codecNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(d.codecSoftware ? Color.orange : Color.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .cardSurface()
    }

    private var chromeCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "globe", text: "Chrome background", trailing: d.chromeStatus)
            Toggle("Stop Chrome when it is not in use",
                   isOn: Binding(get: { d.chromeGuard }, set: { d.setChromeGuard($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            Text("When Chrome has no window, is not frontmost and is not playing audio/video, it is asked to quit. Its background mode is switched off, Google updater/Keystone agents are disabled, and leftover Chrome helpers are stopped. Restored when Chrome is used or this toggle is off.")
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if !d.chromeNote.isEmpty {
                Text(d.chromeNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .cardSurface()
    }

    private var memoryCard: some View {
        VStack(alignment: .leading, spacing: 9) {
            CardTitle(icon: "memorychip", text: "Memory", trailing: d.memoryStatus)

            HStack(spacing: 10) {
                Chip(text: String(format: "Free %.0f%%", d.memoryFree),
                     tint: d.memoryPressure == "normal" ? .green : (d.memoryPressure == "high" ? .orange : .red),
                     active: true)
                Chip(text: "Swap \(d.memorySwap)", tint: .secondary, active: false)
                Chip(text: "Compressed \(d.memoryCompressed)", tint: .secondary, active: false)
                Chip(text: String(format: "Reclaimable %.1f GB", d.memoryReclaimable / 1024), tint: .secondary, active: false)
                Spacer(minLength: 0)
            }

            Toggle("Quit idle apps under memory pressure",
                   isOn: Binding(get: { d.memoryGuard }, set: { d.setMemoryGuard($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            Toggle("Reclaim cache when the kernel runs out",
                   isOn: Binding(get: { d.memoryReclaim }, set: { d.setMemoryReclaim($0) }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .font(.system(size: 12, weight: .medium))

            Text("Runs in every mode except Off. Under high pressure, apps with no window that have been idle for 10+ minutes are asked to quit gracefully (5 minutes at critical pressure). Apps can refuse; nothing is force-killed and no purge is run.")
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if !d.memoryTop.isEmpty {
                VStack(alignment: .leading, spacing: 3) {
                    ForEach(Array(d.memoryTop.enumerated()), id: \.offset) { _, row in
                        let name = row["name"] as? String ?? "?"
                        let mb = row["mb"] as? Double ?? 0
                        HStack(spacing: 6) {
                            Text(name)
                                .font(.system(size: 10.5))
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Spacer(minLength: 6)
                            Text(String(format: "%.0f MB", mb))
                                .font(.system(size: 10, weight: .medium))
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                    }
                }
                .padding(.top, 1)
            }

            if d.memoryTrend >= 0.5 {
                Text(String(format: "Compressor growing %.1f MB/s — reclaiming before it bites", d.memoryTrend))
                    .font(.system(size: 10))
                    .foregroundStyle(.orange)
            }

            if !d.memoryReclaimNote.isEmpty {
                Text(d.memoryReclaimNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if !d.memoryNote.isEmpty {
                Text(d.memoryNote)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
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

            Text("v\(d.appVersion)")
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
                .lineLimit(1)

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
                    ToggleRow(
                        title: "Auto-trim safe background services",
                        subtitle: "On by default and active in every mode except Off. Curated background-only Apple jobs are disabled and their running copies stopped; everything is restored when you switch this off or select Off.",
                        isOn: d.autoTrim
                    ) { d.setAutoTrim($0) }

                    if !d.autoTrimNote.isEmpty {
                        HStack(alignment: .top, spacing: 6) {
                            Image(systemName: "wand.and.stars")
                                .font(.system(size: 10))
                                .foregroundStyle(Color.purple)
                            Text(d.autoTrimNote)
                                .font(.system(size: 10.5))
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(10)
                        .background(Color.purple.opacity(0.08), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    }

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
                        let trimmed = it["auto_trimmed"] as? Bool ?? false
                        let autoSafe = it["auto_safe"] as? Bool ?? false
                        let suffix = trimmed ? " · auto-trimmed" : (autoSafe ? " · auto-safe" : "")
                        ToggleRow(title: id,
                                  subtitle: (it["desc"] as? String ?? "") + suffix,
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
    var chromeGuardPauseUntil: Date?
    var chromeQuitAskedAt: Date?
    var chromeQuitRefusedUntil: Date?

    // Chrome background guard
    @Published var chromeGuard = true
    @Published var chromeStatus = "watching"
    @Published var chromeNote = ""

    // Graceful memory guard
    @Published var memoryGuard = true
    @Published var memoryPressure = "normal"
    @Published var memoryFree = 0.0
    @Published var memorySwap = "—"
    @Published var memoryCompressed = "—"
    @Published var memoryStatus = "watching"
    @Published var memoryNote = ""
    @Published var memoryTop: [[String: Any]] = []
    @Published var memoryReclaim = true
    @Published var memoryReclaimNote = ""
    @Published var memoryReclaims = 0
    @Published var memoryReclaimable = 0.0
    @Published var memoryAdvice = "hold"
    @Published var memoryTrend = 0.0
    var lastMemoryScan: Date?
    var lastChromeScan: Date?

    // T2 / VideoToolbox codec guard
    @Published var codecGuard = true
    @Published var codecH264 = false
    @Published var codecHEVC = false
    @Published var codecStatus = "checking"
    @Published var codecNote = ""
    @Published var codecH264ify = false
    @Published var codecH264ifyEnabled = false
    @Published var codecOfferH264ify = false
    @Published var codecSoftware = false
    @Published var codecHardwareActive = false
    var lastCodecScan: Date?

    // Reduced visual effects: per-user defaults, written by this unprivileged
    // process and tracked so they can always be handed back.
    @Published var effectsGuard = true
    @Published var effectsStatus = "reduced"
    @Published var effectsNote = ""
    var effectsOriginals: [String: String] = [:]
    var lastEffectsScan: Date?
    var lastHelperCheck: Date?
    var lastAccelScan: Date?

    // Core Audio watchdog: the daemon owns the privilege and the decision; the
    // UI only sends a boolean and renders the state it is told.
    @Published var audioGuard = true
    @Published var audioState = "watching"
    @Published var audioNote = ""
    @Published var audioRestarts = 0

    // Network health, measured by the daemon (read-only).
    @Published var netLink = "—"
    @Published var netInterface = ""
    @Published var netRSSI: Double?
    @Published var netTx: Double?
    @Published var netGateway: Double?
    @Published var netLoss = 0.0
    @Published var netScore = 0
    @Published var netGrade = "—"
    @Published var netNote = ""

    @Published var hint = ""
    @Published var software: [[String: Any]] = []
    @Published var macosItems: [[String: Any]] = []
    @Published var macosNote = ""
    @Published var autoTrim = false
    @Published var autoTrimActive = false
    @Published var autoTrimNote = ""

    /// The version read from the bundle at launch.
    let appVersion = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "unknown"

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
        let defaults = UserDefaults.standard
        if defaults.object(forKey: "chromeGuard") != nil {
            chromeGuard = defaults.bool(forKey: "chromeGuard")
        }
        if defaults.object(forKey: "memoryGuard") != nil {
            memoryGuard = defaults.bool(forKey: "memoryGuard")
        }
        if defaults.object(forKey: "codecGuard") != nil {
            codecGuard = defaults.bool(forKey: "codecGuard")
        }
        if defaults.object(forKey: "effectsGuard") != nil {
            effectsGuard = defaults.bool(forKey: "effectsGuard")
        }
        if let raw = defaults.dictionary(forKey: "visualEffectsOriginals") {
            effectsOriginals = raw.compactMapValues { $0 as? String }
        }
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

    /// Opt-in, daemon-side and Auto-only. The daemon owns the allowlist and
    /// the restore bookkeeping; the UI only sends the boolean.
    func setAutoTrim(_ on: Bool) {
        autoTrim = on
        if !on { autoTrimActive = false }
        _ = sendRequest("{\"cmd\":\"auto_trim\",\"value\":\(on)}")
        refresh()
    }

    func setChromeGuard(_ on: Bool) {
        chromeGuard = on
        UserDefaults.standard.set(on, forKey: "chromeGuard")
        chromeNote = ""
        refreshChromeGuard()
    }

    func setMemoryGuard(_ on: Bool) {
        memoryGuard = on
        UserDefaults.standard.set(on, forKey: "memoryGuard")
        memoryNote = ""
        refreshMemoryGuard()
    }

    /// OS-level cache reclaim: default on, every mode except Off. The daemon
    /// enforces the cooldown and the hourly cap; the UI only sends a boolean.
    func setMemoryReclaim(_ on: Bool) {
        memoryReclaim = on
        _ = sendRequest("{\"cmd\":\"memory_reclaim\",\"value\":\(on)}")
        refresh()
    }

    func setCodecGuard(_ on: Bool) {
        codecGuard = on
        UserDefaults.standard.set(on, forKey: "codecGuard")
        codecNote = ""
        refreshCodecGuard()
    }

    /// Turning the guard off restores immediately rather than on the next tick.
    func setEffectsGuard(_ on: Bool) {
        effectsGuard = on
        UserDefaults.standard.set(on, forKey: "effectsGuard")
        effectsNote = ""
        lastEffectsScan = nil
        refreshVisualEffects()
    }

    /// Default on, every mode except Off. Nothing caller-supplied reaches the
    /// signal: the daemon signals the compiled-in `coreaudiod` name only.
    func setAudioGuard(_ on: Bool) {
        audioGuard = on
        _ = sendRequest("{\"cmd\":\"audio_guard\",\"value\":\(on)}")
        refresh()
    }

    func openH264ify() {
        if let url = URL(string: "https://chrome.google.com/webstore/detail/h264ify/aleakchihdccplidncghkekgioiakgal") {
            NSWorkspace.shared.open(url)
        }
    }

    /// Chrome is only touched when it is genuinely idle. The quit request is
    /// asynchronous: we ask once, then wait a few ticks before writing Chrome's
    /// config, so Chrome cannot overwrite the change on its way out.
    func refreshChromeGuard() {
        if let pause = chromeGuardPauseUntil, pause > Date() {
            chromeStatus = "paused"
            return
        }
        let sample = cpuSample()
        let s = chromeGuardState(sample: sample)
        let managed = chromeBgManaged() || !chromeManagedAgents().isEmpty
        switch chromeGuardAction(s, enabled: chromeGuard, managed: managed) {
        case .nothing:
            chromeStatus = chromeGuard ? (s.active ? "Chrome in use" : "watching") : "off"
        case .restore:
            var did = false
            if !chromeManagedAgents().isEmpty, restoreGoogleAgents() { did = true }
            // Leave Local State alone while Chrome is active; restore it when
            // the guard is off or Chrome is closed.
            if !chromeGuard || !s.running, restoreChromeBackground() { did = true }
            chromeStatus = chromeGuard ? (s.active ? "Chrome in use" : "watching") : "off"
            if did { chromeNote = "Restored Chrome's background setting and Google agents." }
        case .trim:
            if s.running {
                if let refused = chromeQuitRefusedUntil, refused > Date() {
                    chromeStatus = "Chrome refused to quit"
                    return
                }
                if let asked = chromeQuitAskedAt {
                    if Date().timeIntervalSince(asked) < 30 {
                        chromeStatus = "quitting"
                        return
                    }
                    chromeQuitAskedAt = nil
                    chromeQuitRefusedUntil = Date().addingTimeInterval(1800)
                    chromeStatus = "Chrome did not quit"
                    chromeNote = "Chrome is idle but did not quit within 30 s; left alone for 30 minutes."
                    return
                }
                chromeQuitAskedAt = Date()
                for app in NSRunningApplication.runningApplications(withBundleIdentifier: chromeBundleID) {
                    app.terminate()
                }
                chromeStatus = "quitting"
                chromeNote = "Chrome is idle; asked it to quit gracefully."
                return
            }

            chromeQuitAskedAt = nil
            var did = false
            let bg = chromeBackgroundMode()
            if bg.readable && bg.enabled != false, disableChromeBackground() { did = true }
            let managedAgents = Set(chromeManagedAgents())
            if !googleBackgroundAgents.allSatisfy({ managedAgents.contains($0) }) {
                if !disableGoogleAgents().isEmpty { did = true }
            }
            if !s.helperPids.isEmpty {
                let stopped = stopChromeHelpers()
                if stopped > 0 {
                    chromeNote = "Stopped \(stopped) leftover Chrome helper(s)."
                    did = true
                }
            }
            chromeStatus = "background stopped"
            if did && chromeNote.isEmpty {
                chromeNote = "Chrome background mode off; Google agents disabled."
            }
        }
    }

    func refreshMemoryGuard() {
        let stats = memoryStats()
        memoryFree = stats.freePercent
        memorySwap = stats.swapTotalMB > 0
            ? String(format: "%.0f/%.0f MB", stats.swapUsedMB, stats.swapTotalMB)
            : "—"
        memoryCompressed = stats.totalMB > 0
            ? String(format: "%.1f GB", stats.compressedMB / 1024.0)
            : "—"
        if !helperRunning { memoryPressure = stats.pressure.rawValue }
        memoryTop = topMemoryApps(3)

        guard memoryGuard else { memoryStatus = "off"; return }
        guard stats.readable else { memoryStatus = "unreadable"; return }
        guard mode != "off" else { memoryStatus = "off (mode)"; return }

        let now = Date()
        let nowEpoch = now.timeIntervalSince1970
        let apps = memoryAppStates(sample: cpuSample())
        var last = doubleMap("memoryLastActive")
        var refused = doubleMap("memoryRefused")
        refused = refused.filter { $0.value > nowEpoch }
        saveDoubleMap("memoryRefused", refused)

        for app in apps {
            if app.frontmost || app.hasWindow || app.audio || app.media || app.cpu >= 3.0 {
                last[app.bundleID] = nowEpoch
            } else if last[app.bundleID] == nil {
                last[app.bundleID] = nowEpoch
            }
        }
        let live = Set(apps.map { $0.bundleID })
        last = last.filter { live.contains($0.key) }
        saveDoubleMap("memoryLastActive", last)

        guard stats.pressure != .normal else {
            memoryStatus = "normal"
            return
        }

        let candidates = apps.compactMap { app -> (MemoryAppState, Double)? in
            let idle = nowEpoch - (last[app.bundleID] ?? nowEpoch)
            let isRefused = (refused[app.bundleID] ?? 0) > nowEpoch
            let ok = shouldQuitIdleApp(
                app,
                pressure: stats.pressure,
                enabled: true,
                idleFor: idle,
                refused: isRefused
            )
            return ok ? (app, idle) : nil
        }.sorted { $0.1 > $1.1 }

        guard let (victim, idle) = candidates.first else {
            memoryStatus = stats.pressure == .critical ? "critical · no idle apps" : "high · no idle apps"
            return
        }
        guard let running = NSRunningApplication(processIdentifier: victim.pid) else { return }
        if running.terminate() {
            memoryStatus = "asked \(victim.name) to quit"
            memoryNote = "Graceful quit requested for \(victim.name) after \(Int(idle / 60)) min idle; it can refuse or ask you to save."
        } else {
            memoryStatus = "\(victim.name) refused to quit"
            memoryNote = "Left \(victim.name) alone for 30 minutes."
        }
        // Never nag: either it is shutting down or it refused. Re-evaluate in 30 minutes.
        refused[victim.bundleID] = nowEpoch + 1800
        saveDoubleMap("memoryRefused", refused)
    }

    func refreshCodecGuard() {
        let sample = cpuSample()
        let pulse = mediaPulse(sample: sample)
        let hw = codecHardwareSupport()
        codecH264 = hw.h264Decode && hw.h264Encode
        codecHEVC = hw.hevcDecode && hw.hevcEncode

        let hardwareActive = cpuFor("VTDecoderXPCService", in: sample) >= VIDEO_DECODE_CPU
            || cpuFor("VTEncoderXPCService", in: sample) >= VIDEO_DECODE_CPU
            || cpuFor("AVConference", in: sample) >= VIDEO_DECODE_CPU
            || acceleratedApps.contains { cpuFor($0.gpuHelperTag, in: sample) >= APP_IN_USE_CPU }

        let verdict = codecVerdict(
            h264: hw.h264Decode,
            hevc: hw.hevcDecode,
            videoActive: pulse.video,
            hardwareActive: hardwareActive
        )
        let ext = h264ifyStatusAll()
        codecH264ify = ext.installed
        codecH264ifyEnabled = ext.enabled
        codecSoftware = verdict == .softwareDecode
        codecHardwareActive = verdict == .hardwareOffload

        guard codecGuard else { codecStatus = "off"; return }
        guard mode != "off" else { codecStatus = "off (mode)"; return }

        switch verdict {
        case .unavailable:
            codecStatus = "no H.264 hardware decode"
            codecNote = "VideoToolbox reports no hardware H.264 decoder on this Mac."
        case .ready:
            codecStatus = "ready"
            codecNote = hw.hevcDecode
                ? "H.264 and HEVC hardware decode are available."
                : "H.264 hardware decode is available."
        case .hardwareOffload:
            codecStatus = "T2 offload active"
            codecNote = "VideoToolbox is doing the decode/encode work in hardware."
        case .softwareDecode:
            codecStatus = "software decode"
            if ext.installed && ext.enabled {
                codecNote = "h264ify is installed and enabled; this site may still be serving a codec the T2 cannot decode. Try reloading the video."
            } else if ext.installed {
                codecNote = "h264ify is installed but not enabled for this profile. Enable it to force H.264."
            } else {
                codecNote = "This site is using a codec the T2 cannot decode in hardware. h264ify makes sites serve H.264 instead."
            }
            codecOfferH264ify = !ext.installed
        }
    }

    /// Keeps the reduced-effects profile in step with the cooling mode. A steady
    /// state costs three reads and no writes, so this is safe on a timer.
    func refreshVisualEffects() {
        let reduce = effectsDesired(enabled: effectsGuard, mode: mode)
        var originals = effectsOriginals
        let outcome = reconcileVisualEffects(catalog: visualEffectCatalog, reduce: reduce, originals: &originals)
        if originals != effectsOriginals {
            effectsOriginals = originals
            UserDefaults.standard.set(originals, forKey: "visualEffectsOriginals")
        }
        if !outcome.failed.isEmpty {
            effectsStatus = "partial"
            effectsNote = "Could not change \(outcome.failed.joined(separator: ", ")); everything else was left as it was."
        } else if reduce {
            effectsStatus = "reduced"
            if outcome.changed > 0 {
                effectsNote = "Motion, transparency and window animations are off to free GPU time. Applies as apps redraw; your original values are recorded."
            }
        } else {
            effectsStatus = effectsGuard ? "off (mode)" : "off"
            if outcome.changed > 0 {
                effectsNote = "Your original motion, transparency and animation settings were put back."
            }
        }
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
            if ok {
                accelPending.remove(app.bundleID)
                // Chrome may be restarting as part of the repair; give it time
                // to come back before the background guard looks at it again.
                if s.running { chromeGuardPauseUntil = Date().addingTimeInterval(30) }
            }
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
                    // A relaunch is a repair in progress, not an idle browser.
                    if relaunch { chromeGuardPauseUntil = Date().addingTimeInterval(30) }
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

    /// Cadence-limited user-space guards. Each one is cheap enough to run on
    /// demand from the UI, but not on every 2 s status tick.
    func runUserSpaceGuards() {
        let now = Date()
        if lastAccelScan.map({ now.timeIntervalSince($0) >= 8 }) ?? true {
            lastAccelScan = now
            refreshAccelerationGuard()
        }
        if lastChromeScan.map({ now.timeIntervalSince($0) >= 8 }) ?? true {
            lastChromeScan = now
            refreshChromeGuard()
        }
        if lastMemoryScan.map({ now.timeIntervalSince($0) >= 15 }) ?? true {
            lastMemoryScan = now
            refreshMemoryGuard()
        }
        if lastCodecScan.map({ now.timeIntervalSince($0) >= 10 }) ?? true {
            lastCodecScan = now
            refreshCodecGuard()
        }
        if lastEffectsScan.map({ now.timeIntervalSince($0) >= 15 }) ?? true {
            lastEffectsScan = now
            refreshVisualEffects()
        }
    }

    func refresh() {
        // `launchctl print` is a fork; the helper's presence does not change
        // every two seconds, so check it on a slow cadence.
        let now = Date()
        if lastHelperCheck.map({ now.timeIntervalSince($0) >= 30 }) ?? true {
            lastHelperCheck = now
            helperPresent = helperInstalled()
        }
        loginOn = loginEnabled()

        guard let s = requestStatus() else {
            helperRunning = false
            statusItem?.button?.title = "—°"
            temp = nil
            fan = nil
            // Chrome/memory/codec guards do not need the helper; keep them
            // running so an uninstalled or stopped helper cannot silently
            // re-enable Chrome background mode.
            runUserSpaceGuards()
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
        autoTrim = s["auto_trim"] as? Bool ?? true
        autoTrimActive = s["auto_trim_active"] as? Bool ?? false
        autoTrimNote = s["auto_trim_note"] as? String ?? ""
        if let a = s["audio_guard"] as? Bool { audioGuard = a }
        audioState = s["audio_state"] as? String ?? "watching"
        audioNote = s["audio_note"] as? String ?? ""
        audioRestarts = s["audio_restarts"] as? Int ?? 0
        netLink = s["net_link"] as? String ?? "—"
        netInterface = s["net_interface"] as? String ?? ""
        netRSSI = s["net_rssi"] as? Double
        netTx = s["net_tx_mbps"] as? Double
        netGateway = s["net_gateway_ms"] as? Double
        netLoss = s["net_loss_pct"] as? Double ?? 0
        netScore = s["net_score"] as? Int ?? 0
        netGrade = s["net_grade"] as? String ?? "—"
        netNote = s["net_note"] as? String ?? ""
        if let r = s["memory_reclaim"] as? Bool { memoryReclaim = r }
        memoryReclaimNote = s["memory_note"] as? String ?? ""
        memoryReclaims = s["memory_reclaims"] as? Int ?? 0
        memoryReclaimable = s["memory_reclaimable_mb"] as? Double ?? 0
        memoryAdvice = s["memory_advice"] as? String ?? "hold"
        memoryTrend = s["memory_trend_mb_s"] as? Double ?? 0
        // The daemon samples the kernel's own VM counters every 2 s; trust that
        // verdict over the UI's coarser reading when the helper is running.
        if let p = s["memory_pressure"] as? String, helperRunning { memoryPressure = p }

        // The three user-space guards are heavier than reading daemon status,
        // so each runs on its own cadence. Chrome is checked often enough to
        // notice that a graceful quit finished; memory and codec are slower.
        runUserSpaceGuards()

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
    var total = 0
    func expect(_ name: String, _ got: AccelAction, _ want: AccelAction) {
        total += 1
        if got != want {
            failures += 1
            print("FAIL \(name): got \(got), want \(want)")
        } else {
            print("ok   \(name)")
        }
    }
    func expectEq<T: Equatable>(_ name: String, _ got: T, _ want: T) {
        total += 1
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

    // ---- Chrome background guard policy ----
    var chrome = ChromeGuardState()
    chrome.installed = true
    expectEq("chrome: idle is trimmed", chromeGuardAction(chrome, enabled: true, managed: false), .trim)
    chrome.running = true
    chrome.hasWindow = true
    expectEq("chrome: window means in use", chromeGuardAction(chrome, enabled: true, managed: false), .nothing)
    expectEq("chrome: window restores managed state", chromeGuardAction(chrome, enabled: true, managed: true), .restore)
    chrome.hasWindow = false
    chrome.frontmost = true
    expectEq("chrome: frontmost means in use", chromeGuardAction(chrome, enabled: true, managed: false), .nothing)
    chrome.frontmost = false
    chrome.launching = true
    expectEq("chrome: still launching means in use", chromeGuardAction(chrome, enabled: true, managed: false), .nothing)
    chrome.launching = false
    chrome.audio = true
    expectEq("chrome: audio means in use", chromeGuardAction(chrome, enabled: true, managed: false), .nothing)
    chrome.audio = false
    chrome.media = true
    expectEq("chrome: media means in use", chromeGuardAction(chrome, enabled: true, managed: false), .nothing)
    chrome.media = false
    expectEq("chrome: disabled restores managed state", chromeGuardAction(chrome, enabled: false, managed: true), .restore)
    expectEq("chrome: disabled and unmanaged is a no-op", chromeGuardAction(chrome, enabled: false, managed: false), .nothing)
    let missingChrome = ChromeGuardState()
    expectEq("chrome: missing install restores", chromeGuardAction(missingChrome, enabled: true, managed: true), .restore)
    expectEq("chrome: missing install is otherwise inert", chromeGuardAction(missingChrome, enabled: true, managed: false), .nothing)

    expectEq("chrome helper: renderer path accepted", isChromeHelperPath("/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)"), true)
    expectEq("chrome helper: crashpad accepted", isChromeHelperPath("/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/chrome_crashpad_handler"), true)
    expectEq("chrome helper: main binary rejected", isChromeHelperPath(chromeMainExecutable), false)
    expectEq("chrome helper: unrelated app rejected", isChromeHelperPath("/Applications/Other.app/Contents/MacOS/Helper"), false)

    // ---- Memory guard policy ----
    let mem = MemoryAppState(bundleID: "com.example.idle", name: "Idle", pid: 42)
    expectEq("memory: normal pressure never quits", shouldQuitIdleApp(mem, pressure: .normal, enabled: true, idleFor: 99999, refused: false), false)
    expectEq("memory: high pressure waits 10 min", shouldQuitIdleApp(mem, pressure: .high, enabled: true, idleFor: 599, refused: false), false)
    expectEq("memory: high pressure acts after 10 min", shouldQuitIdleApp(mem, pressure: .high, enabled: true, idleFor: 600, refused: false), true)
    expectEq("memory: critical waits 5 min", shouldQuitIdleApp(mem, pressure: .critical, enabled: true, idleFor: 299, refused: false), false)
    expectEq("memory: critical acts after 5 min", shouldQuitIdleApp(mem, pressure: .critical, enabled: true, idleFor: 300, refused: false), true)
    expectEq("memory: disabled is inert", shouldQuitIdleApp(mem, pressure: .critical, enabled: false, idleFor: 99999, refused: false), false)
    expectEq("memory: a refusal is respected", shouldQuitIdleApp(mem, pressure: .critical, enabled: true, idleFor: 99999, refused: true), false)
    var protected = mem; protected.protectedApp = true
    expectEq("memory: protected apps are never quit", shouldQuitIdleApp(protected, pressure: .critical, enabled: true, idleFor: 99999, refused: false), false)
    var windowed = mem; windowed.hasWindow = true
    expectEq("memory: apps with windows are never quit", shouldQuitIdleApp(windowed, pressure: .critical, enabled: true, idleFor: 99999, refused: false), false)
    var front = mem; front.frontmost = true
    expectEq("memory: frontmost app is never quit", shouldQuitIdleApp(front, pressure: .critical, enabled: true, idleFor: 99999, refused: false), false)
    var audio = mem; audio.audio = true
    expectEq("memory: audio apps are never quit", shouldQuitIdleApp(audio, pressure: .critical, enabled: true, idleFor: 99999, refused: false), false)
    var media = mem; media.media = true
    expectEq("memory: camera users are never quit", shouldQuitIdleApp(media, pressure: .critical, enabled: true, idleFor: 99999, refused: false), false)
    var busy = mem; busy.cpu = 3.0
    expectEq("memory: busy apps are never quit", shouldQuitIdleApp(busy, pressure: .critical, enabled: true, idleFor: 99999, refused: false), false)

    expectEq("memory: healthy machine is normal", memoryPressureLevel(freePercent: 50, swapUsedPercent: 0, compressedPercent: 0), .normal)
    expectEq("memory: low free RAM is high", memoryPressureLevel(freePercent: 10, swapUsedPercent: 0, compressedPercent: 0), .high)
    expectEq("memory: very low free RAM is critical", memoryPressureLevel(freePercent: 5, swapUsedPercent: 0, compressedPercent: 0), .critical)
    expectEq("memory: full swap is critical", memoryPressureLevel(freePercent: 50, swapUsedPercent: 80, compressedPercent: 0), .critical)
    expectEq("memory: heavy compression is critical", memoryPressureLevel(freePercent: 50, swapUsedPercent: 0, compressedPercent: 60), .critical)
    expectEq("memory: unreadable pressure is critical", memoryPressureLevel(freePercent: .nan, swapUsedPercent: 0, compressedPercent: 0), .critical)

    // ---- Reduced visual effects policy ----
    expectEq("effects: on by default in Auto", effectsDesired(enabled: true, mode: "auto"), true)
    expectEq("effects: follow a capped mode", effectsDesired(enabled: true, mode: "coolquiet"), true)
    expectEq("effects: off when the toggle is off", effectsDesired(enabled: false, mode: "auto"), false)
    expectEq("effects: Off mode hands everything back", effectsDesired(enabled: true, mode: "off"), false)

    let motion = visualEffectCatalog[0]
    expectEq("effects: missing key is written",
             effectChange(motion, current: .missing, reduce: true, original: nil),
             EffectChange.write(motion.reduced))
    expectEq("effects: already reduced is left alone",
             effectChange(motion, current: .flag(motion.reduced), reduce: true, original: nil),
             EffectChange.leave)
    expectEq("effects: opposite value is rewritten",
             effectChange(motion, current: .flag(!motion.reduced), reduce: true, original: nil),
             EffectChange.write(motion.reduced))
    expectEq("effects: a foreign type is never overwritten",
             effectChange(motion, current: .other, reduce: true, original: nil),
             EffectChange.leave)
    expectEq("effects: recorded boolean is restored",
             effectChange(motion, current: .flag(motion.reduced), reduce: false, original: .flag(!motion.reduced)),
             EffectChange.restore(!motion.reduced))
    expectEq("effects: an unchanged key needs no restore",
             effectChange(motion, current: .flag(!motion.reduced), reduce: false, original: .flag(!motion.reduced)),
             EffectChange.leave)
    expectEq("effects: an originally absent key is deleted again",
             effectChange(motion, current: .flag(motion.reduced), reduce: false, original: .missing),
             EffectChange.restoreMissing)
    expectEq("effects: a foreign value is never restored over",
             effectChange(motion, current: .other, reduce: false, original: .flag(false)),
             EffectChange.leave)
    expectEq("effects: nothing recorded means no restore",
             effectChange(motion, current: .flag(true), reduce: false, original: nil),
             EffectChange.leave)
    expectEq("effects: catalog ids are unique",
             Set(visualEffectCatalog.map { $0.id }).count, visualEffectCatalog.count)
    expectEq("effects: every catalog entry names a domain, key and title",
             visualEffectCatalog.allSatisfy { !$0.domain.isEmpty && !$0.key.isEmpty && !$0.title.isEmpty },
             true)

    // End to end over the real defaults store, against a scratch domain so no
    // actual preference is touched: apply, read back, restore, forget.
    let scratch = "com.thermalguard.selftest.\(getpid())"
    let probe = VisualEffectSetting(id: "probe", domain: scratch, key: "probe",
                                    title: "Probe", reduced: true)
    var recorded: [String: String] = [:]
    let applied = reconcileVisualEffects(catalog: [probe], reduce: true, originals: &recorded)
    expectEq("effects: apply writes the reduced value", applied.changed, 1)
    expectEq("effects: applied value reads back", readDefault(domain: scratch, key: "probe"), .flag(true))
    expectEq("effects: absent key is remembered as absent", recorded["probe"], "missing")
    let steady = reconcileVisualEffects(catalog: [probe], reduce: true, originals: &recorded)
    expectEq("effects: a steady state writes nothing", steady.changed, 0)
    let restored = reconcileVisualEffects(catalog: [probe], reduce: false, originals: &recorded)
    expectEq("effects: restore changes it back", restored.changed, 1)
    expectEq("effects: restored key is absent again", readDefault(domain: scratch, key: "probe"), .missing)
    expectEq("effects: restore forgets its record", recorded.isEmpty, true)
    _ = shell("/usr/bin/defaults", ["delete", scratch])

    // ---- T2 / VideoToolbox codec policy ----
    expectEq("codec: no H.264 decoder is unavailable", codecVerdict(h264: false, hevc: true, videoActive: false, hardwareActive: true), CodecVerdict.unavailable)
    expectEq("codec: idle with H.264 is ready", codecVerdict(h264: true, hevc: true, videoActive: false, hardwareActive: false), CodecVerdict.ready(h264: true, hevc: true))
    expectEq("codec: idle H.264 only is ready", codecVerdict(h264: true, hevc: false, videoActive: false, hardwareActive: false), CodecVerdict.ready(h264: true, hevc: false))
    expectEq("codec: active hardware work is offload", codecVerdict(h264: true, hevc: true, videoActive: true, hardwareActive: true), CodecVerdict.hardwareOffload)
    expectEq("codec: active without hardware work is software", codecVerdict(h264: true, hevc: true, videoActive: true, hardwareActive: false), CodecVerdict.softwareDecode)

    print(failures == 0 ? "PASS \(total)/\(total)" : "FAILED \(failures)/\(total)")
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
if CommandLine.arguments.contains("--chrome-check") {
    let s = chromeGuardState(sample: cpuSample())
    let managed = chromeBgManaged() || !chromeManagedAgents().isEmpty
    let action: String
    switch chromeGuardAction(s, enabled: true, managed: managed) {
    case .nothing: action = "nothing"
    case .trim: action = "trim"
    case .restore: action = "restore"
    }
    print("{\"installed\":\(s.installed),\"running\":\(s.running),\"active\":\(s.active),\"has_window\":\(s.hasWindow),\"audio\":\(s.audio),\"media\":\(s.media),\"bg_readable\":\(s.bgReadable),\"bg_enabled\":\(s.bgEnabled.map { $0 ? "true" : "false" } ?? "null"),\"helpers\":\(s.helperPids.count),\"managed\":\(managed),\"action\":\"\(action)\"}")
    exit(0)
}
if CommandLine.arguments.contains("--memory-check") {
    let stats = memoryStats()
    let apps = memoryAppStates(sample: cpuSample())
    let protected = apps.filter { $0.protectedApp }.count
    let idle = apps.filter { !$0.protectedApp && !$0.hasWindow && !$0.frontmost && !$0.audio && !$0.media && $0.cpu < 3.0 }.count
    print("{\"readable\":\(stats.readable),\"free_percent\":\(Int(stats.freePercent)),\"swap_used_mb\":\(Int(stats.swapUsedMB)),\"swap_total_mb\":\(Int(stats.swapTotalMB)),\"compressed_mb\":\(Int(stats.compressedMB)),\"pressure\":\"\(stats.pressure.rawValue)\",\"apps\":\(apps.count),\"protected\":\(protected),\"idle_candidates\":\(idle)}")
    exit(0)
}
if CommandLine.arguments.contains("--effects-check") {
    let q = String(UnicodeScalar(34))
    var rows: [String] = []
    var reduced = 0
    for setting in visualEffectCatalog {
        let state = readDefault(domain: setting.domain, key: setting.key)
        let value: String
        switch state {
        case .missing: value = "null"
        case .flag(let on): value = on ? "true" : "false"
        case .other: value = "other"
        }
        if state == .flag(setting.reduced) { reduced += 1 }
        rows.append("{" + q + "id" + q + ":" + q + setting.id + q
                    + "," + q + "domain" + q + ":" + q + setting.domain + q
                    + "," + q + "key" + q + ":" + q + setting.key + q
                    + "," + q + "value" + q + ":" + value
                    + "," + q + "reduced" + q + ":" + (setting.reduced ? "true" : "false") + "}")
    }
    let settings = rows.joined(separator: ",")
    print("{" + q + "reduced" + q + ":" + String(reduced)
          + "," + q + "total" + q + ":" + String(visualEffectCatalog.count)
          + "," + q + "settings" + q + ":[" + settings + "]}")
    exit(0)
}

if CommandLine.arguments.contains("--codec-check") {
    let hw = codecHardwareSupport()
    let sample = cpuSample()
    let pulse = mediaPulse(sample: sample)
    let hardwareActive = cpuFor("VTDecoderXPCService", in: sample) >= VIDEO_DECODE_CPU
        || cpuFor("VTEncoderXPCService", in: sample) >= VIDEO_DECODE_CPU
        || cpuFor("AVConference", in: sample) >= VIDEO_DECODE_CPU
        || acceleratedApps.contains { cpuFor($0.gpuHelperTag, in: sample) >= APP_IN_USE_CPU }
    let verdict = codecVerdict(h264: hw.h264Decode, hevc: hw.hevcDecode, videoActive: pulse.video, hardwareActive: hardwareActive)
    let label: String
    switch verdict {
    case .unavailable: label = "unavailable"
    case .ready: label = "ready"
    case .hardwareOffload: label = "hardware"
    case .softwareDecode: label = "software"
    }
    let ext = h264ifyStatusAll()
    print("{\"h264_decode\":\(hw.h264Decode),\"h264_encode\":\(hw.h264Encode),\"hevc_decode\":\(hw.hevcDecode),\"hevc_encode\":\(hw.hevcEncode),\"video\":\(pulse.video),\"verdict\":\"\(label)\",\"h264ify_installed\":\(ext.installed),\"h264ify_enabled\":\(ext.enabled)}")
    exit(0)
}
let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
