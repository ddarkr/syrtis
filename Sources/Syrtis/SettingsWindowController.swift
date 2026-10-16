import AppKit
import TokenBarCore
import SwiftUI

/// Shared state observed by the macOS 26+ Scene bridge.
@MainActor
final class SettingsWindowState: ObservableObject {
    @Published var session: LocalUsageScheduler.Session?
    @Published var destination: SettingsWindowController.Destination?
    @Published var destinationRequest: UInt64 = 0
    @Published var scheduler: LocalUsageScheduler?
    @Published var source: (any UsageDataSource)?
    @Published var isContentActive = false
}

/// Owns the standalone settings window (gear button, Cmd-comma, `--settings`).
/// On macOS 26+, uses public NSHostingSceneRepresentation and a native SwiftUI
/// WindowGroup scene for integrated split material, unified
/// toolbar, and native sidebar. On macOS 14-25, falls back to NSHostingController.
@MainActor
final class SettingsWindowController {
    static let shared = SettingsWindowController()
    static let sceneWindowID = "syrtis-settings"
    static let singleWindowIdentity = "settings"

    let state = SettingsWindowState()
    private var sceneBridge: AnyObject?

    private var window: NSWindow?
    // AnyView so the live UI can be swapped for a static placeholder on close in fallback host.
    private var host: NSHostingController<AnyView>?
    private var visibilityObservers: [NSObjectProtocol] = []
    private var scheduler: LocalUsageScheduler?
    private var source: (any UsageDataSource)?
    private var localSession: LocalUsageScheduler.Session?
    private var hasOpenedOnce = false
    private var lastClosedFrameSize: CGSize?

    isolated deinit {
        for observer in visibilityObservers {
            NotificationCenter.default.removeObserver(observer)
        }
    }

    func configure(scheduler: LocalUsageScheduler, source: any UsageDataSource) {
        self.scheduler = scheduler
        self.source = source
        state.scheduler = scheduler
        state.source = source
        registerSceneIfNeeded()
    }

