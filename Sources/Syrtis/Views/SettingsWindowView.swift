import AppKit
import SwiftUI
import TokenBarCore

/// Native settings sidebar with a scrollable form and an optional page preview.
/// Controls and previews share UserDefaults; previews never persist dashboard
/// snapshots or change the popover's transient state.

struct SettingsWindowView: View {
    /// Initial window content size; the window is resizable down to `minSize`.
    static let contentSize = CGSize(width: 1040, height: 700)
    static let minSize = CGSize(width: 800, height: 560)

    // Settings manages process-wide tray preferences, so its client universe
    // stays all-time even when the dashboard is scoped to one saved year. The
    // model still defaults cachesSnapshot to false: it must never overwrite the
    // popover's restore snapshot with its independently polled data.
    @State private var model: DashboardModel
    private var tokensPerMin: Double? { model.liveRate }

    init(scheduler: LocalUsageScheduler, source: any UsageDataSource,
         localSession: LocalUsageScheduler.Session,
         destination: SettingsWindowController.Destination? = nil) {
        _model = State(initialValue: DashboardModel(
            source: source, scheduler: scheduler, localSession: localSession, initialYear: nil))
        self.destination = destination
    }
    /// Set only when a caller asked for a specific place; the intro card is
    /// the one caller today. Nil means an ordinary open, which must land
    /// wherever the user last was.
    var destination: SettingsWindowController.Destination?
    @State private var selectedPage = SettingsPanel.Page.menuBar
    /// Master switch: off hides the preview's Agent-limits card too.
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    /// Observed so the preview (tab list, limits card, trace card) re-derives
    /// the instant the user toggles visibility or reorders in the left panel,
    /// instead of lagging a poller tick behind.
    @AppStorage(ClientRegistry.tabHiddenKey) private var tabsHiddenRaw = ""
    @AppStorage(ClientRegistry.tabOrderKey) private var tabsOrderRaw = ""
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSourceRaw = QuotaResolver.auto
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""
    @AppStorage(ClaudeExtraRoots.generationKey) private var extraRootsGeneration = 0

    /// The user's hidden client set, parsed from the observed raw string.
    private var hiddenClients: Set<String> {
        ClientRegistry.parseIdSet(tabsHiddenRaw)
    }

    /// Reconcile again for every generated payload, selection, or exclusion
    /// change. Legacy/demo payloads have no publication generation, so their
    /// resolved scalar is the content fingerprint that prevents a timestamp
    /// collision from preserving stale defaults.
    private var quotaReconciliationID: String? {
        let excluded = ClientRegistry.quotaExcludedClients()
        let exclusionSignature = [
            excluded.sorted().joined(separator: ","),
            tabsHiddenRaw,
            limitsHiddenRaw,
        ].joined(separator: "|")
        return Self.quotaReconciliationID(
            payload: model.agentUsage,
            persistedSelection: quotaSourceRaw,
            excluding: excluded,
            exclusionSignature: exclusionSignature)
    }

    nonisolated static func quotaReconciliationID(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        exclusionSignature: String
    ) -> String? {
        guard let payload else { return nil }
        let payloadIdentity: String
        if let generation = payload.publicationGeneration {
            payloadIdentity = "generation:\(generation)"
        } else {
            let remaining = QuotaSelectionPolicy.resolveRemainingPercent(
                payload: payload,
                persistedSelection: persistedSelection,
                excluding: excluding,
                cachedRemaining: nil)
            let fingerprint = remaining.map { String($0.bitPattern) } ?? "nil"
            payloadIdentity = "legacy:\(payload.generatedAt):\(fingerprint)"
        }
        return [payloadIdentity, persistedSelection, exclusionSignature]
            .joined(separator: "|")
    }

