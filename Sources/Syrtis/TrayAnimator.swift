import AppKit
import TokenBarCore

#if DEBUG
struct TrayAnimationCPUTestConfiguration {
    let style: String
    let animated: Bool
    /// The top of the speed curve, so the harness measures the worst case.
    let tokensPerMinute = TrayAnimator.animationCapTokensPerMinute

    static let current: TrayAnimationCPUTestConfiguration? = {
        let prefix = "--tray-animation-cpu-test="
        guard let argument = CommandLine.arguments.first(where: { $0.hasPrefix(prefix) })
        else { return nil }
        let value = String(argument.dropFirst(prefix.count))
        switch value {
        case "cat", "parrot", TrayAnimator.sandStyle:
            return TrayAnimationCPUTestConfiguration(style: value, animated: true)
        case "static":
            return TrayAnimationCPUTestConfiguration(style: "cat", animated: false)
        default:
            return nil
        }
    }()
}
#endif

/// RunCat-style menu-bar animation, port of src-tauri's animation.rs: the
/// cat (or parrot) spins faster as the live token rate climbs. Frame sets
/// come in dark/light pairs and follow the menu bar's effective appearance —
/// `anim-*` are white glyphs for a dark menu bar, `anim-*-light` black ones
/// for a light menu bar.
@MainActor
final class TrayAnimator {
    static let animateKey = "tokenbar.tray.animate"
    static let styleKey = "tokenbar.tray.animationStyle"

    static let quotaSourceKey = "tokenbar.quota.source"

    private weak var controller: StatusItemController?
    private let source: any UsageDataSource
    private let scheduler: LocalUsageScheduler
    private var rateSession: LocalUsageScheduler.Session?
#if DEBUG
    private let cpuTest = TrayAnimationCPUTestConfiguration.current
#endif
    /// Only the currently demanded frame set is held ("anim-cat2",
    /// "anim-sand2-light", …). Style/level/theme switches discard the
    /// obsolete set; gauge styles and the stopped state hold nothing. Static
    /// (animation-disabled) state holds just the first frame.
    private var heldKey: String?
    private var heldFrames: [NSImage] = []
    private var quotaTask: Task<Void, Never>?
    private var presentedAnimationKey: String?
    private var isStopped = true
    /// Load signal: the pace-scaled tokens/min ÷ 10K, capped at
    /// `animationCapTokensPerMinute` (3M, so at most 300).
    private var load: Double = 0
    /// Latest snapshot returned by this poller. A newer payload accepted by a
    /// dashboard or Settings poll wins through the shared publication state.
    private var polledQuota: AgentUsagePayload?
    var quota: AgentUsagePayload? { Self.publishedQuota(polledQuota) }

    static func publishedQuota(_ polledQuota: AgentUsagePayload?) -> AgentUsagePayload? {
        if let polledQuota, polledQuota.publicationGeneration == nil { return polledQuota }
        return AgentUsagePublicationCoordinator.latestPayload ?? polledQuota
    }
    /// Fired after every successful quota fetch (title refresh hook).
    var onQuotaUpdated: (() -> Void)?

    init(
        controller: StatusItemController,
        source: any UsageDataSource = UsageDataSources.current,
        scheduler: LocalUsageScheduler
    ) {
        self.controller = controller
        self.source = source
        self.scheduler = scheduler
        self.cachedQuotaRemaining = source.allowsQuotaCachePersistence
            ? UserDefaults.standard.object(forKey: Self.lastRemainingKey) as? Double
            : nil
        // Frame sets load on demand in refreshIcon; nothing is retained here.
    }

    /// The bar's frame box. Art is fitted into it, never stretched to it.
    nonisolated static let frameBox = NSSize(width: 18, height: 18)

    /// PNG frames sorted by name (frame-00 … frame-NN), sized for the bar.
    /// Internal so the settings window's menu-bar mock can render the same
    /// frame sets. Keeps loadFrames API compatibility: single directory load.
    static func loadFrames(directory: String) -> [NSImage] {
        let urls = Bundle.tokenBarResources.urls(
            forResourcesWithExtension: "png", subdirectory: directory) ?? []
        return urls
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
            .compactMap { url in
                guard let image = NSImage(contentsOf: url) else { return nil }
                image.size = barSize(for: image)
                return image
            }
    }

