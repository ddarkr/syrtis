import Foundation
import TokenBarCore

/// The measurement lane behind `--refresh-timing`: how long the synchronous
/// quota refresh holds the main actor on real local data.
///
/// Drives the production `DashboardModel` through an explicitly injected
/// scheduler, the independent provider poll, and the open tab's window stages.
/// Source sync and pure compact query calls are timed separately; the quota
/// half's synchronous main-actor work is measured directly.
///
/// Not reachable from the shipping UI: `main.swift` runs it only for the flag
/// and exits. It reads real local data and fetches the real quota payload,
/// like `--window-probe`. The window-switch pass writes the window selection
/// (`WindowCardLoader.selectionKey`) of whichever defaults domain it runs in
/// and restores the previous value before exiting.
///
/// `--client <id>` picks the tab; the default is `claude`.
@MainActor
enum RefreshTimingProbe {
    static func start() -> Never {
        Task { @MainActor in
            await run()
            exit(0)
        }
        dispatchMain()
    }

    private static func ms(_ start: DispatchTime) -> Double {
        Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1_000_000
    }

    private static func run() async {
        setvbuf(stdout, nil, _IONBF, 0)  // interleave with stderr in order
        let counting = CountingSource()
        let scheduler = LocalUsageScheduler(source: counting, observesSources: false)
        let session = scheduler.openSession()
        defer { scheduler.release(session) }
        let model = DashboardModel(source: counting, scheduler: scheduler, localSession: session)
        let d = UserDefaults.standard
        model.configureQuotaVisibility(
            tabHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""),
            limitsHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""),
            orderRaw: d.string(forKey: ClientRegistry.tabOrderKey) ?? "")

        var before = counting.snapshot()
        var t = DispatchTime.now()
        await model.load()
        print(String(format: "load (graph)                 %8.1f ms", ms(t)))
        counting.report(since: before)

        t = DispatchTime.now()
        before = counting.snapshot()
        let poll = Task { await model.pollAgentUsage() }
        // Settled without a payload means the fetch failed, and the poll would
        // only retry after its interval; the deadline covers a fetch that never
        // settles. Either way there is nothing to time, so say so and stop.
        let deadline = DispatchTime.now().uptimeNanoseconds + 120 * 1_000_000_000
        while model.agentUsage == nil, !model.agentUsageAttempted,
              DispatchTime.now().uptimeNanoseconds < deadline {
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        poll.cancel()
        guard model.agentUsage != nil else {
            FileHandle.standardError.write(Data("refresh-timing: no quota payload (fetch failed or timed out after 120s)\n".utf8))
            exit(1)
        }
        print(String(format: "first quota payload          %8.1f ms", ms(t)))
        counting.report(since: before)

        let args = CommandLine.arguments
        let client = args.firstIndex(of: "--client")
            .flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil } ?? "claude"
        model.windowUsageClient = client
        print("client                       \(client)")

        // First window demand acquires its source scope through the scheduler.
        before = counting.snapshot()
        t = DispatchTime.now()
        model.refreshWindowQuotaHalves()
        print(String(format: "quota half (main actor)      %8.1f ms", ms(t)))

        t = DispatchTime.now()
        await model.refreshWindowUsage()
        print(String(format: "window usage (async total)   %8.1f ms", ms(t)))
        counting.report(since: before)

        // Warm quota halves only read the held compact results.
        var samples: [Double] = []
        for _ in 0..<5 {
            t = DispatchTime.now()
            model.refreshWindowQuotaHalves()
            samples.append(ms(t))
        }
        print("quota half, held (x5)        "
              + samples.map { String(format: "%.1f", $0) }.joined(separator: "  ") + " ms")

        // Window switches select another compact job set. Count actual native
        // sync / query calls rather than treating every refresh as a scan.
        let windows = model.agentUsage?.agents.first { $0.clientId == client }?.uniqueCardWindows ?? []
        let saved = d.string(forKey: WindowCardLoader.selectionKey)
        defer { d.set(saved, forKey: WindowCardLoader.selectionKey) }
        for window in windows + windows {
            d.set("\(client)|\(window.cardId)", forKey: WindowCardLoader.selectionKey)
            before = counting.snapshot()
            t = DispatchTime.now()
            model.refreshWindowQuotaHalves()
            let quotaHalfMs = ms(t)
            t = DispatchTime.now()
            await model.refreshWindowUsage()
            print(String(format: "switch → %-24@ quota half %6.1f ms  window total %8.1f ms",
                         window.cardId, quotaHalfMs, ms(t)))
            counting.report(since: before)
        }
    }
}

