import AppKit
import Observation
import ServiceManagement
import Sparkle

/// Manual Sparkle updates. Only armed when running from an assembled .app
/// bundle — the bare SwiftPM executable (dev runs, --selftest, CI) has no
/// Info.plist feed/key and Sparkle would refuse to start.
///
/// Automatic checks are temporarily disabled; Settings → Check Now remains available.
@MainActor
@Observable
final class UpdaterService: NSObject, SPUUpdaterDelegate {
    static let shared = UpdaterService()

    nonisolated static var isAvailable: Bool { Bundle.main.bundleURL.pathExtension == "app" }

    /// Display version of an available update (nil = up to date / unknown).
    private(set) var availableVersion: String?

    @ObservationIgnored private var controller: SPUStandardUpdaterController?

    private override init() {
        super.init()
        guard Self.isAvailable else { return }
        controller = SPUStandardUpdaterController(
            startingUpdater: false, updaterDelegate: self, userDriverDelegate: nil)
        controller?.updater.automaticallyChecksForUpdates = false
        controller?.startUpdater()
    }

    /// The interactive Sparkle flow (update dialog with install button).
    func checkForUpdates() {
        controller?.checkForUpdates(nil)
    }

    // MARK: - SPUUpdaterDelegate

    /// Prerelease appcast items are tagged <sparkle:channel>beta</sparkle:channel>;
    /// only installs that opt in (Settings → "Receive beta updates") see them.
    nonisolated func allowedChannels(for updater: SPUUpdater) -> Set<String> {
        UserDefaults.standard.bool(forKey: "tokenbar.updates.beta") ? ["beta"] : []
    }

    nonisolated func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
        let version = item.displayVersionString
        Task { @MainActor in self.availableVersion = version }
    }

    nonisolated func updaterDidNotFindUpdate(_ updater: SPUUpdater) {
        Task { @MainActor in self.availableVersion = nil }
    }
}

/// Launch-at-login via SMAppService — also bundle-only (the bare executable
/// has no main-app service identity to register).
@MainActor
enum AutostartService {
    nonisolated static var isAvailable: Bool { UpdaterService.isAvailable }

    nonisolated static func readEnabled() async -> Bool {
        await Task.detached(priority: .utility) {
            SMAppService.mainApp.status == .enabled
        }.value
    }

    @discardableResult
    static func setEnabled(_ enabled: Bool) -> Bool {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            return true
        } catch {
            return false
        }
    }
}