    /// Canonical bundle directory for a demanded animated style. Unknown
    /// styles fall back to the cat set, matching the tray's safe fallback.
    nonisolated static func frameDirectory(style: String, dark: Bool, level: Int = 0) -> String {
        let base: String
        switch style {
        case "parrot": base = "anim-parrot"
        case sandStyle: base = "anim-sand\(min(max(level, 0), sandLevels - 1))"
        case "cat": base = "anim-cat2"
        default: base = "anim-cat2"
        }
        return base + (dark ? "" : "-light")
    }

    /// Consumer-visible demand probe: which single frame set an icon state
    /// needs, so tests can assert obsolete sets are discarded without
    /// touching retained storage. Unknown styles report the cat fallback.
    nonisolated static func demandedFrames(style: String, dark: Bool, level: Int = 0, animated: Bool = true) -> (key: String, staticOnly: Bool) {
        (frameDirectory(style: style, dark: dark, level: level), !animated)
    }

    /// `frameBox`-bounded logical size preserving the art's own aspect ratio.
    ///
    /// This used to assign `frameBox` unconditionally, which is only correct
    /// for square art. `anim-parrot` is 48x36, so forcing 18x18 stretched it
    /// vertically — and only on the paths that render `NSImage.size`:
    /// `button.image` (what static tray mode and the Settings preview show).
    /// The animation itself was never affected, because
    /// `StatusItemAnimationSurface.rasterizedFrame` fits by the *pixel*
    /// dimensions of the representation and ignores the logical size, so the
    /// two paths disagreed about the same asset.
    ///
    /// Pixel data is deliberately untouched: the raster path reads
    /// `representations`, so re-drawing frames here would change what it
    /// sees. Only the logical size is corrected.
    nonisolated static func barSize(for image: NSImage) -> NSSize {
        let pixels = image.representations
            .max(by: { $0.pixelsWide * $0.pixelsHigh < $1.pixelsWide * $1.pixelsHigh })
            .map { NSSize(width: $0.pixelsWide, height: $0.pixelsHigh) } ?? image.size
        guard pixels.width > 0, pixels.height > 0 else { return frameBox }
        let fit = min(frameBox.width / pixels.width, frameBox.height / pixels.height)
        return NSSize(width: pixels.width * fit, height: pixels.height * fit)
    }

    private var defaultsObserver: NSObjectProtocol?
    /// Snapshot of the icon-affecting defaults the observer reacts to. The
    /// global didChangeNotification carries no key and fires for every write
    /// (popover height, active tab, year, quota cache…), so we compare this
    /// signature and act only when an icon setting actually changed —
    /// otherwise an unrelated write would needlessly re-render the gauge and
    /// tear down + restart the animation loop on every keystroke.
    private var iconSettingsSignature = ""

    static func currentIconSignature(defaults d: UserDefaults = .standard) -> String {
        let style = d.string(forKey: styleKey) ?? ""
        let animate = d.object(forKey: animateKey).map { String(describing: $0) } ?? ""
        let quotaSource = d.string(forKey: quotaSourceKey) ?? ""
        let lastRemaining = d.object(forKey: lastRemainingKey).map { String(describing: $0) } ?? ""
        let titleMode = d.string(forKey: TrayMode.storageKey) ?? ""
        let iconColor = d.string(forKey: IconColoring.storageKey) ?? ""
            // The Auto gauge value now depends on the exclusion set, so a hide
            // toggle must re-render (and re-resolve) the gauge — renderGaugeIcon
            // reads `quotaRemaining`, which resolves with the live exclusion.
            // Without these keys the icon kept the excluded client's % until the
            // 30s gauge loop / next quota poll. Value-gated: one re-render on an
            // actual change, unrelated writes stay free.
        let tabHidden = d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""
        let limitsHidden = d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""
        let pace = d.string(forKey: AnimationPace.storageKey) ?? ""
        return [style, animate, quotaSource, lastRemaining, iconColor, tabHidden, limitsHidden, pace, titleMode]
            .joined(separator: "|")
    }