    func registerSceneIfNeeded() {
        if #available(macOS 26.0, *) {
            ensureSceneBridge()
        }
    }

    /// A place in Settings a caller wants brought into view. The intro card
    /// uses it so "Open Settings" lands on the section it just described
    /// instead of the top of the first page.
    enum Destination: Equatable {
        case discord
        case usageAttribution
        case dashboard

        var page: SettingsPanel.Page {
            switch self {
            case .discord: return .general
            case .usageAttribution: return .usageAttribution
            case .dashboard: return .dashboard
            }
        }

        /// Matched by a `.id(...)` on the section itself, or nil when
        /// selecting the page already puts the destination at the top —
        /// the usage-attribution page has exactly one section, so there is
        /// nothing further to scroll to.
        var anchor: String? {
            switch self {
            case .discord: return Self.discordAnchor
            case .usageAttribution, .dashboard: return nil
            }
        }

        /// The Discord section's `.id`, non-optional on purpose: tagging the
        /// section with `anchor` itself would give it an `Optional<String>`
        /// id, which never equals the `String` handed to `scrollTo`, and the
        /// intro card's "Open Settings" would stop scrolling to it.
        static let discordAnchor = "settings.section.discord"
    }

    /// For a control inside the popover: close the popover first, then show
    /// on the next runloop turn, as the popover's own gear button does
    /// (`PopoverView.openSettingsWindow`). Showing in the same turn as the
    /// popover's animated close puts both vibrant windows in one CoreAnimation
    /// transaction and the native switch thumbs lose their first frame; and
    /// leaving the popover open keeps its polling running behind Settings.
    func showFromPopover(scrollingTo destination: Destination? = nil) {
        if let popover = NSApp.keyWindow, popover !== window {
            popover.performClose(nil)
        }
        DispatchQueue.main.async { self.show(scrollingTo: destination) }
    }

    func show(scrollingTo destination: Destination? = nil) {
        guard let scheduler, let source else { return }

        if #available(macOS 26.0, *) {
            ensureSceneBridge()
            if let existing = self.window, existing.isVisible, !existing.isMiniaturized, localSession != nil, destination == nil {
                NSApp.activate(ignoringOtherApps: true)
                existing.makeKeyAndOrderFront(nil)
                return
            }

            if localSession == nil {
                localSession = scheduler.openSession()
            }
            state.scheduler = scheduler
            state.source = source
            state.session = localSession
            state.isContentActive = true
            state.destination = destination
            // Repeated destinations are new navigation requests, not selection changes.
            state.destinationRequest &+= 1

            NSApp.activate(ignoringOtherApps: true)
            if let window = self.window {
                if window.isMiniaturized { window.deminiaturize(nil) }
                window.makeKeyAndOrderFront(nil)
            } else {
                (sceneBridge as? SettingsSceneBridge)?.open()
            }
            return
        }

        // macOS 14-25 Fallback host
        showFallback(scrollingTo: destination)
    }

    private func ensureSceneBridge() {
        if #available(macOS 26.0, *) {
            if sceneBridge == nil {
                sceneBridge = SettingsSceneBridge(state: state)
            }
        }
    }

    func bindSceneWindow(_ window: NSWindow) {
        guard self.window !== window else { return }
        self.window = window
        window.contentMinSize = SettingsWindowView.minSize
        if !hasOpenedOnce {
            hasOpenedOnce = true
            window.setContentSize(SettingsWindowView.contentSize)
            center(window)
            DispatchQueue.main.async { [weak window, weak self] in
                guard let window, let self else { return }
                window.setContentSize(SettingsWindowView.contentSize)
                self.center(window)
            }
        } else if let lastClosedFrameSize {
            DispatchQueue.main.async { [weak window] in
                guard let window else { return }
                window.setFrame(NSRect(origin: window.frame.origin, size: lastClosedFrameSize), display: true)
            }
        }
        observeWindow(window)
    }

    private func removeWindowObservers() {
        for observer in visibilityObservers {
            NotificationCenter.default.removeObserver(observer)
        }
        visibilityObservers.removeAll()
    }

    private func observeWindow(_ window: NSWindow) {
        removeWindowObservers()
        visibilityObservers.append(NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.handleWindowClose() }
        })
        visibilityObservers.append(NotificationCenter.default.addObserver(
            forName: NSWindow.didMiniaturizeNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.releaseContent() }
        })
        visibilityObservers.append(NotificationCenter.default.addObserver(
            forName: NSWindow.didDeminiaturizeNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.show() }
        })
    }

    private func handleWindowClose() {
        if let window = self.window {
            lastClosedFrameSize = window.frame.size
        }
        releaseContent()
        if #available(macOS 26.0, *) {
            self.window = nil
        }
    }

    private func releaseContent() {
        if let localSession { scheduler?.release(localSession) }
        localSession = nil
        state.session = nil
        state.isContentActive = false

        // Fallback placeholder teardown
        host?.rootView = AnyView(Color.clear)
        window?.contentMinSize = SettingsWindowView.minSize
    }

    private func center(_ window: NSWindow) {
        guard let screen = window.screen ?? NSScreen.main ?? NSScreen.screens.first
        else { return }
        let visible = screen.visibleFrame
        window.setFrameOrigin(NSPoint(
            x: visible.midX - window.frame.width / 2,
            y: visible.midY - window.frame.height / 2))
    }

    // MARK: - macOS 14-25 Fallback Hosting

    private func showFallback(scrollingTo destination: Destination? = nil) {
        guard let scheduler, let source else { return }
        let existing = self.window
        if let existing, existing.isVisible, !existing.isMiniaturized, localSession != nil, destination == nil {
            NSApp.activate(ignoringOtherApps: true)
            existing.makeKeyAndOrderFront(nil)
            return
        }
        let window = existing ?? makeFallbackWindow()
        self.window = window
        if let localSession { scheduler.release(localSession) }
        let session = scheduler.openSession()
        localSession = session
        host?.rootView = AnyView(SettingsWindowView(
            scheduler: scheduler, source: source, localSession: session, destination: destination)
            .id(session))
        if window.isMiniaturized { window.deminiaturize(nil) }
        let firstShow = !hasOpenedOnce
        if firstShow {
            hasOpenedOnce = true
            window.setContentSize(SettingsWindowView.contentSize)
            center(window)
            DispatchQueue.main.async { [weak window, weak self] in
                guard let window, let self else { return }
                window.setContentSize(SettingsWindowView.contentSize)
                self.center(window)
            }
        }
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    private func makeFallbackWindow() -> NSWindow {
        let host = NSHostingController(rootView: AnyView(Color.clear))
        host.sizingOptions = [.minSize]
        self.host = host
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: SettingsWindowView.contentSize),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        window.contentViewController = host
        window.title = "Syrtis Settings".localized
        window.contentMinSize = SettingsWindowView.minSize
        window.backgroundColor = NSColor.windowBackgroundColor
        window.isOpaque = false
        window.hasShadow = true
        window.titlebarAppearsTransparent = false
        window.titleVisibility = .visible
        window.titlebarSeparatorStyle = .none
        window.toolbarStyle = .unified
        window.isReleasedWhenClosed = false
        observeWindow(window)
        return window
    }
}

