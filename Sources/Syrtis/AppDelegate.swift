import AppKit
import TokenBarCore

/// App bootstrap: accessory activation policy (menu-bar only, no Dock icon),
/// the status-item controller, and the tray-title refresh loop.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private static let intervalKey = "tokenbar.refresh.intervalMin"

    private let usageSource: any UsageDataSource
    private lazy var localScheduler = LocalUsageScheduler(source: usageSource)
    private var backgroundUsageSession: LocalUsageScheduler.Session?
    private var installedRootGeneration = UserDefaults.standard.integer(forKey: ClaudeExtraRoots.generationKey)

    init(source: any UsageDataSource = UsageDataSources.current) {
        usageSource = source
        super.init()
    }
    private var statusController: StatusItemController?
    private var trayAnimator: TrayAnimator?
    private var titleRefreshTask: Task<Void, Never>?
    private var defaultsObserver: NSObjectProtocol?
    private var defaultsApplyScheduled = false
    // Last good fetches — a failed refresh keeps showing these.
    private var lastGraph: UsagePayload?
    private var trayProjection: TrayDisplayProjection?
    private var discordProjection: TrayDisplayProjection?
    private var refreshIntervalMin = AppDelegate.readIntervalMin()
    // Value gate for immediate filtered-rate replacement. Unrelated defaults
    // writes do not restart the local-source reader.
    private var lastHiddenRaw = UserDefaults.standard.string(forKey: ClientRegistry.tabHiddenKey) ?? ""
    // Value gate for the Claude extra-scan-roots list, same discipline as
    // lastHiddenRaw: didChangeNotification fires for every unrelated write,
    // so only re-push to the FFI registry when this specific key actually
    // changed (e.g. Settings added/removed a config dir).
    private var lastExtraRootsRaw = UserDefaults.standard.string(forKey: ClaudeExtraRoots.storageKey) ?? ""
    // The Discord presence client, or nil whenever this process may not connect
    // (switched off, or a demo/test run). Only makeDiscordClient creates it.
    private var discord: DiscordIPCClient?
    // Value gate for the opt-in switch, same discipline as lastHiddenRaw:
    // didChangeNotification carries no key and fires for every write.
    private var lastDiscordEnabled = false
    // Third value gate, same discipline. Without it, turning whole dollars back
    // off would leave the finer figure on a public profile until the next tray
    // poll — as long as five minutes, with nothing to recall it.
    private var lastCostStyle = DiscordPresence.CostStyle.banded
    /// Same discipline again. Compared as a parsed SET, so a reordered or
    /// respaced write is not read as a change to what gets published.
    private var lastComponents = DiscordPresence.defaultComponents
    /// Fifth value gate. Without it a selection change would wait out the tray
    /// poll, publishing the previous agent's figures for up to five minutes.
    private var lastSelection = DiscordPresence.ClientSelection.mostUsed

    private static func readIntervalMin() -> Int {
        max(1, UserDefaults.standard.object(forKey: intervalKey).flatMap { $0 as? Int } ?? 30)
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        SettingsWindowController.shared.registerSceneIfNeeded()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
#if DEBUG
        if TrayAnimationCPUTestConfiguration.current != nil {
            startTrayAnimationCPUTest()
            return
        }
#endif
        BetaMigration.runIfNeeded() // before anything reads defaults
        ClientRegistry.migrateLegacyOrderKey() // fold the old limits order into the shared tab order, likewise before reads
        // refreshIntervalMin's initializer ran at AppDelegate construction in
        // main.swift, BEFORE the migration above — re-read it now so a migrated
        // (non-default) data-refresh interval is honored this session instead of
        // staying at the pre-migration default until the next defaults write.
        refreshIntervalMin = AppDelegate.readIntervalMin()
        _ = UpdaterService.shared // arm Sparkle when bundled
        // Retain launch timezone provenance for one-shot diagnostic graph APIs.
        AttributedSeriesModel.captureLaunchTimeZone()
        // Push any configured Claude extra scan roots. The FFI registry starts
        // empty every process launch (it's an in-memory RwLock, not persisted
        // core-side; UserDefaults is the source of truth Swift owns).
        //
        // This races the first scan by design and does not block launch. The
        // setter probes each path with `read_dir`, so an unmounted volume —
        // the case the keep-and-retry behavior exists for — could otherwise
        // stall startup. Losing that race costs one scan: the extra roots join
        // the next one. Freezing the menu bar until a dead network mount times
        // out costs considerably more.
        ClaudeExtraRoots.apply()

        // Same split for the same reason: the Keychain consent registry is
        // also an in-memory RwLock that starts empty, so Swift re-applies the
        // user's stored answer. Only a granted answer makes a call — an empty
        // registry already denies, which is exactly right both for someone who
        // has not been asked and for someone who said no.
        GrokBotKeychainConsent.applyIfGranted()

        // Captured Antigravity accounts: same in-memory registry story. The
        // label resolver goes first so no card can render a key as its label.
        AntigravityAccounts.installLabelResolver()
        AntigravityAccounts.apply()

        // After the Syrtis rename, repoint a Dock tile still pinned to the
        // old bundle file name. Background queue; a no-op unless it matches.
        DockPinRepair.runIfNeeded()

        localScheduler.fullVerificationInterval = Double(refreshIntervalMin) * 60
        SettingsWindowController.shared.configure(scheduler: localScheduler, source: usageSource)
        let controller = StatusItemController(scheduler: localScheduler, source: usageSource)
        statusController = controller
        let animator = TrayAnimator(controller: controller, source: usageSource, scheduler: localScheduler)
        trayAnimator = animator
        controller.quotaPayloadProvider = { [weak animator] in animator?.quota }
        // A fresh quota or rate fetch re-renders the title right away.
        animator.onQuotaUpdated = { [weak self] in self?.applyMenuBarState() }
        animator.start()
        startTitleRefresh()
        reconcileBackgroundUsageDemand()

        // didChangeNotification carries no key and fires for EVERY write — the
        // popover height slider mid-drag, active tab, lens, quota cache. Coalesce
        // onto the next main-queue turn so one burst of writes costs one pass
        // (which now also creates status items and renders their 1x/2x images)
        // instead of one per write, without adding a timer or poller. The
        // interval and hidden-set restarts stay value-gated inside the pass for
        // the same reason: an unconditional restart turned each unrelated write
        // into a forced full log re-read.
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.scheduleDefaultsApply() }
        }

        // Debug hooks: `--open-popover` shows the popover shortly after
        // launch, `--settings` the settings window — both screenshot aids.
        if CommandLine.arguments.contains("--open-popover") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                controller.showPopover()
            }
        }
        if CommandLine.arguments.contains("--settings") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                SettingsWindowController.shared.show()
            }
        }
        if CommandLine.arguments.contains("--icon-gallery") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                IconGalleryWindowController.show()
            }
        }

        // The launch-time Discord alert is replaced by the Discord setup card
        // (OnboardingSetupCards), which asks the same question without a modal.
        lastDiscordEnabled = DiscordPresence.enabled()
        lastCostStyle = DiscordPresence.costStyle()
        lastComponents = DiscordPresence.components()
        lastSelection = DiscordPresence.selection()
        applyDiscordPresence()
    }

    /// The one place a Discord connection can come into existence, and
    /// therefore the one place the demo/test gate has to hold. `nil` means this
    /// process may not connect at all — a caller that gets `nil` must not build
    /// a client of its own.
    ///
    /// Static and injectable because `SelfTest.run()` returns `Never` before
    /// `NSApplication.shared` exists: the P0 assertion ("a demo run does not
    /// connect, even with the preference forced on") has to be able to call the
    /// real production gate without an app lifecycle.
    nonisolated static func makeDiscordClient(
        existing: DiscordIPCClient? = nil,
        arguments: [String] = CommandLine.arguments,
        enabled: Bool = DiscordPresence.enabled()
    ) -> DiscordIPCClient? {
        guard DiscordPresence.mayConnect(arguments: arguments, enabled: enabled) else { return nil }
        return existing ?? DiscordIPCClient(connect: DiscordIPC.connectToDiscord)
    }

    /// How one write of the tab-hidden preference changed what may be
    /// published: something newly hidden, something put back, or neither.
    ///
    /// Set membership, not size. Hiding one client while unhiding another is a
    /// single write of one comma-separated string, and a subset or count test
    /// reads that as no change at all — leaving the newly hidden client named
    /// on a public profile for the rest of the publish floor. What matters is
    /// which direction the membership moved, and a write can move both.
    ///
    /// A write that does both is `.reducing`. The content the user removed
    /// outranks the sampling rate: publishing it late is a client they hid
    /// still visible to everyone, publishing the other one early is a sample
    /// of activity they had already consented to publish.
    /// Did the user take something off the profile, or put something back?
    ///
    /// Asked directly, against the clients published at EITHER endpoint,
    /// rather than by comparing two effective sets. A set comparison cannot
    /// tell "removed because hidden" from "removed because deselected", and
    /// those want different answers: only the first is a promise with a
    /// deadline. Coalescing makes the distinction load-bearing — switching
    /// from claude to codex while newly hiding claude is one turn, and
    /// classifying its hidden delta under codex alone reports no change while
    /// the old payload keeps claude on the profile for the rest of the floor.
    nonisolated static func visibilityChange(
        previousHiddenRaw: String, hiddenRaw: String,
        previousSelection: DiscordPresence.ClientSelection = .mostUsed,
        selection: DiscordPresence.ClientSelection = .mostUsed,
        contributors: Set<String>? = nil
    ) -> DiscordIPC.VisibilityChange {
        let previousHidden = ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(previousHiddenRaw))
        let currentHidden = ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw))
        let wasPublished = effectivePublished(
            selection: previousSelection, hidden: previousHidden, contributors: contributors)
        let isPublished = effectivePublished(
            selection: selection, hidden: currentHidden, contributors: contributors)
        // Something that WAS on the profile is now hidden.
        if !wasPublished.isDisjoint(with: currentHidden.subtracting(previousHidden)) {
            return .reducing
        }
        // Something just unhidden is on it now.
        if !isPublished.isDisjoint(with: previousHidden.subtracting(currentHidden)) {
            return .increasing
        }
        return .none
    }

    /// The one direction test both set-shaped preferences use. The parameter
    /// names the set whose GROWTH means less is published — the hidden set for
    /// clients, and the complement for components, which is why the component
    /// caller passes its two sets the other way round.
    nonisolated static func subsetChange<T: Hashable>(
        grownMeansLess previous: Set<T>, to current: Set<T>
    ) -> DiscordIPC.VisibilityChange {
        if !current.isSubset(of: previous) { return .reducing }
        if !previous.isSubset(of: current) { return .increasing }
        return .none
    }

    /// Components are the mirror image of the hidden set: UNTICKING one takes
    /// something off the profile, so a shrinking selection is the reduction.
    /// The arguments go into the same subset test swapped, rather than into a
    /// second classifier with its own idea of which way is which.
    /// A selection change replaces what is published rather than narrowing or
    /// widening it, so anything computed for the previous one is stale.
    ///
    /// Compared as EFFECTIVE published sets — the selection intersected with
    /// what is visible — and not as raw selections, so switching between two
    /// clients that are both hidden is correctly no change at all. The hidden
    /// set's own movement is classified separately by `visibilityChange`; this
    /// isolates the selection's contribution by holding it fixed.
    /// What a given selection would actually publish under a given hidden set.
    /// Every set-shaped classification below compares two of these rather than
    /// the raw preferences, so a change that cannot move a published byte is
    /// correctly no change at all.
    /// `contributors` narrows this to the clients that actually have a stripe
    /// today. Without it `.mostUsed` expands to the entire registry, so hiding
    /// a registered client that contributed nothing reads as a reduction —
    /// and while Discord is offline that grant cannot be spent, so a later
    /// payload carrying genuinely new activity inherits it and goes out inside
    /// the floor. Nil means "do not narrow", which is what the pure-function
    /// assertions use.
    nonisolated static func effectivePublished(
        selection: DiscordPresence.ClientSelection, hidden: Set<String>,
        contributors: Set<String>? = nil
    ) -> Set<String> {
        let registered = Set(ClientRegistry.allIds)
        let selected: Set<String>
        switch selection {
        case .mostUsed: selected = registered
        case .only(let id): selected = registered.contains(id) ? [id] : []
        case .malformed: selected = []
        }
        let visible = selected.subtracting(hidden)
        return contributors.map(visible.intersection) ?? visible
    }

    /// A selection change replaces what is published rather than narrowing or
    /// widening it, so anything computed for the previous one is stale.
    nonisolated static func selectionChange(
        previous: DiscordPresence.ClientSelection,
        current: DiscordPresence.ClientSelection,
        hidden: Set<String>
    ) -> DiscordIPC.VisibilityChange {
        effectivePublished(selection: previous, hidden: hidden)
            == effectivePublished(selection: current, hidden: hidden) ? .none : .retiring
    }

    nonisolated static func componentsChange(
        previous: Set<DiscordPresence.Component>, current: Set<DiscordPresence.Component>
    ) -> DiscordIPC.VisibilityChange {
        subsetChange(grownMeansLess: current, to: previous)
    }

    /// The cost switch read the same way the hidden set is: which direction did
    /// the published content move.
    ///
    /// Whole dollars → banded is a reduction. The figure the user had on their
    /// profile is replaced by a coarser one, and until that lands the precise
    /// one is still up. Banded → whole dollars adds precision back and is
    /// throttled like any other sample.
    /// `publishedInBoth` is whether cost is part of the composition on BOTH
    /// sides of the change. When it is not, the style cannot alter a single
    /// published byte, so calling it a reduction would retire work that is not
    /// actually stale.
    ///
    /// Both sides, not just the current one: a cost that was just ticked or
    /// unticked is already classified by `componentsChange`, and the style it
    /// arrives or leaves with is part of that same change.
    nonisolated static func costStyleChange(
        previous: DiscordPresence.CostStyle, current: DiscordPresence.CostStyle,
        publishedInBoth: Bool = true
    ) -> DiscordIPC.VisibilityChange {
        guard publishedInBoth else { return .none }
        switch (previous, current) {
        case (.wholeDollars, .banded): return .reducing
        case (.banded, .wholeDollars): return .increasing
        default: return .none
        }
    }

    /// Reconcile the presence with the current preferences and the cached
    /// graph. Start, stop and publish all run through here so there is a single
    /// gated path, and every call site is just a trigger.
    private func applyDiscordPresence(visibility: DiscordIPC.VisibilityChange = .none) {
        guard let client = AppDelegate.makeDiscordClient(existing: discord) else {
            // Switched off, or a demo/test run. `stop()` clears the activity on
            // Discord's side before closing, so a client that existed a moment
            // ago does not leave its last payload on the profile.
            //
            // The reference is deliberately NOT dropped here. `stop()` only
            // *queues* the clear, and `applicationWillTerminate`'s bounded
            // drain is guarded on this being non-nil — so nil-ing it means
            // switching Discord off and immediately quitting abandons the clear
            // as the process exits, and the last activity stays on the profile
            // after consent was withdrawn. A stopped client costs a closed
            // socket's worth of nothing, and re-enabling reuses it: the gate in
            // `makeDiscordClient` is what decides whether it may run, never the
            // presence of the object.
            discord?.stop()
            return
        }
        discord = client
        client.start()
        client.publish(discordPayload(), visibility: visibility)
    }

    /// What would be published right now: today's figures from the last good
    /// graph, with the tab-hidden clients excluded.
    ///
    /// `hiddenClients()` and deliberately not `quotaExcludedClients()` or
    /// `hiddenLimitsClients()` — those are different sets, and this one is what
    /// `trayTotals` folds the very same stripes with. Nothing is published
    /// before the first graph arrives.
    private func discordPayload() -> DiscordPresence.Payload? {
        guard let discordProjection else { return nil }
        return DiscordPresence.payload(
            projection: discordProjection,
            hidden: ClientRegistry.hiddenTabClients(),
            today: Format.todayKey(),
            // Read here and passed down. `DiscordPresence` performs no
            // preference lookup of its own, so the privacy assertions cannot
            // come to depend on the defaults of whatever machine runs them.
            costStyle: DiscordPresence.costStyle(),
            components: DiscordPresence.components(),
            selection: DiscordPresence.selection())
    }

    private func scheduleDefaultsApply() {
        guard !defaultsApplyScheduled else { return }
        defaultsApplyScheduled = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.defaultsApplyScheduled = false
            self.applyMenuBarState()

            let next = AppDelegate.readIntervalMin()
            if next != self.refreshIntervalMin {
                self.refreshIntervalMin = next
                self.localScheduler.fullVerificationInterval = Double(next) * 60
            }

            // A visibility change alters the filtered live rate, which applyMenuBarState
            // cannot recompute from its cached rate. Value-gate the extra fetch.
            let hiddenRaw = UserDefaults.standard.string(
                forKey: ClientRegistry.tabHiddenKey) ?? ""
            let previousHiddenRaw = self.lastHiddenRaw
            let hiddenChanged = hiddenRaw != self.lastHiddenRaw
            if hiddenChanged {
                self.lastHiddenRaw = hiddenRaw
                self.trayAnimator?.refreshFilteredRate()
            }

            // Settings added/removed/edited a Claude config dir — push the
            // new list to the FFI registry immediately (D1: no restart).
            let extraRootsRaw = UserDefaults.standard.string(
                forKey: ClaudeExtraRoots.storageKey) ?? ""
            if extraRootsRaw != self.lastExtraRootsRaw {
                self.lastExtraRootsRaw = extraRootsRaw
                ClaudeExtraRoots.apply()
            }

            let installed = UserDefaults.standard.integer(forKey: ClaudeExtraRoots.generationKey)
            if installed != self.installedRootGeneration {
                self.installedRootGeneration = installed
                self.localScheduler.rootsChanged()
            }
            self.reconcileBackgroundUsageDemand()

            // Hiding a client has to leave the published presence in the SAME
            // turn — waiting for the next refresh cycle would keep that client
            // on a public profile for up to five minutes. Value-gated on the
            // same two raw values so an unrelated write does not re-publish.
            let discordEnabled = DiscordPresence.enabled()
            let costStyle = DiscordPresence.costStyle()
            let previousCostStyle = self.lastCostStyle
            let components = DiscordPresence.components()
            let previousComponents = self.lastComponents
            let selection = DiscordPresence.selection()
            let previousSelection = self.lastSelection
            if hiddenChanged || discordEnabled != self.lastDiscordEnabled
                || costStyle != previousCostStyle || components != previousComponents
                || selection != previousSelection {
                // The classification decides only whether earlier queued work
                // is stale, NOT how fast this reaches the wire. Every change
                // waits out the publish floor; the Settings copy states that
                // wait. What a reduction still buys is that a payload computed
                // before it must not be written after it — that would put the
                // client the user removed back on the profile rather than
                // merely being slow.
                // Today's actual contributors, so a hide of a client with no
                // usage today is not mistaken for taking something down.
                let contributors = self.lastGraph.map { graph in
                    Set(graph.contributions.last { $0.date == Format.todayKey() }?
                        .clients.map(\.client) ?? [])
                }
                let change = AppDelegate.visibilityChange(
                    previousHiddenRaw: previousHiddenRaw, hiddenRaw: hiddenRaw,
                    previousSelection: previousSelection, selection: selection,
                    contributors: contributors)
                    .combined(with: AppDelegate.costStyleChange(
                        previous: previousCostStyle, current: costStyle,
                        publishedInBoth: previousComponents.contains(.cost)
                            && components.contains(.cost)))
                    .combined(with: AppDelegate.componentsChange(
                        previous: previousComponents, current: components))
                    .combined(with: AppDelegate.selectionChange(
                        previous: previousSelection, current: selection,
                        hidden: ClientRegistry.hiddenTabClients(ClientRegistry.parseIdSet(hiddenRaw))))
                self.lastDiscordEnabled = discordEnabled
                self.lastCostStyle = costStyle
                self.lastComponents = components
                self.lastSelection = selection
                self.applyDiscordPresence(visibility: change)
            }
        }
    }

