import AppKit
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

// MARK: - Chrome guard
//
// Chrome stores "Use hardware acceleration" in its own Local State file, and a
// Chrome update (or a crash loop) can put it back to `enabled: false`. With
// acceleration off, a 1.1 GHz Intel Air decodes video in software, burns both
// cores, heats up and hangs playback — which is exactly the symptom this app
// exists to prevent. So we watch the setting and put it back.

struct ChromeState {
    var installed = false
    var enabled = true
    var running = false
    var readable = true
}

func chromeAppURL() -> URL? {
    let p = "/Applications/Google Chrome.app"
    return FileManager.default.fileExists(atPath: p) ? URL(fileURLWithPath: p) : nil
}

func chromeLocalStateURL() -> URL {
    FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/Google/Chrome/Local State")
}

func chromeRunning() -> Bool {
    !NSRunningApplication.runningApplications(withBundleIdentifier: "com.google.Chrome").isEmpty
}

func chromeState() -> ChromeState {
    var s = ChromeState()
    s.installed = chromeAppURL() != nil
    s.running = chromeRunning()
    guard s.installed else { return s }
    guard let data = try? Data(contentsOf: chromeLocalStateURL()),
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

/// Quit Chrome if needed, re-enable acceleration, drop the stale GPU cache and
/// relaunch. Only ever touches Chrome's own config; nothing here needs root.
@discardableResult
func fixChromeAcceleration(relaunch: Bool) -> (Bool, String) {
    guard chromeAppURL() != nil else { return (false, "Google Chrome is not installed.") }

    let wasRunning = chromeRunning()
    if wasRunning {
        NSAppleScript(source: "tell application \"Google Chrome\" to quit")?
            .executeAndReturnError(nil)
        for _ in 0..<50 where chromeRunning() {
            Thread.sleep(forTimeInterval: 0.1)
        }
        if chromeRunning() { return (false, "Chrome did not quit; close it and try again.") }
    }

    let url = chromeLocalStateURL()
    guard let data = try? Data(contentsOf: url),
          var obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        return (false, "Chrome's settings file could not be read.")
    }
    obj["hardware_acceleration_mode"] = ["enabled": true]
    obj["hardware_acceleration_mode_previous"] = true
    guard let out = try? JSONSerialization.data(withJSONObject: obj, options: []) else {
        return (false, "Chrome's settings could not be re-encoded.")
    }
    let tmp = url.appendingPathExtension("thermalguard-tmp")
    do {
        try out.write(to: tmp, options: .atomic)
        _ = try FileManager.default.replaceItemAt(url, withItemAt: tmp)
    } catch {
        return (false, "Could not write Chrome's settings: \(error.localizedDescription)")
    }

    let fm = FileManager.default
    let base = url.deletingLastPathComponent()
    for profile in ["Default", "Profile 1", "Profile 2", "System Profile"] {
        let cache = base.appendingPathComponent(profile).appendingPathComponent("GPUCache")
        if fm.fileExists(atPath: cache.path) { try? fm.removeItem(at: cache) }
    }

    if relaunch && wasRunning, let app = chromeAppURL() {
        NSWorkspace.shared.openApplication(at: app, configuration: NSWorkspace.OpenConfiguration()) { _, _ in }
    }
    return (true, relaunch && wasRunning ? "Chrome fixed and relaunched." : "Chrome fixed.")
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
    let chromeLabel = NSTextField(wrappingLabelWithString: "")
    let chromeFixButton = NSButton(title: "Fix Chrome…", target: nil, action: nil)
    var chromeAutoFixed = false
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

        chromeLabel.font = NSFont.systemFont(ofSize: 11)
        chromeLabel.textColor = .secondaryLabelColor
        chromeLabel.preferredMaxLayoutWidth = 500
        chromeFixButton.target = self
        chromeFixButton.action = #selector(fixChrome)
        chromeFixButton.bezelStyle = .rounded
        chromeFixButton.isHidden = true

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
                                        modeControl, fanControl, mediaCheck, chromeLabel, chromeFixButton, hintLabel,
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

    @objc func fixChrome() {
        let (ok, msg) = fixChromeAcceleration(relaunch: true)
        chromeLabel.stringValue = msg
        if ok { chromeAutoFixed = true }
        refresh()
    }

    /// Runs on every refresh tick: catches a Chrome update that switched
    /// acceleration back off. When Chrome is closed the fix is silent; when it
    /// is open we never quit it behind the user's back, we offer the button.
    func refreshChromeGuard() {
        let s = chromeState()
        guard s.installed else {
            chromeLabel.stringValue = ""
            chromeFixButton.isHidden = true
            return
        }
        if s.enabled {
            chromeLabel.stringValue = "Chrome: hardware video decode on."
            chromeFixButton.isHidden = true
            chromeAutoFixed = false
            return
        }
        if !s.running, !chromeAutoFixed {
            let (ok, msg) = fixChromeAcceleration(relaunch: false)
            chromeAutoFixed = ok
            chromeLabel.stringValue = "Chrome: acceleration was off — \(msg) Reopen Chrome."
            chromeFixButton.isHidden = ok
            return
        }
        if !s.readable {
            chromeLabel.stringValue = "Chrome is running; its settings are readable once it quits."
            chromeFixButton.isHidden = true
            return
        }
        chromeLabel.stringValue = "Chrome: hardware acceleration is OFF — video will stutter and heat the Mac."
        chromeFixButton.isHidden = false
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
        refreshChromeGuard()
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

// Headless entry points, used by the test suite and for a quick check from a
// terminal: `ThermalGuard --chrome-check` / `--chrome-fix`.
if CommandLine.arguments.contains("--chrome-check") {
    let s = chromeState()
    print("{\"installed\":\(s.installed),\"enabled\":\(s.enabled),\"running\":\(s.running),\"readable\":\(s.readable)}")
    exit(s.enabled ? 0 : 3)
}
if CommandLine.arguments.contains("--chrome-fix") {
    let (ok, msg) = fixChromeAcceleration(relaunch: false)
    print(msg)
    exit(ok ? 0 : 1)
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
