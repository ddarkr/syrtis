import Foundation
import TokenBarCore

/// The measurement lane behind `--launch-timeline`: when each part of the
/// dashboard becomes drawable after a cold process start.
///
/// Starts the same scheduler requests and independent provider poll the
/// popover starts when it opens, alongside the open tab's window stages and
/// attributed series, so they contend the way they do in the app, and records
/// the first time each milestone holds. `--refresh-timing` times the stages
/// one after another,
/// which says what each costs alone and nothing about the order a user sees.
///
/// Not reachable from the shipping UI: `main.swift` runs it only for the flag
/// and exits. It reads real local data and fetches the real quota payload.
/// No snapshot is restored or written (`cachesSnapshot: false`), so the graph
/// milestone is the refresh a relaunch waits for behind the restored picture.
/// `--client <id>` picks the open tab; the default is `claude`.
@MainActor
enum LaunchTimelineProbe {
    static func start() -> Never {
        Task { @MainActor in
            await run()
            exit(0)
        }
        dispatchMain()
    }

    private static func run() async {
        setvbuf(stdout, nil, _IONBF, 0)
        let args = CommandLine.arguments
        let client = args.firstIndex(of: "--client")
            .flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil } ?? "claude"
        let t0 = DispatchTime.now()
        func ms() -> Double { Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1e6 }

        let source = TimedSource()
        let scheduler = LocalUsageScheduler(source: source, observesSources: false)
        let modelSession = scheduler.openSession()
        let seriesSession = scheduler.openSession()
        let traySession = scheduler.openSession()
        defer {
            scheduler.release(modelSession)
            scheduler.release(seriesSession)
            scheduler.release(traySession)
        }
        let model = DashboardModel(source: source, scheduler: scheduler, localSession: modelSession)
        let series = AttributedSeriesModel(scheduler: scheduler, localSession: seriesSession)
        let d = UserDefaults.standard
        model.configureQuotaVisibility(
            tabHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""),
            limitsHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""),
            orderRaw: d.string(forKey: ClientRegistry.tabOrderKey) ?? "")
        model.windowUsageClient = client

        // `--only graph,quota,...` starts a subset to find which tasks slow
        // which. Names: graph (one-shot scheduler acquire), quota (independent
        // provider poll), window (the open tab's stages), trace (one-shot),
        // series, and tray (opt-in forced scheduler acquire).
        let only = args.firstIndex(of: "--only")
            .flatMap { $0 + 1 < args.count ? Set(args[$0 + 1].split(separator: ",").map(String.init)) : nil }
        func wants(_ name: String) -> Bool { only?.contains(name) ?? true }
        var tasks: [Task<Void, Never>] = []
        var trayRefreshDone = false
        if wants("graph") { tasks.append(Task { await model.load() }) }
        if wants("quota") { tasks.append(Task { await model.pollAgentUsage() }) }
        var windowStagesDone = false
        if wants("window") {
            tasks.append(Task {
                model.refreshWindowQuotaHalves()
                await model.refreshWindowUsage()
                windowStagesDone = true
            })
        }
        if wants("trace") { tasks.append(Task { await model.refreshTraceOnce() }) }
        if only?.contains("tray") == true {
            tasks.append(Task {
                do {
                    _ = try await scheduler.acquire(
                        .init(kind: "graph"), session: traySession, force: true)
                    trayRefreshDone = true
                } catch {
                    FileHandle.standardError.write(Data("launch-timeline: tray refresh failed: \(error)\n".utf8))
                }
            })
        }
        if wants("series") {
            tasks.append(Task {
                await series.load(
                    source: source, confirmed: UsageAttribution.confirmed().records)
            })
        }

        var seen: [String: Double] = [:]
        func mark(_ name: String, _ holds: Bool) {
            if holds, seen[name] == nil {
                seen[name] = ms()
                print(String(format: "%8.0f ms  %@", seen[name]!, name))
            }
        }
        // One milestone per selected task, so a subset never ends with nothing
        // to report. Trace is timed at the source because an empty result is
        // still an answer; window and tray stages use task completion.
        let quotaHalf = "\(client) window card: quota half"
        let cardReady = "\(client) window card: ready"
        let cardNoQuota = "\(client) window card: settled without quota (blocked / no history)"
        let cardScanFailed = "\(client) window card: quota half, usage scan failed"
        let cardSkipped = "\(client) window card: not shown (client hidden or not offered)"
        var milestones: [String] = []
        if wants("graph") { milestones.append("graph") }
        if wants("quota") { milestones += ["quota payload", quotaHalf, cardReady] }
        if wants("window") { milestones.append("window stages finished") }
        if wants("trace") { milestones.append("trace (first answer)") }
        if only?.contains("tray") == true { milestones.append("tray forced refresh") }
        if wants("series") { milestones.append("attributed series") }
        // A card that settles any other way will never be ready: it satisfies
        // the card milestones instead of holding the run until the deadline,
        // under its own name so it is never reported as ready.
        func pending(_ name: String) -> Bool {
            guard seen[name] == nil else { return false }
            let settledOtherwise = seen[cardNoQuota] != nil || seen[cardSkipped] != nil
            if name == quotaHalf { return !settledOtherwise }
            if name == cardReady { return !settledOtherwise && seen[cardScanFailed] == nil }
            return true
        }
        let deadline = 90_000.0
        while milestones.contains(where: pending), ms() < deadline {
            mark("graph", model.stats != nil)
            mark("quota payload", model.agentUsage != nil)
            // Hidden by the saved tab or limits settings, or not a quota
            // client at all: the refresh never builds this card.
            mark(cardSkipped, model.agentUsage != nil && !model.windowCardClients.contains(client))
            switch model.windowCards[client] {
            case let .quotaOnly(_, scanFailed)?:
                mark(quotaHalf, true)
                mark(cardScanFailed, scanFailed)
            case .ready?:
                mark(quotaHalf, true)
                mark(cardReady, true)
            case .blocked?, .noQuotaHistory?:
                mark(cardNoQuota, true)
            default: break
            }
            mark("window stages finished", windowStagesDone)
            mark("trace (first answer)", source.answered("trace"))
            mark("tray forced refresh", trayRefreshDone)
            mark("attributed series", series.points != nil)
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        for name in milestones where pending(name) {
            print("   never  \(name) (within \(Int(deadline / 1000)) s)")
        }
        tasks.forEach { $0.cancel() }
        source.reportTimings()
    }
}