// MARK: - macOS 26+ Scene Integration

@available(macOS 26.0, *)
struct SettingsWindowScene: Scene {
    @ObservedObject var state: SettingsWindowState

    var body: some Scene {
        WindowGroup(
            Text("Syrtis Settings".localized),
            id: SettingsWindowController.sceneWindowID,
            for: String.self
        ) { _ in
            SettingsSceneHostView(state: state)
        }
        .windowResizability(.contentMinSize)
        .defaultSize(width: SettingsWindowView.contentSize.width, height: SettingsWindowView.contentSize.height)
        .windowToolbarStyle(.unified)
    }
}

@available(macOS 26.0, *)
struct SettingsSceneHostView: View {
    @ObservedObject var state: SettingsWindowState

    var body: some View {
        Group {
            if let session = state.session,
               let scheduler = state.scheduler,
               let source = state.source,
               state.isContentActive {
                SettingsWindowView(
                    scheduler: scheduler,
                    source: source,
                    localSession: session,
                    destination: state.destination,
                    destinationRequest: state.destinationRequest
                )
                .id(session)
            } else {
                Color.clear
            }
        }
        .frame(minWidth: SettingsWindowView.minSize.width, minHeight: SettingsWindowView.minSize.height)
        .background(SettingsWindowAccessor { window in
            SettingsWindowController.shared.bindSceneWindow(window)
        })
    }
}

private struct SettingsWindowAccessor: NSViewRepresentable {
    let onWindow: (NSWindow) -> Void

    func makeNSView(context: Context) -> WindowView {
        let view = WindowView()
        view.onWindow = onWindow
        return view
    }

    func updateNSView(_ nsView: WindowView, context: Context) {
        nsView.onWindow = onWindow
    }

    final class WindowView: NSView {
        var onWindow: ((NSWindow) -> Void)?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window else { return }
            DispatchQueue.main.async { [weak self, weak window] in
                guard let self, let window, self.window === window else { return }
                self.onWindow?(window)
            }
        }
    }
}

@available(macOS 26.0, *)
@MainActor
private final class SettingsSceneBridge {
    let representation: NSHostingSceneRepresentation<SettingsWindowScene>

    init(state: SettingsWindowState) {
        let rep = NSHostingSceneRepresentation {
            SettingsWindowScene(state: state)
        }
        self.representation = rep
        NSApplication.shared.addSceneRepresentation(rep)
    }

    func open() {
        representation.environment.openWindow(
            id: SettingsWindowController.sceneWindowID,
            value: SettingsWindowController.singleWindowIdentity
        )
    }
}