#if DEBUG
    /// Hermetic tray-rendering benchmark path. It intentionally skips migrations,
    /// updater startup, defaults observers, and all polling while preserving the
    /// shipping status-item controller, assets, renderer, and teardown ownership.
    private func startTrayAnimationCPUTest() {
        let controller = StatusItemController(scheduler: localScheduler, source: usageSource)
        statusController = controller
        let animator = TrayAnimator(controller: controller, source: usageSource, scheduler: localScheduler)
        trayAnimator = animator
        controller.updateTitle("1.0M/min")
        animator.start()
    }
#endif

    func applicationWillTerminate(_ notification: Notification) {
        titleRefreshTask?.cancel()
        localScheduler.stop()
        trayAnimator?.stop()
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        // Remove the status item / close the popover so ControlCenter tears
        // the menu-bar item down cleanly (avoids the ~40s RunningBoard
        // "waiting on exit context" stall seen on the 2026-06-16 quit).
        statusController?.tearDown()
        if let discord {
            discord.stop()
            // Waited on, not fired and forgotten: `stop()` queues the activity
            // clear on the client's serial queue, and the process is gone
            // moments after this method returns. Whether Discord reliably drops
            // an activity when the socket just closes is unverified, so the
            // clear is the mechanism and the close is the backstop, not the
            // other way round.
            //
            // The wait is bounded on THIS side rather than trusting the
            // client's own socket timeouts. `drainForTesting` is `queue.sync`,
            // which waits for whatever that serial queue is already doing —
            // possibly a `recv` or `send` sitting on its 2s timeout — and has
            // no ceiling of its own. Quit is the wrong place to inherit
            // someone else's worst case: the ~40s RunningBoard stall handled
            // just below is what that looks like to a user. A local sub-KiB
            // frame needs single-digit milliseconds, so 300ms is generous for
            // the clear and cheap when Discord is gone.
            //
            // The name says "ForTesting" because the selftest is its other
            // caller. This use is not a test; do not delete it as one.
            let cleared = DispatchSemaphore(value: 0)
            DispatchQueue.global(qos: .userInitiated).async {
                discord.drainForTesting()
                cleared.signal()
            }
            _ = cleared.wait(timeout: .now() + 0.3)
        }

    }

    /// Re-derive every menu-bar surface from the cached data and the current
    /// settings: the main item's title plus the individual client items.
    /// The rate comes only from the animator's demand-driven 30s reader.
    /// Both halves recompute from cache and the item pass is diffed, so calling
    /// this often is cheap.
    private func applyMenuBarState() {
        let mode = TrayMode.current
        let quotaRemaining = trayAnimator?.quotaRemaining
        // Before the title, so a gauge drawn fresh minutes ago turns grey on
        // the same pass as the title beside it (#420).
        trayAnimator?.syncStaleGauge()
        let rate = trayAnimator?.tokensPerMinRate
        statusController?.updateTitle(
            mode.title(graph: lastGraph, tokensPerMin: rate, quotaRemaining: quotaRemaining,
                       projection: trayProjection),
            color: mode.titleColor(quotaRemaining: quotaRemaining),
            quotaRemaining: mode == .quotaLeft ? quotaRemaining : nil,
            stale: mode == .quotaLeft && (trayAnimator?.readingIsStaleNow ?? false))

        statusController?.reconcileClientItems(ClientTray.runtimePresentations(
            graph: lastGraph,
            payload: trayAnimator?.quota,
            enabled: ClientTray.enabled(),
            selections: ClientTray.selections(),
            hidden: ClientRegistry.hiddenTabClients(),
            officialClients: AgentIconView.availableOfficialClientIDs()))
    }

    /// Keep default display reports prepared even while every window is closed.
    private func reconcileBackgroundUsageDemand() {
        let trayNeedsGraph = TrayMode.current.needsGraph
            || !ClientTray.enabled().subtracting(ClientRegistry.hiddenTabClients()).isEmpty
        let discordNeedsGraph = DiscordPresence.mayConnect(
            arguments: CommandLine.arguments, enabled: DiscordPresence.enabled())
        // lastGraph is last-good display state, not a native lease. Dropping it
        // here blanks resumed titles/client items until a refresh succeeds.
        if trayNeedsGraph {
            if trayProjection == nil, let lastGraph {
                trayProjection = TrayDisplayProjection(graph: lastGraph)
            }
        } else {
            trayProjection = nil
        }
        if discordNeedsGraph {
            if discordProjection == nil, let lastGraph {
                discordProjection = TrayDisplayProjection(graph: lastGraph)
            }
        } else {
            discordProjection = nil
        }
        let session = backgroundUsageSession ?? localScheduler.openSession()
        backgroundUsageSession = session
        let year = DashboardModel.resolveYear()
        let queries: Set<LocalUsageScheduler.Query> = [
            .init(kind: "graph"),
            .init(kind: "graph", year: year),
            .init(kind: "models", year: year)
        ]
        localScheduler.update(session, queries: queries, active: false) { [weak self] query, result in
            guard let self, query.year == nil,
                  case .success(.graph(let graph)) = result else { return }
            self.lastGraph = graph
            self.trayProjection = trayNeedsGraph ? TrayDisplayProjection(graph: graph) : nil
            self.discordProjection = discordNeedsGraph ? TrayDisplayProjection(graph: graph) : nil
            self.applyMenuBarState()
            self.applyDiscordPresence()
        }
    }

    /// Stale quota rendering has its own cheap cadence, even when no local
    /// rate or graph consumer exists. Quota network polling stays independent.
    private func startTitleRefresh() {
        titleRefreshTask = Task { [weak self] in
            while !Task.isCancelled {
                self?.applyMenuBarState()
                self?.applyDiscordPresence()
                try? await Task.sleep(for: .seconds(30))
            }
        }
    }

}