/// Forwards the injected source's complete native lease surface and records
/// both first answers and actual source-sync / compact-query durations.
private final class TimedSource: UsageDataSource, @unchecked Sendable {
    private let live = LiveUsageDataSource()
    private let lock = NSLock()
    private var returned: Set<String> = []

    private var timings: [String: [Double]] = [:]
    func answered(_ call: String) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return returned.contains(call)
    }

    private func note(_ call: String) {
        lock.lock(); returned.insert(call); lock.unlock()
    }

    private func record(_ call: String, since start: DispatchTime) {
        let elapsed = Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1e6
        lock.lock()
        returned.insert(call)
        timings[call, default: []].append(elapsed)
        lock.unlock()
    }

    func reportTimings() {
        lock.lock()
        let samples = timings
        lock.unlock()
        for call in samples.keys.sorted() {
            let values = samples[call]!
            print("\(call) (x\(values.count))  "
                  + values.map { String(format: "%.1f", $0) }.joined(separator: "  ") + " ms")
        }
    }

    var allowsQuotaCachePersistence: Bool { live.allowsQuotaCachePersistence }
    var usesNativeLocalUsage: Bool { true }
    func localUsageSync(_ request: LocalUsageSyncRequest) async throws -> LocalUsageSyncResult {
        let start = DispatchTime.now()
        defer { record("source sync", since: start) }
        return try await live.localUsageSync(request)
    }
    func localUsageQuery(_ request: LocalUsageQueryRequest) async throws -> LocalUsageValue {
        let start = DispatchTime.now()
        var kind = "failed"
        defer { record("compact pure query: \(kind)", since: start) }
        let value = try await live.localUsageQuery(request)
        switch value {
        case .graph: kind = "graph"
        case .models: kind = "models"
        case .hourly: kind = "hourly"
        case .agents: kind = "agents"
        case .rate: kind = "rate"
        case .trace: kind = "trace"; note("trace")
        }
        return value
    }
    func releaseLocalUsage(_ token: String) async {
        await live.releaseLocalUsage(token)
    }
    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        try await live.graph(year: year, priority: priority)
    }
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        defer { note("refreshGraph") }
        return try await live.refreshGraph(year: year, priority: priority)
    }
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        try await live.modelReport(year: year, priority: priority)
    }
    func hourlyReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> HourlyReport {
        try await live.hourlyReport(year: year, clients: clients, priority: priority)
    }
    func agentsReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> AgentsReport {
        try await live.agentsReport(year: year, clients: clients, priority: priority)
    }
    func agentUsage() async throws -> AgentUsagePayload { try await live.agentUsage() }
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        defer { note("trace") }
        return try await live.usageTrace(windowSecs: windowSecs)
    }
    func tokensPerMin() async throws -> Double { try await live.tokensPerMin() }
    func windowUsage(_ request: WindowUsageRequest) async throws -> WindowUsageBatch {
        try await live.windowUsage(request)
    }
    func releaseWindowUsage(_ snapshotToken: String) async {
        await live.releaseWindowUsage(snapshotToken)
    }
    func quotaCurve(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) async throws -> QuotaCurve? {
        try await live.quotaCurve(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
    func quotaCurveSync(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) throws -> QuotaCurve? {
        try live.quotaCurveSync(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
}