    func start() {
        isStopped = false
#if DEBUG
        if let cpuTest {
            load = Self.animationLoad(
                tokensPerMinute: AnimationPace.current().scaled(cpuTest.tokensPerMinute))
            tokensPerMinRate = cpuTest.tokensPerMinute
            refreshIcon()
            reportCPUTestReady(cpuTest)
            return
        }
#endif
        iconSettingsSignature = Self.currentIconSignature()
        controller?.setAppearanceChangeHandler { [weak self] in
            self?.refreshIcon()
            // The stale title grey depends on the appearance too (#420); the
            // title is redrawn by the menu-bar pass, not by `refreshIcon`.
            self?.onQuotaUpdated?()
        }
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                let next = Self.currentIconSignature()
                guard next != self.iconSettingsSignature else { return }
                self.iconSettingsSignature = next
                // A pace change rescales the rate already held.
                if let rate = self.tokensPerMinRate {
                    self.load = Self.animationLoad(
                        tokensPerMinute: AnimationPace.current().scaled(rate))
                }
                if let payload = self.quota {
                    self.reconcileQuotaRemaining(with: payload)
                }
                self.refreshIcon()
                self.reconcileLoadDemand()
            }
        }
        refreshIcon()
        reconcileLoadDemand()
        startQuotaPolling()
    }

    func stop() {
        isStopped = true
        rateGeneration += 1
        quotaTask?.cancel()
        quotaTask = nil
        if let rateSession { scheduler.release(rateSession) }
        rateSession = nil
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        defaultsObserver = nil
        controller?.setAppearanceChangeHandler(nil)
        controller?.stopTrayAnimation()
        presentedAnimationKey = nil
        releaseHeldFrames()
    }

    private var currentStyle: String {
#if DEBUG
        if let cpuTest { return cpuTest.style }
#endif
        return UserDefaults.standard.string(forKey: Self.styleKey) ?? "cat"
    }

    /// Draws the current gauge style immediately (no-op for cat/parrot).
    /// The stale state the gauge was last drawn with. The title is recomputed
    /// every ~30 s, the gauge only on quota polls (~5 min), so without this the
    /// title could turn grey minutes before the gauge (#420).
    private var drawnStale: Bool?

    /// Redraws the gauge when its stale state no longer matches `readingIsStaleNow`.
    /// Called from the menu-bar pass that recomputes the title, so the two
    /// cross the 30-minute threshold on the same tick.
    func syncStaleGauge() {
        guard QuotaIconStyle(rawValue: currentStyle) != nil,
              drawnStale != nil, drawnStale != readingIsStaleNow
        else { return }
        refreshIcon()
    }

    /// Whether the reading the menu bar shows is stale now: the gauge fill
    /// and, since #420, the quota title use the same answer.
    var readingIsStaleNow: Bool {
        Self.readingIsStale(
            payload: quota,
            persistedSelection: UserDefaults.standard.string(forKey: Self.quotaSourceKey)
                ?? QuotaResolver.auto,
            excluding: ClientRegistry.quotaExcludedClients(),
            defaults: source.allowsQuotaCachePersistence ? .standard : nil,
            now: Date())
    }

    private func renderGaugeIcon() {
        let style = currentStyle
        guard let gaugeStyle = QuotaIconStyle(rawValue: style) else { return }
        let coloring = IconColoring(
            rawValue: UserDefaults.standard.string(forKey: IconColoring.storageKey) ?? ""
        ) ?? .warningOnly
        presentedAnimationKey = nil
        let stale = readingIsStaleNow
        drawnStale = stale
        controller?.setStaticIcon(
            TrayIcons.image(
                style: gaugeStyle, remaining: quotaRemaining,
                dark: controller?.isDarkAppearance ?? true,
                coloring: coloring, stale: stale),
            isTemplate: false)
    }

    /// Internal so the settings window's preview can use the same last-good
    /// reading before its own quota fetch lands.
    nonisolated static let lastRemainingKey = "tokenbar.quota.lastRemaining"

    /// Remaining percent reconciled from the most recent successful outer
    /// payload. Live mode seeds this from UserDefaults so an outer FFI failure
    /// before any new payload preserves the last-good reading; demo mode keeps
    /// only the process-local synthetic value.
    private var cachedQuotaRemaining: Double?

    /// The selected quota window's remaining percent. A missing outer payload
    /// may use the last-good scalar; a successful payload must resolve a fresh,
    /// finite value from its own windows.
    var quotaRemaining: Double? {
        let persistedSelection = UserDefaults.standard.string(forKey: Self.quotaSourceKey)
            ?? QuotaResolver.auto
        return QuotaSelectionPolicy.resolveRemainingPercent(
            payload: quota,
            persistedSelection: persistedSelection,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: cachedQuotaRemaining)
    }

    /// Reconcile a successful payload with the scalar cache. A missing outer
    /// payload returns the cache without touching defaults; nil defaults keep
    /// demo mode process-local.
    nonisolated static func applyQuotaRemaining(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        cachedRemaining: Double?,
        defaults: UserDefaults?
    ) -> Double? {
        guard payload != nil else { return cachedRemaining }
        let remaining = QuotaSelectionPolicy.resolveRemainingPercent(
            payload: payload,
            persistedSelection: persistedSelection,
            excluding: excluding,
            cachedRemaining: cachedRemaining)
        if let remaining {
            defaults?.set(remaining, forKey: Self.lastRemainingKey)
        } else {
            defaults?.removeObject(forKey: Self.lastRemainingKey)
        }
        if remaining != nil, let payload,
           let resolvedAt = QuotaSelectionPolicy.resolvedAt(
               payload: payload, persistedSelection: persistedSelection, excluding: excluding)
        {
            defaults?.set(resolvedAt.timeIntervalSince1970, forKey: Self.lastResolvedAtKey)
        } else {
            defaults?.removeObject(forKey: Self.lastResolvedAtKey)
        }
        return remaining
    }

    /// Fetch time of the snapshot behind the persisted scalar (#8), written
    /// beside `lastRemainingKey` by `applyQuotaRemaining`. It only matters when
    /// the outer payload is missing and the cached scalar is drawn; with a
    /// payload, the age is read from the payload itself.
    nonisolated static let lastResolvedAtKey = "tokenbar.quota.lastResolvedAt"

    /// How old the gauge's reading may be before it is drawn as stale. Tens of
    /// minutes, not a small multiple of the 300 s poll. 30 min chosen by the
    /// maintainer (2026-09-27).
    nonisolated static let quotaStaleAfter: TimeInterval = 30 * 60

    /// Whether the drawn reading is older than `quotaStaleAfter` (#8). With a
    /// payload, the age comes from the same resolve that produced the value, so
    /// value and age cannot come from different writers; without one, from the
    /// stamp persisted beside the cached scalar. Unknown age is not stale.
    /// Shared by the tray and the Settings preview. Nil defaults (demo mode)
    /// are never stale.
    nonisolated static func readingIsStale(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        defaults: UserDefaults?,
        now: Date
    ) -> Bool {
        guard let defaults else { return false }
        let resolvedAt = payload.map {
            QuotaSelectionPolicy.resolvedAt(
                payload: $0, persistedSelection: persistedSelection, excluding: excluding)
        } ?? (defaults.object(forKey: lastResolvedAtKey) as? Double)
            .map(Date.init(timeIntervalSince1970:))
        guard let resolvedAt else { return false }
        return now.timeIntervalSince(resolvedAt) > quotaStaleAfter
    }

    private func reconcileQuotaRemaining(with payload: AgentUsagePayload) {
        let defaults = UserDefaults.standard
        cachedQuotaRemaining = Self.applyQuotaRemaining(
            payload: payload,
            persistedSelection: defaults.string(forKey: Self.quotaSourceKey)
                ?? QuotaResolver.auto,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: cachedQuotaRemaining,
            defaults: source.allowsQuotaCachePersistence ? defaults : nil)
    }

    /// Persist a proven legacy-label migration after the payload's scalar state
    /// has been reconciled. Demo mode remains entirely process-local.
    private func persistQuotaSelectionMigration(for payload: AgentUsagePayload) {
        guard source.allowsQuotaCachePersistence else { return }
        let defaults = UserDefaults.standard
        let persistedSelection = defaults.string(forKey: Self.quotaSourceKey)
            ?? QuotaResolver.auto
        if let migrated = QuotaSelectionPolicy.migrationToPersist(
            payload: payload, persistedSelection: persistedSelection)
        {
            defaults.set(migrated, forKey: Self.quotaSourceKey)
        }
    }

    static func applyQuotaPayload(
        _ candidate: AgentUsagePayload,
        store: (AgentUsagePayload) -> Void,
        reconcile: (AgentUsagePayload) -> Void,
        persistSelection: (AgentUsagePayload) -> Void,
        render: () -> Void,
        notify: () -> Void
    ) {
        let payload = AgentUsagePublicationCoordinator.resolve(candidate)
        store(payload)
        reconcile(payload)
        persistSelection(payload)
        render()
        notify()
    }

    private func currentFrames() -> [NSImage] { heldFrames }

    private var animateEnabled: Bool {
#if DEBUG
        if let cpuTest { return cpuTest.animated }
#endif
        return UserDefaults.standard.object(forKey: Self.animateKey) == nil
            || UserDefaults.standard.bool(forKey: Self.animateKey)
    }

    /// Load is tokens/min in units of 10k, capped at `animationCapTokensPerMinute`.
    nonisolated static func animationLoad(tokensPerMinute: Double) -> Double {
        min(max(0, tokensPerMinute), animationCapTokensPerMinute) / 10_000.0
    }

    /// Below this the loop plays at its idle rate.
    nonisolated static let animationFloorTokensPerMinute = 50_000.0
    /// At and above this it plays at its top rate. 1M until the maintainer
    /// moved it to 3M, where busy machines actually run.
    nonisolated static let animationCapTokensPerMinute = 3_000_000.0
    nonisolated static let animationIdleFPS = 2.0
    nonisolated static let animationTopFPS = 40.0

    /// Frame interval in whole milliseconds. Between the floor and the cap
    /// the rate rises on a log scale, so every decade of tokens/min changes
    /// the speed by the same factor; a linear ramp to the cap left the common
    /// 100k–1M range looking almost idle.
    nonisolated static func animationIntervalMilliseconds(load: Double) -> Int {
        let tokensPerMinute = load * 10_000.0
        guard tokensPerMinute > animationFloorTokensPerMinute else {
            return Int(1000.0 / animationIdleFPS)
        }
        let span = log(animationCapTokensPerMinute / animationFloorTokensPerMinute)
        let t = min(1, log(tokensPerMinute / animationFloorTokensPerMinute) / span)
        let fps = animationIdleFPS * pow(animationTopFPS / animationIdleFPS, t)
        return Int(1000.0 / fps)
    }

    nonisolated static func animationLayerSpeed(load: Double) -> Double {
        500.0 / Double(animationIntervalMilliseconds(load: load))
    }

    nonisolated static func effectiveAnimationFPS(load: Double) -> Double {
        2.0 * animationLayerSpeed(load: load)
    }

    nonisolated static func baseAnimationDuration(frameCount: Int) -> Double {
        Double(frameCount) / 2.0
    }

    private var animationSpeed: Float {
        currentStyle == Self.sandStyle
            ? Float(Self.sandLayerSpeed)
            : Float(Self.animationLayerSpeed(load: load))
    }

    // MARK: - Sand (scripts/gen_sand_frames.py)

    /// The sand style shows usage by how much sand falls, not by how fast the
    /// loop plays: each level is its own frame set, all drawn for one fixed
    /// rate. `animationLayerSpeed` plays the cat faster instead.
    nonisolated static let sandStyle = "sand"
    nonisolated static let sandLevels = 4
    /// The rate the sand frames are drawn for (`FPS` in the generator). The
    /// layer plays a set at 2 fps at speed 1 (`baseAnimationDuration`).
    nonisolated static let sandFPS = 24.0
    nonisolated static var sandLayerSpeed: Double { sandFPS / 2.0 }
    /// Level thresholds in tokens/min on the same log scale as the other
    /// styles: idle below 50k, then 300k and 1.5M.
    nonisolated static let sandThresholds: [Double] = [50_000, 300_000, 1_500_000]

    nonisolated static func sandLevel(tokensPerMinute: Double) -> Int {
        sandThresholds.filter { tokensPerMinute >= $0 }.count
    }

    /// Every style that animates (the others are gauges). One list for the
    /// tray, the Settings controls and the pace onboarding card.
    nonisolated static let animatedStyles = ["cat", "parrot", sandStyle]

    /// Every menu-bar icon choice with its label, for Settings and the setup
    /// card, so the two cannot drift apart.
    nonisolated static var iconStyleOptions: [(value: String, label: String)] {
        [("cat", "Spinning cat"), ("parrot", "Party parrot"), (sandStyle, "Sand shoal")]
            + QuotaIconStyle.allCases.map { ($0.rawValue, $0.label) }
    }

    /// A level must be passed by this factor before it changes, so a rate
    /// hovering at a threshold does not swap frame sets every 30 s poll: each
    /// swap restarts the loop and re-rasterises 144 frames on the main thread.
    nonisolated static let sandHysteresis = 1.2

    /// The level to show for `tokensPerMinute` (already pace-scaled), given
    /// the level shown now. Moving up needs the rate to clear the threshold
    /// by `sandHysteresis`; moving down needs it to fall that far below.
    nonisolated static func sandLevel(tokensPerMinute: Double, current: Int?) -> Int {
        let raw = sandLevel(tokensPerMinute: tokensPerMinute)
        guard let current, raw != current else { return raw }
        if raw > current {
            let confirmed = sandLevel(tokensPerMinute: tokensPerMinute / sandHysteresis)
            return max(current, confirmed)
        }
        let confirmed = sandLevel(tokensPerMinute: tokensPerMinute * sandHysteresis)
        return min(current, confirmed)
    }

    private var sandLevelShown: Int?

    /// The demanded sand level. `load` already holds the pace-scaled rate;
    /// every threshold is below the cap, so `load * 10_000` reads it back
    /// without a second pace lookup. Hysteresis preserved.
    private func demandedSandLevel() -> Int {
        let level = Self.sandLevel(tokensPerMinute: load * 10_000, current: sandLevelShown)
        sandLevelShown = level
        return level
    }

    /// First frame only, for animation-disabled state — avoids retaining a
    /// full animated set as a still icon.
    static func loadFirstFrame(directory: String) -> [NSImage] {
        let urls = Bundle.tokenBarResources.urls(
            forResourcesWithExtension: "png", subdirectory: directory) ?? []
        guard let first = urls.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }).first,
              let image = NSImage(contentsOf: first) else { return [] }
        image.size = barSize(for: image)
        return [image]
    }

    /// The held set for the demanded directory, loading on key change and
    /// discarding the obsolete set. Static state keys separately so toggling
    /// animation back on reloads the full set instead of animating one frame.
    /// Unknown/empty directories fall back to the cat set.
    private func useDemandedSet(directory: String, staticOnly: Bool) -> [NSImage] {
        let key = directory + (staticOnly ? "|static" : "")
        if heldKey == key { return heldFrames }
        var loaded = staticOnly ? Self.loadFirstFrame(directory: directory) : Self.loadFrames(directory: directory)
        var resolvedKey = key
        if loaded.isEmpty {
            let fallback = "anim-cat2" + (directory.hasSuffix("-light") ? "-light" : "")
            if fallback != directory {
                loaded = staticOnly ? Self.loadFirstFrame(directory: fallback) : Self.loadFrames(directory: fallback)
                resolvedKey = fallback + (staticOnly ? "|static" : "")
            }
        }
        heldKey = resolvedKey
        heldFrames = loaded
        return loaded
    }

    /// Drops the held set when the tray no longer shows frames (gauge style
    /// or stopped). The renderer's own stop already clears its arrays.
    private func releaseHeldFrames() {
        heldKey = nil
        heldFrames = []
    }

    private func refreshIcon() {
        guard !isStopped else { return }
        let style = currentStyle
        if QuotaIconStyle(rawValue: style) != nil {
            releaseHeldFrames()
            renderGaugeIcon()
            return
        }

        let dark = controller?.isDarkAppearance ?? true
        let level = style == Self.sandStyle ? demandedSandLevel() : 0
        let directory = Self.frameDirectory(style: style, dark: dark, level: level)
        let set = useDemandedSet(directory: directory, staticOnly: !animateEnabled)
        guard let first = set.first else {
            controller?.setAnimatedFrames([], speed: animationSpeed)
            return
        }

        guard animateEnabled else {
            presentedAnimationKey = nil
            controller?.setStaticIcon(first, isTemplate: true)
            return
        }

        if presentedAnimationKey != heldKey {
            presentedAnimationKey = heldKey
            controller?.setAnimatedFrames(set, speed: animationSpeed)
        } else {
            controller?.setAnimationSpeed(animationSpeed)
        }
    }

    private func updateAnimationSpeedIfPresented() {
        guard QuotaIconStyle(rawValue: currentStyle) == nil else { return }
        // Sand changes frame set, not speed, when usage crosses a level. It
        // does so with animation off too: the still frame is the level's dune,
        // so a static sand icon still has to follow usage.
        if currentStyle == Self.sandStyle {
            refreshIcon()
            return
        }
        guard animateEnabled, presentedAnimationKey != nil else { return }
        controller?.setAnimationSpeed(animationSpeed)
    }

