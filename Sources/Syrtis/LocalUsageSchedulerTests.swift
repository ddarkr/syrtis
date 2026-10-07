import Foundation
import TokenBarCore

private actor SchedulerFixtureSource: UsageDataSource {
    enum Failure: Error { case sourceUnavailable, unexpectedLegacyCall }
    nonisolated let allowsQuotaCachePersistence = false
    nonisolated let usesNativeLocalUsage = false
    private var generation: UInt64 = 1
    private var pricing: UInt64 = 1
    private var syncRequests: [LocalUsageSyncRequest] = []
    private var queries: [LocalUsageQueryRequest] = []
    private var releases: [String] = []
    private var capturedRecentEvents: [String: Int64] = [:]
    private var pauseObservers: [CheckedContinuation<Void, Never>] = []
    private var releaseObservers: [(Int, CheckedContinuation<Void, Never>)] = []
    private var pauseNextWindow = false
    private var pausedWindow: CheckedContinuation<Void, Never>?
    private var pauseNext = false
    private var paused: CheckedContinuation<Void, Never>?
    private var failNext = false
    private var recentEventMs: Int64 = 0

    func pauseNextSync() { pauseNext = true }
    func isPaused() -> Bool { paused != nil }
    func resumeSync() { paused?.resume(); paused = nil }
    func waitForPausedSync() async {
        if paused != nil { return }
        await withCheckedContinuation { pauseObservers.append($0) }
    }
    func pauseNextWindowQuery() { pauseNextWindow = true }
    func isWindowPaused() -> Bool { pausedWindow != nil }
    func resumeWindowQuery() { pausedWindow?.resume(); pausedWindow = nil }
    func waitForReleaseCount(_ count: Int) async {
        if releases.count >= count { return }
        await withCheckedContinuation { releaseObservers.append((count, $0)) }
    }
    func failNextSync() { failNext = true }
    func appendRecentEvent(at milliseconds: Int64) { recentEventMs = milliseconds; generation += 1 }
    func history() -> ([LocalUsageSyncRequest], [LocalUsageQueryRequest], [String]) {
        (syncRequests, queries, releases)
    }
    func localUsageSync(_ request: LocalUsageSyncRequest) async throws -> LocalUsageSyncResult {
        syncRequests.append(request)
        // Capture at entry, before a blocked source read. An event appended
        // while this is paused belongs only to the post-request verification.
        let capturedGeneration = generation
        let capturedEvent = recentEventMs
        if pauseNext {
            pauseNext = false
            await withCheckedContinuation { continuation in
                paused = continuation
                for observer in pauseObservers { observer.resume() }
                pauseObservers.removeAll()
            }
        }
        if failNext { failNext = false; throw Failure.sourceUnavailable }
        if request.refreshPricing { pricing += 1 }
        let token = "fixture:\(syncRequests.count):\(capturedGeneration)"
        capturedRecentEvents[token] = capturedEvent
        return LocalUsageSyncResult(
            leaseToken: token, rootGeneration: 1, priceGeneration: String(pricing), verifiedAtMs: 0,
            scopes: request.scopes.map {
                LocalUsageScopeSnapshot(id: $0.id, sourceGeneration: capturedGeneration,
                                        quotaSnapshotToken: $0.kind == "quota" ? token + ":quota:" + $0.id : nil)
            }, watchManifest: LocalSourceWatchManifest(roots: [], ignoredFileURLs: []), changed: true)
    }
    func localUsageQuery(_ request: LocalUsageQueryRequest) async throws -> LocalUsageValue {
        queries.append(request)
        guard let capturedEvent = capturedRecentEvents[request.leaseToken] else { throw Failure.sourceUnavailable }
        switch request.kind {
        case "rate":
            let now = request.nowMs ?? 0
            return .rate(capturedEvent > 0 && capturedEvent <= now && now - capturedEvent < 60_000 ? 120 : 0)
        case "trace": return .trace([])
        case "graph": return .graph(DemoData.payload(for: request.year))
        case "models": return .models(DemoData.modelReport(for: request.year))
        case "hourly": return .hourly(DemoData.hourlyReport(for: request.year, clients: request.clients))
        case "agents": return .agents(DemoData.agentsReport(for: request.year, clients: request.clients))
        default: throw Failure.unexpectedLegacyCall
        }
    }
    func releaseLocalUsage(_ token: String) async {
        releases.append(token); capturedRecentEvents[token] = nil
        for (count, observer) in releaseObservers where releases.count >= count { observer.resume() }
        releaseObservers.removeAll { releases.count >= $0.0 }
    }
    func windowUsage(_ request: WindowUsageRequest) async throws -> WindowUsageBatch {
        guard let token = request.snapshotToken else { throw Failure.unexpectedLegacyCall }
        let capturedGeneration = generation
        if pauseNextWindow {
            pauseNextWindow = false
            await withCheckedContinuation { pausedWindow = $0 }
        }
        return WindowUsageBatch(
            snapshotToken: token, sourceGeneration: capturedGeneration, rootGeneration: 1,
            coverage: .init(fromMs: request.fromMs, untilMs: request.untilMs))
    }
    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload { throw Failure.unexpectedLegacyCall }
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload { throw Failure.unexpectedLegacyCall }
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport { throw Failure.unexpectedLegacyCall }
    func hourlyReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> HourlyReport { throw Failure.unexpectedLegacyCall }
    func agentsReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> AgentsReport { throw Failure.unexpectedLegacyCall }
    func agentUsage() async throws -> AgentUsagePayload { throw Failure.unexpectedLegacyCall }
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] { throw Failure.unexpectedLegacyCall }
    func tokensPerMin() async throws -> Double { throw Failure.unexpectedLegacyCall }
}