    private static let pageTop = "settings.page.top"
    private static let featurePages: [SettingsPanel.Page] = [
        .menuBar,
        .dashboard,
        .usageAttribution,
    ]
    private static let appPages: [SettingsPanel.Page] = [
        .general,
        .about,
    ]
    private static let githubURL = URL(string: "https://github.com/Nanako0129/syrtis")!
    private static let sponsorURL = URL(string: "https://www.patreon.com/cw/Nanako0129/membership")!

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var columnVisibility: NavigationSplitViewVisibility = .all

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            sidebarView
        } detail: {
            detailView
        }
        .frame(
            minWidth: Self.minSize.width, idealWidth: Self.contentSize.width,
            minHeight: Self.minSize.height, idealHeight: Self.contentSize.height)
        .task(id: "\(selectedPage.rawValue)|\(previewExpanded)|\(tabsHiddenRaw)") {
            model.configureLocalDemand(view: .daily, clients: [], settings: wantsTracePreview,
                                       models: selectedPage == .usageAttribution)
        }
        // See `PopoverView`: keyed so removing an account takes its card away
        // in the same turn rather than at the poll's convenience.
        .task(id: extraRootsGeneration) { await model.pollAgentUsage() }
        .task(id: quotaReconciliationID) {
            guard let payload = model.agentUsage else { return }
            let defaults = model.allowsQuotaCachePersistence
                ? UserDefaults.standard
                : nil
            _ = Self.applyQuotaRemaining(
                payload: payload,
                persistedSelection: quotaSourceRaw,
                excluding: ClientRegistry.quotaExcludedClients(),
                defaults: defaults)
        }
        // The preview card's Chart layout needs quota curves, and nothing here
        // was asking for them — so picking Chart changed the radio button and
        // nothing else, which reads as a broken setting rather than a setting
        // whose data is absent.
        //
        // Curves only. `windowUsageClient` stays nil deliberately: that is what
        // gates the message scan, and a Settings window must not start a
        // multi-second scan to render a thumbnail. Each curve is a ~2ms read of
        // an already-persisted file.
        //
        // The identity carries the client list because the closure reads it and
        // it arrives with the graph, after this view first appears.
        // Configured, not assigned. Setting `windowCardClients` directly used
        // the usage-derived list, so a quota-only provider — one with a login
        // but no local records — was never in it and its persisted curve was
        // never read, while the preview card still drew the row from the quota
        // payload. The row appeared with a plain bar instead of its chart.
        // `configureQuotaVisibility` hands the model the same three inputs the
        // popover gives it, and `refreshWindowQuotaHalves` derives membership
        // from them plus the quota publication, which is also what keeps it
        // current as later publications arrive.
        .task(id: "\(selectedPage.rawValue)|\(previewExpanded)|\(limitsEnabled)|\(tabsHiddenRaw)|\(tabsOrderRaw)|\(limitsHiddenRaw)") {
            if wantsQuotaCurves {
                model.configureQuotaVisibility(
                    tabHidden: ClientRegistry.parseIdSet(tabsHiddenRaw),
                    limitsHidden: ClientRegistry.parseIdSet(limitsHiddenRaw),
                    orderRaw: tabsOrderRaw)
            } else {
                // No rendered curve while hidden: exclude every registered id so
                // the refresh below prunes retained curves. Real preference keys
                // stay untouched; membership still derives from the quota
                // publication, not the usage-derived preview list.
                model.configureQuotaVisibility(
                    tabHidden: Set(ClientRegistry.allIds),
                    limitsHidden: Set(ClientRegistry.allIds),
                    orderRaw: tabsOrderRaw)
            }
            model.refreshWindowQuotaHalves()
        }
        .onDisappear { model.releaseLocalDemand() }
    }

    private var sidebarView: some View {
        List(selection: $selectedPage) {
            Section {
                ForEach(Self.featurePages) { page in
                    sidebarRow(for: page)
                }
            }
            Section {
                ForEach(Self.appPages) { page in
                    sidebarRow(for: page)
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            sidebarFooter
                .padding(.horizontal, 12)
                .padding(.bottom, 10)
        }
        .navigationSplitViewColumnWidth(min: 190, ideal: 216, max: 260)
    }

    private func sidebarRow(for page: SettingsPanel.Page) -> some View {
        Label {
            Text(page.localizedTitle)
        } icon: {
            Image(systemName: page.symbolName)
                .foregroundStyle(selectedPage == page ? Color.primary : Color.accentColor)
        }
        .tag(page)
    }

    private var detailView: some View {
        ScrollViewReader { proxy in
            Form {
                SettingsPanel(
                    page: selectedPage,
                    agentUsage: model.agentUsage,
                    modelReport: model.modelReport,
                    presentClients: model.stats?.presentClients,
                    isLoading: isInitialLoad,
                    reportLoading: model.modelLoading
                )
                .id(Self.pageTop)

                if showsPreview {
                    Section {
                        previewGroup
                    }
                }
            }
            .formStyle(.grouped)
            .safeAreaInset(edge: .bottom) {
                if model.localError != nil || model.localPending {
                    Text(model.localError == nil ? "Checking local usage…" : "Local usage could not be refreshed.")
                        .font(NativeDesign.detailFont)
                        .foregroundStyle(.secondary)
                        .help(model.localError ?? "")
                        .padding(8)
                }
            }
            .navigationTitle(selectedPage.localizedTitle)
            .onChange(of: selectedPage) {
                previewExpanded = true
                proxy.scrollTo(Self.pageTop, anchor: .top)
            }
            .onChange(of: destination) {
                applyDestination(destination, proxy: proxy)
            }
            .onAppear {
                applyDestination(destination, proxy: proxy)
            }
        }
    }

    private func applyDestination(_ destination: SettingsWindowController.Destination?, proxy: ScrollViewProxy) {
        guard let destination else { return }
        selectedPage = destination.page
        guard let anchor = destination.anchor else { return }
        DispatchQueue.main.async {
            let animation: Animation? = reduceMotion ? nil : .easeOut(duration: 0.25)
            withAnimation(animation) {
                proxy.scrollTo(anchor, anchor: .top)
            }
        }
    }
    /// Trace-600 demand follows the only surface rendering UsageTraceCard:
    /// the expanded Dashboard preview. Curves follow the rendered expanded
    /// limits card on top of that.
    private var wantsTracePreview: Bool {
        selectedPage == .dashboard && previewExpanded
    }
    private var wantsQuotaCurves: Bool {
        selectedPage == .dashboard && previewExpanded && limitsEnabled
    }
    /// Preview is relevant only to the Menu bar and Dashboard pages: the actual
    /// menu-bar strips and the actual cards those pages configure. Every other
    /// page gets the full width for its form, with no unrelated cards stack.
    private var showsPreview: Bool {
        selectedPage == .menuBar || selectedPage == .dashboard
    }

    /// Collapsible via a native control so narrow widths can reclaim the space;
    /// reset to expanded on page switch. Existing copy doubles as the label so
    /// no new string is introduced.
    @State private var previewExpanded = true

    private var previewGroup: some View {
        DisclosureGroup(
            isExpanded: $previewExpanded,
            content: {
                // Unmount while collapsed so PreviewIcon frames and the
                // menu-bar TimelineView release instead of hiding retained.
                if previewExpanded { previewColumn }
            },
            label: {
                Text("Live preview — settings apply immediately.")
                    .font(NativeDesign.detailFont)
                    .foregroundStyle(.secondary)
            })
    }

    /// Clients the preview card renders, derived exactly as the card itself
    /// derives them.
    ///
    /// This is no longer also the curve set: quota-only providers reach the
    /// card through the quota payload rather than through local usage, so the
    /// curves are derived by `refreshWindowQuotaHalves` from the visibility
    /// configured above. Keeping this list as the curve set is what left those
    /// providers charted as plain bars.
    private var previewClients: [String] {
        ClientRegistry.displayClients(
            present: model.stats?.presentClients ?? [],
            hiddenRaw: tabsHiddenRaw, orderRaw: tabsOrderRaw)
    }

    /// Whether either initial request is still in flight. Keyed on request
    /// lifecycle, NOT on payload presence: `stats`/`agentUsage` stay nil when a
    /// fetch fails (`load()` leaves `stats` nil on error and `pollAgentUsage()`
    /// only assigns on success), so waiting for a payload would spin forever
    /// against a persistent failure instead of reaching a terminal state.
    private var isInitialLoad: Bool {
        Self.isInitialLoad(
            phase: model.phase, agentUsageAttempted: model.agentUsageAttempted)
    }

    nonisolated static func isInitialLoad(
        phase: DashboardModel.Phase, agentUsageAttempted: Bool
    ) -> Bool {
        if case .loading = phase { return true }
        return !agentUsageAttempted
    }

    /// Compact native accessory anchoring app identity, running version, and outbound
    /// links to a platform Menu. System controls and fonts without custom hover chips.
    private var sidebarFooter: some View {
        Menu {
            Text("\("Version".localized) \(AppInfo.version)")
            Divider()
            Link(destination: Self.githubURL) {
                Label("GitHub".localized, systemImage: "chevron.left.forwardslash.chevron.right")
            }
            Link(destination: Self.sponsorURL) {
                Label("Sponsor".localized, systemImage: "heart")
            }
        } label: {
            HStack(spacing: 7) {
                BrandMark()
                    .frame(width: 16, height: 16)
                Text(AppInfo.name)
                    .font(NativeDesign.bodyFont.weight(.medium))
            }
        }
        .help(AppInfo.name)
        .accessibilityLabel(AppInfo.name)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    nonisolated static func applyQuotaRemaining(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        defaults: UserDefaults?
    ) -> Double? {
        let cachedRemaining = defaults?.object(forKey: TrayAnimator.lastRemainingKey) as? Double
        return TrayAnimator.applyQuotaRemaining(
            payload: payload,
            persistedSelection: persistedSelection,
            excluding: excluding,
            cachedRemaining: cachedRemaining,
            defaults: defaults)
    }

    // MARK: - Preview column

    private var previewColumn: some View {
        VStack(alignment: .leading, spacing: NativeDesign.spacing) {
            if selectedPage == .menuBar {
                section("Menu bar") {
                    // The stale rule reads the clock, so with polls failing and
                    // the payload unchanged nothing else would redraw the preview
                    // when a reading crosses the 30-minute threshold (#420).
                    TimelineView(.periodic(from: .now, by: 60)) { _ in
                        VStack(spacing: 6) {
                            MenuBarMock(
                                dark: true, graph: model.payload,
                                tokensPerMin: tokensPerMin, agentUsage: model.agentUsage,
                                allowsQuotaCachePersistence: model.allowsQuotaCachePersistence)
                            MenuBarMock(
                                dark: false, graph: model.payload,
                                tokensPerMin: tokensPerMin, agentUsage: model.agentUsage,
                                allowsQuotaCachePersistence: model.allowsQuotaCachePersistence)
                        }
                    }
                }
            }

            if selectedPage == .dashboard {
                if limitsEnabled {
                    section("Agent limits card") {
                        AgentLimitsCard(
                            clients: previewClients,
                            trace: model.trace, agentUsage: model.agentUsage,
                            reorderable: true, curves: model.windowCurves)
                    }
                }

                section("Live session card") {
                    UsageTraceCard(buckets: model.trace, windowSecs: 600, hidden: hiddenClients)
                }
            }
        }
    }

    private func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.localized)
                .font(NativeDesign.detailFont.weight(.semibold))
                .foregroundStyle(.secondary)
            content()
        }
    }

}