/// Counts actual source syncs separately from pure compact queries, including
/// the existing window lease lane used by the card.
private final class CountingSource: UsageDataSource, @unchecked Sendable {
    private let live = LiveUsageDataSource()
    private let lock = NSLock()
    struct Measurements {
        var syncCalls = 0
        var syncMs = 0.0
        var queryCalls = 0
        var queryMs = 0.0
        var windowScans = 0
        var windowQueries = 0
    }
    private var measurements = Measurements()

    func snapshot() -> Measurements {
        lock.lock(); defer { lock.unlock() }
        return measurements
    }

    private func record(sync: Bool, since start: DispatchTime) {
        let elapsed = Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1e6
        lock.lock(); defer { lock.unlock() }
        if sync {
            measurements.syncCalls += 1
            measurements.syncMs += elapsed
        } else {
            measurements.queryCalls += 1
            measurements.queryMs += elapsed
        }
    }

    private func recordWindow(capturesSnapshot: Bool) {
        lock.lock(); defer { lock.unlock() }
        if capturesSnapshot { measurements.windowScans += 1 }
        else { measurements.windowQueries += 1 }
    }

    func report(since before: Measurements) {
        let after = snapshot()
        print(String(format: "  source sync x%d %8.1f ms; compact pure query x%d %8.1f ms; window sync x%d / query x%d",
                     after.syncCalls - before.syncCalls, after.syncMs - before.syncMs,
                     after.queryCalls - before.queryCalls, after.queryMs - before.queryMs,
                     after.windowScans - before.windowScans, after.windowQueries - before.windowQueries))
    }
    var allowsQuotaCachePersistence: Bool { live.allowsQuotaCachePersistence }
    var usesNativeLocalUsage: Bool { true }
    func localUsageSync(_ request: LocalUsageSyncRequest) async throws -> LocalUsageSyncResult {
        let start = DispatchTime.now()
        defer { record(sync: true, since: start) }
        return try await live.localUsageSync(request)
    }
    func localUsageQuery(_ request: LocalUsageQueryRequest) async throws -> LocalUsageValue {
        let start = DispatchTime.now()
        defer { record(sync: false, since: start) }
        return try await live.localUsageQuery(request)
    }
    func releaseLocalUsage(_ token: String) async {
        await live.releaseLocalUsage(token)
    }
    func windowUsage(_ request: WindowUsageRequest) async throws -> WindowUsageBatch {
        recordWindow(capturesSnapshot: request.snapshotToken == nil)
        return try await live.windowUsage(request)
    }
    func releaseWindowUsage(_ snapshotToken: String) async {
        await live.releaseWindowUsage(snapshotToken)
    }
    func quotaCurveSync(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) throws -> QuotaCurve? {
        try live.quotaCurveSync(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
    func quotaCurve(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) async throws -> QuotaCurve? {
        try await live.quotaCurve(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload { try await live.graph(year: year, priority: priority) }
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload { try await live.refreshGraph(year: year, priority: priority) }
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport { try await live.modelReport(year: year, priority: priority) }
    func hourlyReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> HourlyReport { try await live.hourlyReport(year: year, clients: clients, priority: priority) }
    func agentsReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> AgentsReport { try await live.agentsReport(year: year, clients: clients, priority: priority) }
    func agentUsage() async throws -> AgentUsagePayload { try await live.agentUsage() }
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] { try await live.usageTrace(windowSecs: windowSecs) }
    func tokensPerMin() async throws -> Double { try await live.tokensPerMin() }
}