@MainActor
extension LocalUsageScheduler {
    static func selfTest(_ expect: @escaping (Bool, String) -> Void) async {
        final class Clock {
            var monotonic: TimeInterval = 0
            var wall = Date(timeIntervalSince1970: 1_800_000_000)
            func advance(_ seconds: TimeInterval) { monotonic += seconds; wall.addTimeInterval(seconds) }
        }
        let clock = Clock()
        let source = SchedulerFixtureSource()
        let scheduler = LocalUsageScheduler(
            source: source, observesSources: false, automaticallySchedules: false,
            monotonicNow: { clock.monotonic }, wallNow: { clock.wall })
        let first = scheduler.openSession()
        let second = scheduler.openSession()
        let rate = Query(kind: "rate")
        var firstRates: [Double] = []
        var secondRates: [Double] = []
        await source.appendRecentEvent(at: Int64(clock.wall.timeIntervalSince1970 * 1000))
        scheduler.update(first, queries: [rate], active: true) { _, result in
            if case .success(.rate(let value)) = result { firstRates.append(value) }
        }
        scheduler.update(second, queries: [rate], active: true) { _, result in
            if case .success(.rate(let value)) = result { secondRates.append(value) }
        }
        await scheduler.runDueWork()
        var history = await source.history()
        expect(history.0.count == 1 && history.1.count == 1 && firstRates == [120] && secondRates == [120],
               "scheduler exact demand union shares ingestion and one compact query")
        expect(history.0[0].scopes.count == 1 && history.0[0].scopes[0].clients == nil
               && !history.0[0].refreshPricing, "recent-only demand has all-client authority and no pricing transport")
        clock.advance(10)
        await scheduler.runDueWork()
        history = await source.history()
        expect(history.0.count == 1 && history.1.count == 2,
               "ten-second visible expiry publication is a pure query, not ingestion")
        clock.advance(20)
        await scheduler.runDueWork()
        history = await source.history()
        expect(history.0.count == 2 && !history.0[1].verifyTopology && !history.0[1].force,
               "active thirty-second fallback verifies known inputs without rediscovery")
        scheduler.sourcesChanged(requiresTopology: false)
        let beforeDebounce = scheduler.syncCount
        clock.advance(0.1)
        await scheduler.runDueWork()
        expect(scheduler.syncCount == beforeDebounce, "dirty hint does not bypass debounce")
        clock.advance(0.2)
        await scheduler.runDueWork()
        expect(scheduler.syncCount == beforeDebounce + 1, "dirty deadline drains a hint once")
        clock.advance(30)
        await scheduler.runDueWork()
        expect(firstRates.last == 0 && secondRates.last == 0,
               "pure moving-window publication expires a previously positive rate to real zero")

        await source.appendRecentEvent(at: Int64(clock.wall.timeIntervalSince1970 * 1000))
        scheduler.sourcesChanged(requiresTopology: false)
        clock.advance(1)
        await scheduler.runDueWork()
        expect(firstRates.last == 120, "blocked-source clock fixture begins with a confirmed positive rate")
        await source.pauseNextSync()
        clock.advance(30)
        let blockedSource = Task { await scheduler.runDueWork() }
        await source.waitForPausedSync()
        let blockedSyncs = scheduler.syncCount
        clock.advance(31)
        await scheduler.runClockWork()
        let stillBlocked = await source.isPaused()
        expect(stillBlocked && scheduler.syncCount == blockedSyncs && firstRates.last == 0 && secondRates.last == 0,
               "a blocked mounted-source verification cannot prevent pure captured-rate expiration")
        await source.resumeSync()
        await blockedSource.value

        scheduler.release(first)
        scheduler.release(second)
        let closedSyncs = scheduler.syncCount
        let closedQueries = scheduler.queryCount
        clock.advance(1000)
        await scheduler.runDueWork()
        expect(scheduler.syncCount == closedSyncs && scheduler.queryCount == closedQueries,
               "last consumer close makes source and aggregate work quiescent")
        scheduler.update(first, queries: [rate], active: true) { _, _ in
            expect(false, "released session must never be resurrected")
        }
        await scheduler.runDueWork()
        expect(scheduler.syncCount == closedSyncs, "late old update cannot recreate demand")

        let visible = scheduler.openSession()
        let graph = Query(kind: "graph", year: "2026")
        let hourly = Query(kind: "hourly", year: "2026", clients: ["claude"])
        let otherHourly = Query(kind: "hourly", year: "2026", clients: ["codex"])
        scheduler.update(visible, queries: [graph, hourly, otherHourly], active: true) { _, _ in }
        await scheduler.runDueWork()
        history = await source.history()
        let exact = history.0.last!.scopes.filter { $0.kind == "report" }
        expect(exact.count == 3 && exact.contains(where: { $0.clients == ["claude"] })
               && exact.contains(where: { $0.clients == ["codex"] }) && exact.contains(where: { $0.clients == nil }),
               "mixed hourly questions capture distinct producing scopes before aggregation")
        let model = Query(kind: "models", year: "2026")
        scheduler.update(visible, queries: [graph, model], active: true) { _, _ in }
        await scheduler.runDueWork()
        scheduler.update(visible, queries: [graph], active: true) { _, _ in }
        await scheduler.runDueWork()
        let hiddenModelQueries = (await source.history()).1.filter { $0.kind == "models" }.count
        clock.advance(30)
        await scheduler.runDueWork()
        expect((await source.history()).1.filter { $0.kind == "models" }.count == hiddenModelQueries,
               "leaving a model lens removes refresh demand rather than retaining a sticky wanted flag")

        await source.pauseNextSync()
        scheduler.sourcesChanged(requiresTopology: true)
        clock.advance(1)
        let oldWork = Task { await scheduler.runDueWork() }
        await source.waitForPausedSync()
        expect(await source.isPaused(), "forced-refresh fixture reaches a genuinely blocked older verification")
        var forcedCompleted = false
        let manual = Task {
            defer { forcedCompleted = true }
            return try await scheduler.acquire(graph, session: visible, force: true)
        }
        while scheduler.pendingAcquisitions == 0, !forcedCompleted { await Task.yield() }
        expect(!forcedCompleted, "older in-flight verification cannot complete a newer forced waiter")
        await source.appendRecentEvent(at: Int64(clock.wall.timeIntervalSince1970 * 1000))
        await source.resumeSync()
        await oldWork.value
        let beforeForce = scheduler.syncCount
        await scheduler.runDueWork()
        let forcedValue = try? await manual.value
        history = await source.history()
        expect(forcedValue != nil && scheduler.syncCount == beforeForce + 1 && history.0.last?.force == true,
               "manual refresh queued behind older work starts its own post-request full verification")

        await source.pauseNextSync()
        scheduler.sourcesChanged(requiresTopology: true)
        clock.advance(1)
        let closingWork = Task { await scheduler.runDueWork() }
        await source.waitForPausedSync()
        scheduler.release(visible)
        let reopened = scheduler.openSession()
        var reopenedPublications = 0
        scheduler.update(reopened, queries: [graph], active: true) { _, result in
            if case .success = result { reopenedPublications += 1 }
        }
        await source.resumeSync()
        await closingWork.value
        expect(reopenedPublications == 0, "late closed-owner verification cannot publish into a reopened session")
        await scheduler.runDueWork()
        expect(reopenedPublications == 1, "replacement native session remains live after late old completion")

        await source.failNextSync()
        scheduler.sourcesChanged(requiresTopology: false)
        clock.advance(1)
        await scheduler.runDueWork()
        expect(scheduler.lastError != nil && reopenedPublications == 1,
               "failed verification keeps last-good output and records a distinct error")
        clock.advance(10)
        await scheduler.runDueWork()
        expect(scheduler.lastError == nil, "successful retry clears source error without a fabricated zero")
        scheduler.fullVerificationInterval = 100_000
        clock.advance(3600)
        await scheduler.runDueWork()
        history = await source.history()
        expect(history.0.last?.refreshPricing == true && history.0.last?.force == false,
               "price expiry requests pricing independently from forced source ingestion")
        let beforeClockSync = scheduler.syncCount
        let beforeClockQuery = scheduler.queryCount
        clock.wall.addTimeInterval(86_400)
        scheduler.clockChanged()
        await scheduler.runDueWork()
        expect(scheduler.syncCount == beforeClockSync && scheduler.queryCount > beforeClockQuery,
               "midnight/timezone rebucketing invalidates graph aggregates without source synchronization")
        scheduler.stop()

        let closedClock = Clock()
        let closedSource = SchedulerFixtureSource()
        let closedScheduler = LocalUsageScheduler(
            source: closedSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { closedClock.monotonic }, wallNow: { closedClock.wall })
        closedScheduler.fullVerificationInterval = 100_000
        let tray = closedScheduler.openSession()
        closedScheduler.update(tray, queries: [graph], active: false) { _, _ in }
        await closedScheduler.runDueWork()
        closedClock.advance(299)
        await closedScheduler.runDueWork()
        expect(closedScheduler.syncCount == 1, "closed graph demand does not inherit an active thirty-second scan clock")
        closedClock.advance(1)
        await closedScheduler.runDueWork()
        let closedHistory = await closedSource.history()
        expect(closedScheduler.syncCount == 2 && closedHistory.0.last?.verifyTopology == true,
               "closed graph fallback verifies authoritative topology within five minutes")
        for interval: TimeInterval in [60, 300, 900, 1800, 3600] {
            closedScheduler.fullVerificationInterval = interval
            // First establish the new interval's authoritative starting point.
            let request = Task { try await closedScheduler.acquire(graph, session: tray, force: true) }
            while closedScheduler.pendingAcquisitions == 0 { await Task.yield() }
            await closedScheduler.runDueWork()
            _ = try? await request.value
            closedClock.advance(interval - 1)
            await closedScheduler.runDueWork()
            let beforeFull = closedScheduler.syncCount
            closedClock.advance(1)
            await closedScheduler.runDueWork()
            let forceHistory = await closedSource.history()
            expect(closedScheduler.syncCount == beforeFull + 1 && forceHistory.0.last?.force == true,
                   "configured full-verification interval \(Int(interval)) seconds preserves its force deadline")
        }
        closedScheduler.rootsChanged()
        await closedScheduler.runDueWork()
        expect((await closedSource.history()).0.last?.previousLeaseToken == nil,
               "a changed installed root registry reissues capture without a mismatched native lease")

        let quota = quotaScope(clientId: "claude", accountKey: nil)
        closedScheduler.setQuotaScopes(tray, [quota])
        await closedScheduler.runDueWork()
        let pinnedToken = try? await closedScheduler.quotaSnapshot(clientId: "claude", accountKey: nil, session: tray)
        if let pinnedToken {
            await closedSource.waitForReleaseCount(closedScheduler.syncCount - 1)
            let request = WindowUsageRequest(accountKey: nil, fromMs: 0, untilMs: 100, snapshotToken: pinnedToken)
            await closedSource.pauseNextWindowQuery()
            let query = Task { try await closedScheduler.windowUsage(request, session: tray) }
            while !(await closedSource.isWindowPaused()) { await Task.yield() }
            let priorReleases = (await closedSource.history()).2.count
            await closedSource.appendRecentEvent(at: 1)
            closedScheduler.sourcesChanged(requiresTopology: false)
            closedClock.advance(1)
            await closedScheduler.runDueWork()
            expect((await closedSource.history()).2.count == priorReleases,
                   "rotating native captures keeps an in-flight same-token quota chain alive")
            await closedSource.resumeWindowQuery()
            let stale = try? await query.value
            expect(stale == nil && !closedScheduler.isCurrentQuotaSnapshot(pinnedToken, session: tray),
                   "an older compact quota generation cannot publish after source supersession")
            closedScheduler.finishQuotaSnapshot(pinnedToken)
            await closedSource.waitForReleaseCount(priorReleases + 1)
            expect((await closedSource.history()).2.count == priorReleases + 1,
                   "finishing the final quota pin releases its retired native lease")
        } else { expect(false, "quota fixture acquires a real scheduler-owned compact token") }
        closedScheduler.stop()
        let modelClock = Clock()
        let modelSource = SchedulerFixtureSource()
        let modelScheduler = LocalUsageScheduler(
            source: modelSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { modelClock.monotonic }, wallNow: { modelClock.wall })
        let modelSession = modelScheduler.openSession()
        let visibleModel = DashboardModel(
            source: modelSource, scheduler: modelScheduler, localSession: modelSession,
            initialYear: "2026", buildIdentity: nil)
        visibleModel.configureLocalDemand(view: .daily, clients: ["claude"])
        await modelScheduler.runDueWork()
        expect((await modelSource.history()).1.allSatisfy { $0.kind != "models" && $0.kind != "hourly" },
               "collapsed Daily renders graph-owned turns without hidden model/hourly queries")
        let oldRowOwner = UUID(), replacementRowOwner = UUID()
        visibleModel.setModelRowsExpanded(true, owner: oldRowOwner)
        await modelScheduler.runDueWork()
        expect(visibleModel.modelReport != nil, "an actually expanded row acquires its captured model colours")
        visibleModel.setModelRowsExpanded(true, owner: replacementRowOwner)
        visibleModel.setModelRowsExpanded(false, owner: oldRowOwner)
        expect(visibleModel.modelReport != nil, "late old-row disappearance cannot close a replacement row's model demand")
        visibleModel.setModelRowsExpanded(false, owner: replacementRowOwner)
        expect(visibleModel.modelReport == nil, "collapsing the last model consumer releases its view-only report")
        let modelQueries = (await modelSource.history()).1.filter { $0.kind == "models" }.count
        modelClock.advance(30)
        await modelScheduler.runDueWork()
        expect((await modelSource.history()).1.filter { $0.kind == "models" }.count == modelQueries,
               "source verification does not resurrect a collapsed model consumer")
        visibleModel.releaseLocalDemand()
        modelScheduler.stop()
        LocalSourceHints.selfTest(expect)
    }
}