/// One mock menu-bar strip (dark or light) with the Syrtis status item
/// rendered from the same inputs the real one uses: TrayIcons gauges or the
/// cat/parrot frame sets, plus TrayMode's title over live data.
private struct MenuBarMock: View {
    let dark: Bool
    let graph: UsagePayload?
    let tokensPerMin: Double?
    let agentUsage: AgentUsagePayload?
    let allowsQuotaCachePersistence: Bool

    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(AnimationPace.storageKey) private var paceRaw = AnimationPace.default.rawValue
    @AppStorage(MenuBarTextColor.storageKey) private var textColorMode = MenuBarTextColor.automatic.rawValue
    @AppStorage(MenuBarTextColor.customColorKey) private var textColorHex = MenuBarTextColor.defaultHex
    @AppStorage(MenuBarTextColor.warningColorKey) private var warningTextColorHex = QuotaColorLevel.warning.defaultHex
    @AppStorage(MenuBarTextColor.criticalColorKey) private var criticalTextColorHex = QuotaColorLevel.critical.defaultHex
    @AppStorage(TrayAnimator.styleKey) private var animationStyle = "cat"
    @AppStorage(TrayAnimator.animateKey) private var animateTray = true
    @AppStorage(IconColoring.storageKey) private var iconColoringRaw = IconColoring.warningOnly.rawValue
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto

