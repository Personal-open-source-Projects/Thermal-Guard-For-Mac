import AppKit
import CoreMediaIO
import Foundation

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

final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

// MARK: - App

final class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
    var window: NSWindow?
    var timer: Timer?

    // dashboard widgets
    let tempLabel = NSTextField(labelWithString: "—")
    let fanLabel = NSTextField(labelWithString: "fan —")
    let stateLabel = NSTextField(labelWithString: "connecting…")
    let hintLabel = NSTextField(wrappingLabelWithString: "")
    let detailView = NSTextField(wrappingLabelWithString: "")
    let modeLabels = ["Auto", "Off", "Balanced", "Cool & Quiet", "Max Cool"]
    let modeValues = ["auto", "off", "balanced", "coolquiet", "maxcool"]
    lazy var modeControl = NSSegmentedControl(labels: modeLabels,
                                              trackingMode: .selectOne, target: nil, action: nil)
    let fanLabels = ["SMC curve", "Silent", "Maximum"]
    let fanValues = ["auto", "quiet", "max"]
    lazy var fanControl = NSSegmentedControl(labels: fanLabels,
                                             trackingMode: .selectOne, target: nil, action: nil)
    let installButton = NSButton(title: "Install Thermal Helper…", target: nil, action: nil)
    let loginButton = NSButton(checkboxWithTitle: "Start ThermalGuard at login", target: nil, action: nil)
    let mediaCheck = NSButton(checkboxWithTitle: "Shield video & call apps (automatic: no throttling, no power cap below 84 °C)", target: nil, action: nil)
    let accelLabel = NSTextField(wrappingLabelWithString: "")
    let accelButton = NSButton(title: "Fix now…", target: nil, action: nil)
    /// Apps that were seen decoding video / on a camera call while their
    /// acceleration was off: the repair is owed, and lands when the call ends.
    var accelPending: Set<String> = []
    var accelNote = ""
    let manageButton = NSButton(title: "Manage installed software…", target: nil, action: nil)
    let macosButton = NSButton(title: "macOS background services…", target: nil, action: nil)
    let quitButton = NSButton(title: "Quit", target: nil, action: nil)

    var softwareWindow: NSWindow?
    var softwareStack: NSStackView?
    var agentItems: [[String: Any]] = []

    var macosWindow: NSWindow?
    var macosStack: NSStackView?
    var macosItems: [[String: Any]] = []
    var macosNote = ""

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildStatusItem()
        buildWindow()
        refresh()
        timer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.refresh()
        }
        RunLoop.main.add(timer!, forMode: .common)
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
            w.orderOut(nil)
        } else {
            refresh()
            w.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
        }
    }

    // MARK: window

    func buildWindow() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 560, height: 640),
                         styleMask: [.titled, .closable, .miniaturizable],
                         backing: .buffered, defer: false)
        w.title = "ThermalGuard — keep this Mac cool"
        w.center()
        w.isReleasedWhenClosed = false

        let bg = NSVisualEffectView()
        bg.material = .underWindowBackground
        bg.blendingMode = .behindWindow
        bg.state = .active
        w.contentView = bg

        let title = NSTextField(labelWithString: "ThermalGuard")
        title.font = NSFont.systemFont(ofSize: 20, weight: .semibold)

        tempLabel.font = NSFont.monospacedDigitSystemFont(ofSize: 44, weight: .bold)
        fanLabel.font = NSFont.monospacedDigitSystemFont(ofSize: 14, weight: .regular)
        stateLabel.font = NSFont.systemFont(ofSize: 12)
        stateLabel.textColor = .secondaryLabelColor

        modeControl.target = self
        modeControl.action = #selector(modeChanged)
        modeControl.selectedSegment = 0

        fanControl.target = self
        fanControl.action = #selector(fanChanged)
        fanControl.selectedSegment = 0

        installButton.target = self
        installButton.action = #selector(installHelper)
        installButton.bezelStyle = .rounded

        loginButton.target = self
        loginButton.action = #selector(toggleLogin)
        loginButton.state = loginEnabled() ? .on : .off

        mediaCheck.target = self
        mediaCheck.action = #selector(mediaGuardChanged)
        mediaCheck.state = .on

        accelLabel.font = NSFont.systemFont(ofSize: 11)
        accelLabel.textColor = .secondaryLabelColor
        accelLabel.preferredMaxLayoutWidth = 500
        accelButton.target = self
        accelButton.action = #selector(fixAccelerationNow)
        accelButton.bezelStyle = .rounded
        accelButton.isHidden = true

        manageButton.target = self
        manageButton.action = #selector(openSoftware)
        manageButton.bezelStyle = .rounded

        macosButton.target = self
        macosButton.action = #selector(openMacOS)
        macosButton.bezelStyle = .rounded

        quitButton.target = self
        quitButton.action = #selector(quitApp)
        quitButton.bezelStyle = .rounded

        hintLabel.stringValue = ""
        hintLabel.font = NSFont.systemFont(ofSize: 11)
        hintLabel.textColor = .secondaryLabelColor
        hintLabel.preferredMaxLayoutWidth = 500

        detailView.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        detailView.preferredMaxLayoutWidth = 500
        detailView.isSelectable = true

        let buttons = NSStackView(views: [installButton, manageButton, macosButton, loginButton, quitButton])
        buttons.orientation = .horizontal
        buttons.spacing = 8

        let stack = NSStackView(views: [title, tempLabel, fanLabel, stateLabel,
                                        modeControl, fanControl, mediaCheck, accelLabel, accelButton, hintLabel,
                                        separator(), detailView, buttons])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false

        bg.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: bg.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: bg.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: bg.topAnchor, constant: 20),
            detailView.widthAnchor.constraint(equalTo: stack.widthAnchor),
        ])
        w.setContentSize(NSSize(width: 560, height: 660))
        window = w
    }

    func separator() -> NSBox {
        let box = NSBox()
        box.boxType = .separator
        box.translatesAutoresizingMaskIntoConstraints = false
        box.widthAnchor.constraint(equalToConstant: 500).isActive = true
        return box
    }

    // MARK: actions

    @objc func modeChanged() {
        // Only react to a real click. Programmatic segment updates during
        // refresh() must never push a mode back to the daemon.
        guard NSApp.currentEvent != nil else { return }
        let i = modeControl.selectedSegment
        if i >= 0 && i < modeValues.count { sendMode(modeValues[i]) }
        refresh()
    }

    @objc func fanChanged() {
        // Same rule as the mode control: ignore programmatic updates.
        guard NSApp.currentEvent != nil else { return }
        let i = fanControl.selectedSegment
        if i >= 0 && i < fanValues.count { sendFan(fanValues[i]) }
        refresh()
    }

    @objc func mediaGuardChanged() {
        guard NSApp.currentEvent != nil else { return }
        let on = mediaCheck.state == .on
        _ = sendRequest("{\"cmd\":\"media_guard\",\"value\":\"\(on ? "on" : "off")\"}")
        refresh()
    }

    /// User override for the one case the guard must not take itself: a repair
    /// that needs the app restarted while a call or video is running.
    @objc func fixAccelerationNow() {
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
        var offerFix = false

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
                    lines.append("\(app.name): acceleration off, waiting for a video or call to fix it")
                }
            case .hold(let why):
                lines.append("\(app.name): acceleration off — \(why)")
                offerFix = pulse.active
            case .enableNow(let relaunch):
                let (ok, msg) = enableAcceleration(app, relaunch: relaunch)
                if ok {
                    accelPending.remove(app.bundleID)
                    lines.append("Fixed: \(msg)")
                } else {
                    lines.append("\(app.name): \(msg)")
                    offerFix = true
                }
            }
        }

        if !accelNote.isEmpty { lines.append(accelNote) }
        accelLabel.stringValue = lines.joined(separator: "\n")
        accelButton.isHidden = !offerFix
        accelNote = ""
    }

    @objc func quitApp() { NSApp.terminate(nil) }

    // MARK: installed software (user decides)

    @objc func openSoftware() {
        if softwareWindow == nil {
            let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 620, height: 540),
                             styleMask: [.titled, .closable, .resizable],
                             backing: .buffered, defer: false)
            w.title = "Installed software — your call"
            w.center()
            w.isReleasedWhenClosed = false

            let scroll = NSScrollView()
            scroll.hasVerticalScroller = true
            scroll.drawsBackground = false
            let doc = FlippedView(frame: NSRect(x: 0, y: 0, width: 620, height: 200))
            let stack = NSStackView()
            stack.orientation = .vertical
            stack.alignment = .leading
            stack.spacing = 7
            stack.translatesAutoresizingMaskIntoConstraints = false
            doc.addSubview(stack)
            NSLayoutConstraint.activate([
                stack.topAnchor.constraint(equalTo: doc.topAnchor, constant: 14),
                stack.leadingAnchor.constraint(equalTo: doc.leadingAnchor, constant: 14),
                stack.trailingAnchor.constraint(equalTo: doc.trailingAnchor, constant: -14),
            ])
            scroll.documentView = doc
            w.contentView = scroll
            softwareWindow = w
            softwareStack = stack
        }
        refreshSoftware()
        softwareWindow?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func refreshSoftware() {
        guard let stack = softwareStack,
              let raw = sendRequest("{\"cmd\":\"agents\"}"),
              let d = raw.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any],
              let items = obj["items"] as? [[String: Any]]
        else { return }
        agentItems = items

        for v in stack.arrangedSubviews {
            stack.removeArrangedSubview(v)
            v.removeFromSuperview()
        }

        let header = NSTextField(wrappingLabelWithString:
            "Third‑party background items installed on this Mac. Ticked = running. "
            + "Untick to stop one (you are asked to confirm). Nothing here is changed "
            + "until you click, and everything can be re‑ticked. Apple services are hidden "
            + "and cannot be changed.")
        header.font = NSFont.systemFont(ofSize: 11)
        header.textColor = .secondaryLabelColor
        header.preferredMaxLayoutWidth = 580
        stack.addArrangedSubview(header)

        if items.isEmpty {
            stack.addArrangedSubview(NSTextField(labelWithString: "No third‑party background items found."))
        }

        for (idx, it) in items.enumerated() {
            let label = it["label"] as? String ?? "?"
            let loaded = it["loaded"] as? Bool ?? false
            let scope = it["scope"] as? String ?? "system"
            let b = NSButton(checkboxWithTitle: label, target: self, action: #selector(toggleAgent(_:)))
            b.tag = idx
            b.state = loaded ? .on : .off
            b.toolTip = "\(scope) · \(it["path"] as? String ?? "")"
            stack.addArrangedSubview(b)
        }

        if let doc = stack.superview {
            let h = stack.fittingSize.height + 28
            doc.frame = NSRect(x: 0, y: 0, width: 620, height: max(h, 200))
        }
    }

    @objc func toggleAgent(_ sender: NSButton) {
        let i = sender.tag
        guard i < agentItems.count else { return }
        let it = agentItems[i]
        let label = it["label"] as? String ?? ""
        let want = sender.state == .on

        if !want {
            let a = NSAlert()
            a.messageText = "Stop \(label)?"
            a.informativeText = "The vendor's background item will be disabled until you re‑tick it here. "
                + "This is reversible and nothing is deleted."
            a.addButton(withTitle: "Disable")
            a.addButton(withTitle: "Cancel")
            if a.runModal() != .alertFirstButtonReturn {
                sender.state = .on
                return
            }
        }

        // Label only: the daemon re-derives path and scope itself, so the UI
        // cannot be used to point launchctl at an arbitrary file.
        _ = sendRequest("{\"cmd\":\"agent\",\"label\":\"\(label)\",\"enable\":\(want)}")
        refreshSoftware()
    }

    // MARK: macOS background services (user decides)

    @objc func openMacOS() {
        if macosWindow == nil {
            let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 640, height: 560),
                             styleMask: [.titled, .closable, .resizable],
                             backing: .buffered, defer: false)
            w.title = "macOS background services — your call"
            w.center()
            w.isReleasedWhenClosed = false

            let scroll = NSScrollView()
            scroll.hasVerticalScroller = true
            scroll.drawsBackground = false
            let doc = FlippedView(frame: NSRect(x: 0, y: 0, width: 640, height: 300))
            let stack = NSStackView()
            stack.orientation = .vertical
            stack.alignment = .leading
            stack.spacing = 10
            stack.translatesAutoresizingMaskIntoConstraints = false
            doc.addSubview(stack)
            NSLayoutConstraint.activate([
                stack.topAnchor.constraint(equalTo: doc.topAnchor, constant: 14),
                stack.leadingAnchor.constraint(equalTo: doc.leadingAnchor, constant: 14),
                stack.trailingAnchor.constraint(equalTo: doc.trailingAnchor, constant: -14),
            ])
            scroll.documentView = doc
            w.contentView = scroll
            macosWindow = w
            macosStack = stack
        }
        refreshMacOS()
        macosWindow?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func refreshMacOS() {
        guard let stack = macosStack,
              let raw = sendRequest("{\"cmd\":\"apple\"}"),
              let d = raw.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: d)) as? [String: Any],
              let items = obj["items"] as? [[String: Any]]
        else { return }
        macosItems = items
        macosNote = obj["note"] as? String ?? ""

        for v in stack.arrangedSubviews {
            stack.removeArrangedSubview(v)
            v.removeFromSuperview()
        }

        let header = NSTextField(wrappingLabelWithString:
            "macOS' own background work. Unticking is reversible and never deletes anything: "
            + "launchd services are re-enabled with one click, Spotlight and Power Nap use "
            + "Apple's own switches. Services protected by SIP will refuse and say so.")
        header.font = NSFont.systemFont(ofSize: 11)
        header.textColor = .secondaryLabelColor
        header.preferredMaxLayoutWidth = 600
        stack.addArrangedSubview(header)

        if !macosNote.isEmpty {
            let note = NSTextField(wrappingLabelWithString: macosNote)
            note.font = NSFont.systemFont(ofSize: 11, weight: .medium)
            note.textColor = .systemBlue
            note.preferredMaxLayoutWidth = 600
            stack.addArrangedSubview(note)
        }

        for (idx, it) in items.enumerated() {
            let id = it["id"] as? String ?? "?"
            let enabled = it["enabled"] as? Bool ?? false
            let desc = it["desc"] as? String ?? ""
            let row = NSStackView()
            row.orientation = .vertical
            row.alignment = .leading
            row.spacing = 1

            let b = NSButton(checkboxWithTitle: id, target: self, action: #selector(toggleMacOS(_:)))
            b.tag = idx
            b.state = enabled ? .on : .off
            let sub = NSTextField(wrappingLabelWithString: desc)
            sub.font = NSFont.systemFont(ofSize: 10)
            sub.textColor = .tertiaryLabelColor
            sub.preferredMaxLayoutWidth = 580

            row.addArrangedSubview(b)
            row.addArrangedSubview(sub)
            stack.addArrangedSubview(row)
        }

        if let doc = stack.superview {
            let h = stack.fittingSize.height + 28
            doc.frame = NSRect(x: 0, y: 0, width: 640, height: max(h, 300))
        }
    }

    @objc func toggleMacOS(_ sender: NSButton) {
        let i = sender.tag
        guard i < macosItems.count else { return }
        let id = macosItems[i]["id"] as? String ?? ""
        let want = sender.state == .on

        if !want {
            let a = NSAlert()
            a.messageText = "Turn off \(id)?"
            a.informativeText = "This stops that background job until you turn it back on here. "
                + "Nothing is deleted and no system files are modified."
            a.addButton(withTitle: "Turn off")
            a.addButton(withTitle: "Cancel")
            if a.runModal() != .alertFirstButtonReturn {
                sender.state = .on
                return
            }
        }

        _ = sendRequest("{\"cmd\":\"apple_set\",\"id\":\"\(id)\",\"enable\":\(want)}")
        refreshMacOS()
    }

    /// POSIX shell single-quote escaping, so a path can never break out of the
    /// command that is handed to `do shell script` as root.
    func shellQuote(_ s: String) -> String {
        return "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    @objc func installHelper() {
        guard let script = Bundle.main.path(forResource: "install-helper", ofType: "sh") else {
            hintLabel.stringValue = "install-helper.sh missing from the app bundle."
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
                hintLabel.stringValue = "Installer error: \(e[NSAppleScript.errorMessage] ?? "unknown")"
            } else {
                hintLabel.stringValue = "Helper installed. Thermal protection is active."
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

    @objc func toggleLogin() {
        let path = loginPlistPath()
        if loginButton.state == .on {
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
    }

    // MARK: refresh

    func refresh() {
        refreshAccelerationGuard()
        guard let s = requestStatus() else {
            statusItem.button?.title = "—°"
            stateLabel.stringValue = "Thermal helper not running"
            installButton.isEnabled = true
            modeControl.isEnabled = false
            detailView.stringValue = """
            The privileged helper is not installed or not running.

            Click “Install Thermal Helper…” once. It installs a small root
            daemon that reads the SMC thermometer and applies the power cap.
            After that, this window shows live data.
            """
            return
        }

        modeControl.isEnabled = true
        installButton.isEnabled = false

        let temp = s["cpu_temp"] as? Double
        let fan = s["fan_rpm"] as? Double
        let limit = s["speed_limit"] as? Int
        let mode = s["mode"] as? String ?? "balanced"
        let capActive = s["cap_active"] as? Bool ?? false
        let fanSupported = s["fan_supported"] as? Bool
        let fanNote = s["fan_note"] as? String ?? ""
        let smcOK = s["smc_ok"] as? Bool ?? false
        let smcErr = s["smc_error"] as? String ?? ""
        let batt = s["battery_temp"] as? Double
        let ac = s["ac_power"] as? Bool ?? false
        let turbo = s["turbo_mhz"] as? Int
        let lpm = s["lpm_mhz"] as? Int
        let procs = s["procs"] as? [[String: Any]] ?? []
        let demoted = s["demoted"] as? [[String: Any]] ?? []
        let events = s["events"] as? [String] ?? []

        if let t = temp {
            let shown = Int(t.rounded())
            statusItem.button?.title = "\(shown)°"
            tempLabel.stringValue = "\(shown)°C"
            tempLabel.textColor = t >= 90 ? .systemRed : (t >= 78 ? .systemOrange : .labelColor)
        } else {
            statusItem.button?.title = "–°"
            tempLabel.stringValue = "n/a"
            tempLabel.textColor = .secondaryLabelColor
        }

        var fanLine = "fan "
        if let f = fan { fanLine += "\(Int(f)) RPM" } else { fanLine += "unknown" }
        fanLabel.stringValue = fanLine

        let modeIdx = modeValues.firstIndex(of: mode) ?? 0
        if modeControl.selectedSegment != modeIdx { modeControl.selectedSegment = modeIdx }

        var stateBits: [String] = []
        stateBits.append(s["mode_label"] as? String ?? mode)
        stateBits.append(capActive ? "power cap ON" : "power cap off")
        if let l = limit, l < 100 { stateBits.append("OS throttling (\(l)%)") }
        stateBits.append(ac ? "on AC" : "on battery")
        if (s["media_active"] as? Bool) == true {
            stateBits.append((s["media_guard"] as? Bool) == true ? "call/video shielded" : "call/video active")
        }
        if let g = s["media_guard"] as? Bool, (g ? .on : .off) != mediaCheck.state {
            mediaCheck.state = g ? .on : .off
        }
        stateLabel.stringValue = stateBits.joined(separator: " · ")

        let fanIntent = s["fan_intent"] as? String ?? "auto"
        let fanIdx = fanValues.firstIndex(of: fanIntent) ?? 0
        if fanControl.selectedSegment != fanIdx { fanControl.selectedSegment = fanIdx }
        // Only "Maximum" is known-unsupported by a model; "Silent" is worth
        // offering because the SMC either accepts it or reports it back plainly.
        if fanSupported == false && fanIdx == 0 { fanControl.isEnabled = false } else { fanControl.isEnabled = true }

        hintLabel.stringValue = fanNote.isEmpty ? "" : fanNote

        var lines: [String] = []
        lines.append("CPU   \(temp.map { String(format: "%.1f °C", $0) } ?? "n/a")   Battery \(batt.map { String(format: "%.1f °C", $0) } ?? "n/a")")
        lines.append("SMC   \(smcOK ? "connected" : "unavailable — \(smcErr)")")
        if let turbo, let lpm {
            lines.append("Clock ceiling  \(capActive ? "\(lpm/1000).\((lpm%1000)/100) GHz (capped)" : "\(turbo/1000).\((turbo%1000)/100) GHz (turbo)")")
        }
        lines.append("")
        lines.append("WHAT IS USING THE CPU")
        if procs.isEmpty {
            lines.append("  (none sampled)")
        }
        for p in procs.prefix(7) {
            let name = p["name"] as? String ?? "?"
            let cpu = p["cpu"] as? Double ?? 0
            let pid = p["pid"] as? Int ?? 0
            let mark = demoted.contains(where: { ($0["pid"] as? Int) == pid }) ? "↓" : " "
            lines.append(String(format: "  %@ %5.1f%%  %@", mark, cpu, name))
        }
        if !demoted.isEmpty {
            lines.append("")
            lines.append("↓ = demoted to background QoS (reversible, nothing is killed)")
        }
        lines.append("")
        lines.append("RECENT EVENTS")
        for e in events.suffix(5) {
            lines.append("  \(e)")
        }
        detailView.stringValue = lines.joined(separator: "\n")
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