#if DEBUG
    private func reportCPUTestReady(_ test: TrayAnimationCPUTestConfiguration) {
        let frameCount = currentFrames().count
        let duration = Self.baseAnimationDuration(frameCount: frameCount)
        // The speed actually handed to the layer: sand plays at its fixed rate.
        let speed = test.animated ? Double(animationSpeed) : 0
        let fps = 2 * speed
        print(String(
            format: "TRAY_CPU_TEST_READY style=%@ animated=%@ frames=%d base_duration=%.3f speed=%.3f fps=%.3f",
            test.style, test.animated.description, frameCount, duration, speed, fps))
        fflush(stdout)
    }
#endif

    /// OAuth quota fetch is network-bound (~30s worst case across four
    /// providers), so refresh on a 5-minute cadence — quota windows move
    /// slowly and the popover has its own faster loop while open.
    private func startQuotaPolling() {
        quotaTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let source = self?.source else { break }
                // Read before the fetch, like the popover's poll: the fetch is
                // network-bound and owns most of the cycle, so a registry change
                // lands during it far more often than during the sleep.
                let registryEpoch = ClaudeExtraRoots.RegistryChange.epoch
                // Not awaited: an attempt may wait on Google, and the quota
                // fetch must not wait on it. A capture that lands mid-fetch
                // wakes this loop through `RegistryChange` like any edit.
                await Self.prepareAntigravityAutoCapture()
                let payload = try? await source.agentUsage()
                guard let self, !Task.isCancelled else { break }
                // A payload built for the previous account set must not be
                // applied. `AgentUsageThrottle.invalidate()` deliberately hands
                // an in-flight result to the waiter that asked for it rather
                // than failing it — the caller asked a question and gets an
                // answer — but this loop's answer becomes the Auto gauge, the
                // coordinator state and a persisted scalar, and the correction
                // is a whole network round away. Dropping it costs one cycle;
                // applying it shows the old account set for that round.
                //
                // The epoch, not the payload's contents: nothing in the payload
                // says which registry produced it.
                guard ClaudeExtraRoots.RegistryChange.epoch == registryEpoch else { continue }
                if let payload {
                    Self.applyQuotaPayload(
                        payload,
                        store: { self.polledQuota = $0 },
                        reconcile: { self.reconcileQuotaRemaining(with: $0) },
                        persistSelection: { self.persistQuotaSelectionMigration(for: $0) },
                        render: { self.renderGaugeIcon() },
                        notify: { self.onQuotaUpdated?() })
                } else {
                    // Nothing changed but the reading's age; re-render so the
                    // stale marker (#8) can appear without a successful poll.
                    self.renderGaugeIcon()
                }
                // Interruptible. This poll is the Auto gauge's only source, and
                // it is the one poll that runs with no window open — the launch
                // race and every change made while the popover is closed reach
                // the tray through here and nowhere else. A plain 300-second
                // sleep meant an added account could be missing from the gauge,
                // or a removed one still counted in it, for five minutes.
                await ClaudeExtraRoots.RegistryChange.sleep(upTo: 300, since: registryEpoch)
            }
        }
    }

    /// Automatic capture's trigger, once per quota-poll iteration (the first
    /// one is the launch check). The toggle is checked HERE, at the call site:
    /// with it off nothing reaches the core, not even the attributes-only
    /// marker query. Returns the started check so the selftest can await it.
    @discardableResult
    static func pollAntigravityAutoCapture(
        defaults: UserDefaults = .standard,
        autoCapture: AntigravityAutoCapture? = nil
    ) -> Task<Void, Never>? {
        guard defaults.bool(forKey: AntigravityAutoCapture.enabledKey) else { return nil }
        return Task {
            await prepareAntigravityAutoCapture(defaults: defaults, autoCapture: autoCapture)?.value
        }
    }

    /// What both poll loops await before a quota fetch. Off: nothing reaches
    /// the core. On: the marker is read and a changed login forgets the
    /// current account before the fetch; the returned capture attempt runs on
    /// its own.
    @discardableResult
    static func prepareAntigravityAutoCapture(
        defaults: UserDefaults = .standard,
        autoCapture: AntigravityAutoCapture? = nil
    ) async -> Task<Void, Never>? {
        guard defaults.bool(forKey: AntigravityAutoCapture.enabledKey) else { return nil }
        return await (autoCapture ?? .shared).prepareForFetch()
    }

    /// The raw tokens/min value from the last load poll — exposed so the
    /// tray title can display it without its own FFI call.
    private(set) var tokensPerMinRate: Double?

    /// Invalidated on cancellation and every replacement fetch, before any
    /// await. An old result cannot land even while its replacement is pending.
    private var rateGeneration = 0

    nonisolated static func needsLocalRate(style: String, animated: Bool, mode: TrayMode) -> Bool {
        mode == .tokensPerMin || style == sandStyle
            || (QuotaIconStyle(rawValue: style) == nil && animated)
    }

    private var needsLocalRate: Bool {
        Self.needsLocalRate(style: currentStyle, animated: animateEnabled, mode: .current)
    }

    /// The animator owns menu-bar publication; the scheduler owns ingestion.
    private func reconcileLoadDemand() {
        guard !isStopped else { return }
        rateGeneration += 1
        let generation = rateGeneration
        guard needsLocalRate, let query = LiveRate.query() else {
            if let rateSession { scheduler.release(rateSession) }
            rateSession = nil
            if needsLocalRate { applyRate(0, generation: generation) }
            return
        }
        let session = rateSession ?? scheduler.openSession()
        rateSession = session
        scheduler.setUsageReset(session) { [weak self] in
            guard let self else { return nil }
            self.tokensPerMinRate = nil
            self.load = 0
            self.updateAnimationSpeedIfPresented()
            self.onQuotaUpdated?()
            return nil
        }
        scheduler.update(session, queries: [query], active: true) { [weak self] _, result in
            guard case .success(let value) = result, let rate = LiveRate.rate(from: value) else { return }
            self?.applyRate(rate, generation: generation)
        }
    }

    func refreshFilteredRate() { reconcileLoadDemand() }

    private func applyRate(_ rate: Double, generation: Int) {
        guard !isStopped, needsLocalRate, generation == rateGeneration else { return }
        load = Self.animationLoad(tokensPerMinute: AnimationPace.current().scaled(rate))
        tokensPerMinRate = rate
        updateAnimationSpeedIfPresented()
        onQuotaUpdated?()
    }

}