    var body: some View {
        let mode = TrayMode(rawValue: trayModeRaw) ?? .todayTokens
        let remaining = quotaRemaining
        let title = mode.title(
            graph: graph, tokensPerMin: tokensPerMin, quotaRemaining: remaining)
        let level = (mode == .quotaLeft ? remaining : nil)
            .map(QuotaColorLevel.init(remaining:)) ?? .normal
        let customHex: String = switch level {
        case .normal: textColorHex
        case .warning: warningTextColorHex
        case .critical: criticalTextColorHex
        }
        let ink: Color = dark ? .white : .black

        HStack(spacing: 10) {
            Text((dark ? "Dark" : "Light").localized)
                .font(.caption2)
                .foregroundStyle(ink.opacity(0.4))
            Spacer()
            // The Syrtis status item, hover-highlighted to stand out.
            HStack(spacing: title.isEmpty ? 0 : 4) {
                icon(remaining: remaining)
                if !title.isEmpty {
                    Text(title)
                        .font(.system(size: 12).monospacedDigit())
                        .foregroundStyle(
                            MenuBarTextColor.applyingStale(
                                MenuBarTextColor.resolve(
                                    automatic: mode.titleColor(quotaRemaining: remaining),
                                    modeRaw: textColorMode, hex: customHex),
                                // Same stale rule as the menu bar and the icon
                                // beside this title (#420).
                                stale: mode == .quotaLeft && TrayAnimator.readingIsStale(
                                    payload: agentUsage,
                                    persistedSelection: quotaSource,
                                    excluding: ClientRegistry.quotaExcludedClients(),
                                    defaults: allowsQuotaCachePersistence
                                        ? .standard : nil,
                                    now: Date()),
                                quotaRemaining: mode == .quotaLeft ? remaining : nil,
                                dark: dark)
                                .map(Color.init(nsColor:)) ?? ink)
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(
                (dark ? Color.white : Color.black).opacity(0.16),
                in: RoundedRectangle(cornerRadius: 5))
            Image(systemName: "wifi")
                .font(.system(size: 11))
                .foregroundStyle(ink.opacity(0.5))
            Text(Self.clock)
                .font(.system(size: 12))
                .foregroundStyle(ink.opacity(0.5))
        }
        .padding(.horizontal, 10)
        .frame(height: 27)
        .background(
            dark ? Color(white: 0.13) : Color(white: 0.93),
            in: RoundedRectangle(cornerRadius: 7))
        .overlay(
            RoundedRectangle(cornerRadius: 7)
                .strokeBorder(Color.primary.opacity(0.12), lineWidth: 1))
    }

    /// Mirrors TrayAnimator.quotaRemaining through the shared payload-aware
    /// policy. Excludes the tab- and limits-hidden clients from the auto pick,
    /// same as the real tray.
    private var quotaRemaining: Double? {
        let cachedRemaining: Double? = allowsQuotaCachePersistence
            ? UserDefaults.standard.object(forKey: TrayAnimator.lastRemainingKey) as? Double
            : nil
        return QuotaSelectionPolicy.resolveRemainingPercent(
            payload: agentUsage,
            persistedSelection: quotaSource,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: cachedRemaining)
    }

    @ViewBuilder
    private func icon(remaining: Double?) -> some View {
        if let gauge = QuotaIconStyle(rawValue: animationStyle) {
            let coloring = IconColoring(rawValue: iconColoringRaw) ?? .warningOnly
            // Same stale rule as the menu bar (#8), evaluated at render time.
            let stale = TrayAnimator.readingIsStale(
                payload: agentUsage,
                persistedSelection: quotaSource,
                excluding: ClientRegistry.quotaExcludedClients(),
                defaults: allowsQuotaCachePersistence ? .standard : nil,
                now: Date())
            Image(nsImage: TrayIcons.image(
                style: gauge, remaining: remaining, dark: dark, coloring: coloring,
                stale: stale))
        } else {
            PreviewIcon(
                style: animationStyle, dark: dark, animate: animateTray,
                interval: frameInterval,
                level: TrayAnimator.sandLevel(
                    tokensPerMinute: (AnimationPace(rawValue: paceRaw) ?? .default)
                        .scaled(tokensPerMin ?? 0)))
        }
    }

    /// Same pacing as TrayAnimator: idle 2 fps up to 50k tok/min, a log ramp
    /// to 40 fps at 3M.
    private var frameInterval: TimeInterval {
        if animationStyle == TrayAnimator.sandStyle { return 1 / TrayAnimator.sandFPS }
        let pace = AnimationPace(rawValue: paceRaw) ?? .default
        return Double(TrayAnimator.animationIntervalMilliseconds(
            load: TrayAnimator.animationLoad(tokensPerMinute: pace.scaled(tokensPerMin ?? 0)))) / 1000
    }

    private static let clock = Date.now.formatted(date: .omitted, time: .shortened)
}

/// One demanded preview icon: holds only the visible style/level/theme set
/// (first frame when static), discarding it when hidden or when the demand
/// changes. Replaces the old unbounded per-directory PreviewFrames cache,
/// which kept every previewed cohort alive for the process lifetime.
private struct PreviewIcon: View {
    let style: String
    let dark: Bool
    let animate: Bool
    let interval: TimeInterval
    let level: Int

    @State private var frames: [NSImage] = []

    private var demand: (key: String, staticOnly: Bool) {
        TrayAnimator.demandedFrames(style: style, dark: dark, level: level, animated: animate)
    }

    var body: some View {
        Group {
            if frames.isEmpty {
                Image(systemName: "chart.bar.fill")
                    .font(.system(size: 12))
            } else if animate {
                TimelineView(.periodic(from: .now, by: interval)) { timeline in
                    let index = Int(
                        timeline.date.timeIntervalSinceReferenceDate / interval)
                        % frames.count
                    Image(nsImage: frames[index])
                }
            } else {
                Image(nsImage: frames[0])
            }
        }
        // Keyed by the single demanded set: a style/level/theme/animate
        // change cancels the old load and retains only the new one. Hidden
        // views drop their frames via onDisappear.
        .task(id: demand.key + (demand.staticOnly ? "|static" : "|anim")) {
            let loaded = demand.staticOnly
                ? TrayAnimator.loadFirstFrame(directory: demand.key)
                : TrayAnimator.loadFrames(directory: demand.key)
            if loaded.isEmpty {
                let fallback = TrayAnimator.frameDirectory(style: "cat", dark: dark)
                frames = demand.staticOnly
                    ? TrayAnimator.loadFirstFrame(directory: fallback)
                    : TrayAnimator.loadFrames(directory: fallback)
            } else {
                frames = loaded
            }
        }
        .onDisappear { frames = [] }
    }
}
