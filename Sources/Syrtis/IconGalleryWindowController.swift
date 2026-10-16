import AppKit
import SwiftUI

/// Debug-only standalone window for `--icon-gallery` (see IconGalleryView).
/// Throwaway screenshot aid, not a shipped feature — no state persistence.
/// Resizable, like Settings: the host follows the root minimum only, and a
/// close swaps the live grid for a flexible placeholder so no live content
/// is retained while the window sits closed.
@MainActor
enum IconGalleryWindowController {
    private static var window: NSWindow?
    private static var host: NSHostingController<AnyView>?
    private static var closeObserver: NSObjectProtocol?

    static func show() {
        if window == nil { makeWindow() }
        host?.rootView = AnyView(IconGalleryView())
        guard let window else { return }
        let firstShow = !window.isVisible
        if window.isMiniaturized { window.deminiaturize(nil) }
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        if firstShow { window.center() }
    }

    private static func makeWindow() {
        let host = NSHostingController(rootView: AnyView(Color.clear))
        // Let the root supply its minimum, but never follow preferred or
        // intrinsic sizes when replacing live content with a placeholder.
        host.sizingOptions = [.minSize]
        self.host = host
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: IconGalleryView.contentSize),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered, defer: false)
        // A content-view controller also negotiates its zero placeholder
        // minimum with NSWindow. Keep the hosting view, not that size owner.
        window.contentView = host.view
        window.title = "Agent Icon Gallery (debug)"
        window.contentMinSize = IconGalleryView.minSize
        window.isReleasedWhenClosed = false
        Self.window = window
        closeObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main
        ) { _ in
            MainActor.assumeIsolated {
                // Flexible placeholder: a fixed frame here would be
                // renegotiated as the window minimum on the next show.
                Self.host?.rootView = AnyView(Color.clear)
            }
        }
    }
}
