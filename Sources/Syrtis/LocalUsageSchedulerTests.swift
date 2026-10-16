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
    private var graphGenerations: [String: UInt64] = [:]
    private var distinctGraphs = false
    private var pauseObservers: [CheckedContinuation<Void, Never>] = []
    private var releaseObservers: [(Int, CheckedContinuation<Void, Never>)] = []
    private var pauseNextWindow = false
    private var pausedWindow: CheckedContinuation<Void, Never>?
    private var pauseNext = false
    private var paused: CheckedContinuation<Void, Never>?
    private var pauseNextQueryFlag = false
    private var pausedQuery: CheckedContinuation<Void, Never>?
    private var queryPauseObservers: [CheckedContinuation<Void, Never>] = []
    private var pausedQueryKind: String?
    private var pausedQueryKinds: Set<String> = []
    private var failNext = false
    private var failQueryKinds: Set<String> = []
    private var invalidateNextSync = false
    private var recentEventMs: Int64 = 0
    private var lastScopeIDs: [String]?
    private var lastToken: String?
    private var lastGeneration: UInt64?
    private var lastPrice: String?
    private var lastSyncResult: LocalUsageSyncResult?
    private var reportDay = Format.todayKey()

    func setReportDay(_ day: String) { reportDay = day; generation += 1 }

    func capturedQuotaToken(for scope: LocalUsageScope) -> String? {
        lastSyncResult?.scopes.first(where: { $0.id == scope.id })?.quotaSnapshotToken
    }

    func useDistinctGraphs() { distinctGraphs = true }
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
    func pauseNextQuery() { pauseNextQueryFlag = true }
    func pauseNextQuery(kind: String) { pauseNextQueryFlag = true; pausedQueryKinds.insert(kind) }
    func isQueryPaused() -> Bool { pausedQuery != nil }
    func pausedQueryKindForTesting() -> String? { pausedQueryKind }
    func resumeQuery() { pausedQuery?.resume(); pausedQuery = nil; pausedQueryKind = nil; pausedQueryKinds.removeAll() }
    func waitForPausedQuery() async {
        if pausedQuery != nil { return }
        await withCheckedContinuation { queryPauseObservers.append($0) }
    }
    func failNextSync() { failNext = true }
    func failNextQuery(kind: String) { failQueryKinds.insert(kind) }
    func invalidateBeforeNextSync() { invalidateNextSync = true }
    func evictCurrentLease() {
        if let lastToken { capturedRecentEvents[lastToken] = nil }
    }
    func waitForReleaseCount(_ count: Int) async {
        if releases.count >= count { return }
        await withCheckedContinuation { releaseObservers.append((count, $0)) }
    }
    func appendRecentEvent(at milliseconds: Int64) { recentEventMs = milliseconds; generation += 1 }
    func history() -> ([LocalUsageSyncRequest], [LocalUsageQueryRequest], [String]) {
        (syncRequests, queries, releases)
    }
    func localUsageSync(_ request: LocalUsageSyncRequest) async throws -> LocalUsageSyncResult {
        syncRequests.append(request)
        if invalidateNextSync {
            invalidateNextSync = false
            evictCurrentLease()
        }
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
        // Native reuses its lease token only when every input is unchanged:
        // the caller chains the prior token, the prior token is still live
        // (never released), scopes match exactly, and neither the source
        // generation nor the price generation moved.
        let scopeIDs = request.scopes.map(\.id).sorted()
        let token: String
        if scopeIDs == lastScopeIDs, let reused = lastToken,
           request.previousLeaseToken == reused, capturedRecentEvents[reused] != nil,
           capturedGeneration == lastGeneration, String(pricing) == lastPrice {
            token = reused
        } else {
            token = "fixture:\(syncRequests.count):\(capturedGeneration)"
            capturedRecentEvents[token] = capturedEvent
            graphGenerations[token] = capturedGeneration
            lastScopeIDs = scopeIDs
            lastToken = token
            lastGeneration = capturedGeneration
            lastPrice = String(pricing)
        }
        let result = LocalUsageSyncResult(
            leaseToken: token, rootGeneration: 1, priceGeneration: String(pricing), verifiedAtMs: 0,
            scopes: request.scopes.map {
                LocalUsageScopeSnapshot(id: $0.id, sourceGeneration: capturedGeneration,
                                        quotaSnapshotToken: $0.kind == "quota" ? token + ":quota:" + $0.id : nil)
            }, watchManifest: LocalSourceWatchManifest(roots: [], ignoredFileURLs: []), changed: true)
        lastSyncResult = result
        return result
    }
    func localUsageQuery(_ request: LocalUsageQueryRequest) async throws -> LocalUsageValue {
        queries.append(request)
        // Capture the fail flag BEFORE any await: the pause below suspends,
        // and a later query must not consume another query's failure.
        let shouldFail = failQueryKinds.contains(request.kind)
        if shouldFail { failQueryKinds.remove(request.kind) }
        if pauseNextQueryFlag, pausedQueryKinds.isEmpty || pausedQueryKinds.contains(request.kind) {
            pauseNextQueryFlag = false
            pausedQueryKinds.removeAll()
            pausedQueryKind = request.kind
            await withCheckedContinuation { continuation in
                pausedQuery = continuation
                for observer in queryPauseObservers { observer.resume() }
                queryPauseObservers.removeAll()
            }
        }
        if shouldFail { throw Failure.sourceUnavailable }
        guard let capturedEvent = capturedRecentEvents[request.leaseToken] else { throw Failure.sourceUnavailable }
        switch request.kind {
        case "rate":
            let now = request.nowMs ?? 0
            return .rate(capturedEvent > 0 && capturedEvent <= now && now - capturedEvent < 60_000 ? 120 : 0)
        case "trace": return .trace([])
        case "graph":
            let payload = DemoData.payload(for: request.year, today: reportDay)
            guard distinctGraphs else { return .graph(payload) }
            var object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(payload)) as! [String: Any]
            var meta = object["meta"] as! [String: Any]
            meta["generatedAt"] = "fixture-generation-\(graphGenerations[request.leaseToken] ?? 0)"
            object["meta"] = meta
            return .graph(try JSONDecoder().decode(UsagePayload.self, from: JSONSerialization.data(withJSONObject: object)))
        case "models": return .models(DemoData.modelReport(for: request.year, today: reportDay))
        case "hourly": return .hourly(DemoData.hourlyReport(for: request.year, clients: request.clients, today: reportDay))
        case "agents": return .agents(DemoData.agentsReport(for: request.year, clients: request.clients, today: reportDay))
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
        // An app-lifetime owner prepares display facts without a dashboard.
        DashboardModel.invalidateScanDerivedCaches()
        let displayClock = Clock()
        let displaySource = SchedulerFixtureSource()
        await displaySource.useDistinctGraphs()
        let displayScheduler = LocalUsageScheduler(
            source: displaySource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { displayClock.monotonic }, wallNow: { displayClock.wall })
        let background = displayScheduler.openSession()
        let displayGraph = Query(kind: "graph", year: "2026")
        let displayModels = Query(kind: "models", year: "2026")
        displayScheduler.update(background, queries: [displayGraph, displayModels], active: false) { _, _ in }
        await displayScheduler.runDueWork()
        let originalCapture = displayScheduler.displaySnapshot(for: displayGraph)?.capturedAt
        displayClock.advance(301)
        func opened(_ year: String? = "2026") -> DashboardModel {
            DashboardModel(source: displaySource, scheduler: displayScheduler,
                           localSession: displayScheduler.openSession(), initialYear: year, buildIdentity: nil)
        }
        let prepared = opened()
        expect({
            if case .ready = prepared.phase { return true }; return false
        }() && prepared.payload?.meta.generatedAt == "fixture-generation-1"
               && prepared.modelReport != nil && prepared.restoredSnapshot?.savedAt == originalCapture,
               "background display seeds actual graph and matching models synchronously with original capture age")
        let wrongYear = opened("2025")
        expect({
            if case .loading = wrongYear.phase { return true }; return false
        }() && wrongYear.payload == nil, "prepared display requires the exact selected year")
        wrongYear.releaseLocalDemand()
        prepared.releaseLocalDemand()

        await displaySource.pauseNextSync()
        let pendingRefresh = Task { await displayScheduler.runDueWork() }
        await displaySource.waitForPausedSync()
        let pendingDisplay = opened()
        expect(pendingDisplay.payload != nil && pendingDisplay.localPending
               && pendingDisplay.restoredSnapshot?.savedAt == originalCapture,
               "opening during blocked background verification keeps last-good display and pending state")
        await displaySource.failNextSync()
        await displaySource.resumeSync()
        await pendingRefresh.value
        let failedDisplay = opened()
        expect(failedDisplay.payload != nil && failedDisplay.localError != nil
               && failedDisplay.restoredSnapshot?.failed == true,
               "failed background verification retains ready display with an honest error")
        pendingDisplay.releaseLocalDemand()
        failedDisplay.releaseLocalDemand()

        displayClock.advance(301)
        await displaySource.appendRecentEvent(at: Int64(displayClock.wall.timeIntervalSince1970 * 1000))
        await displaySource.failNextQuery(kind: "models")
        await displayScheduler.runDueWork()
        let recovered = opened()
        expect(recovered.payload?.meta.generatedAt == "fixture-generation-2"
               && recovered.restoredSnapshot?.savedAt != originalCapture,
               "next open uses the latest successful background graph generation")
        expect(recovered.modelReport == nil,
               "old model generation cannot be labeled as the newly prepared graph")
        recovered.releaseLocalDemand()
        displayClock.advance(301)
        await displayScheduler.runDueWork()
        let complete = opened()
        expect(complete.payload != nil && complete.modelReport != nil && complete.localError == nil,
               "background recovery restores a matching graph and model pair")
        complete.releaseLocalDemand()
        await displaySource.pauseNextSync()
        var forceCompleted = false
        let forcedDisplay = Task {
            defer { forceCompleted = true }
            return try await displayScheduler.acquire(displayGraph, session: background, force: true)
        }
        while displayScheduler.pendingAcquisitions == 0, !forceCompleted { await Task.yield() }
        let forcedWork = Task { await displayScheduler.runDueWork() }
        await displaySource.waitForPausedSync()
        expect(!forceCompleted && displayScheduler.displaySnapshot(for: displayGraph) != nil,
               "last-good display never satisfies a forced freshness barrier")
        await displaySource.resumeSync()
        await forcedWork.value
        _ = try? await forcedDisplay.value
        let previousRootsProvider = ClaudeExtraRoots.appliedProvider
        ClaudeExtraRoots.appliedProvider = { "{\"claude\":[\"/synthetic-new-root\"]}" }
        expect(displayScheduler.displaySnapshot(for: displayGraph) == nil,
               "installed root change rejects old display before the coalesced scheduler notification")
        ClaudeExtraRoots.appliedProvider = previousRootsProvider
        displayScheduler.rootsChanged()
        DashboardModel.invalidateScanDerivedCaches()
        let otherRoot = opened()
        expect(otherRoot.payload == nil && otherRoot.modelReport == nil,
               "root invalidation cannot leak a previous prepared display")
        otherRoot.releaseLocalDemand()
        displayScheduler.release(background)
        displayScheduler.stop()
        // Cadence belongs to the exact recent question, not to global activity.
        for mode in 0..<4 {
            let cadenceClock = Clock()
            // An inactive wall bucket crosses at +3s, before its actual +10s cadence.
            cadenceClock.wall.addTimeInterval(7)
            let cadenceSource = SchedulerFixtureSource()
            let cadenceScheduler = LocalUsageScheduler(
                source: cadenceSource, observesSources: false, automaticallySchedules: false,
                monotonicNow: { cadenceClock.monotonic }, wallNow: { cadenceClock.wall })
            let recentSession = cadenceScheduler.openSession()
            let cadenceRate = Query(kind: "rate")
            var cadenceRates: [Double] = []
            var clockTicks = 0
            await cadenceSource.appendRecentEvent(
                at: Int64(cadenceClock.wall.timeIntervalSince1970 * 1000) - 56_000)
            cadenceScheduler.update(recentSession, queries: [cadenceRate], active: mode == 0) { _, result in
                if case .success(.rate(let value)) = result { cadenceRates.append(value) }
            }
            cadenceScheduler.setClock(recentSession) { clockTicks += 1 }
            if mode >= 2 {
                let otherSession = cadenceScheduler.openSession()
                // Even another visible recent scope must not accelerate this one.
                let other = mode == 2 ? Query(kind: "graph", year: "2026")
                    : Query(kind: "rate", clients: ["codex"])
                cadenceScheduler.update(otherSession, queries: [other], active: true) { _, _ in }
            }
            await cadenceScheduler.runDueWork()
            let initialSyncs = cadenceScheduler.syncCount
            let initialTicks = clockTicks
            let initialQueries = (await cadenceSource.history()).1.filter { $0.scopeId == cadenceRate.scope.id }.count
            cadenceClock.advance(5)
            await cadenceScheduler.runDueWork()
            let atFive = (await cadenceSource.history()).1.filter { $0.scopeId == cadenceRate.scope.id }.count
            expect(cadenceRates == (mode == 0 ? [120, 0] : [120])
                   && atFive == initialQueries + (mode == 0 ? 1 : 0),
                   "active recent expires at five seconds; inactive recent ignores unrelated active demand mode \(mode)")
            if mode < 3 {
                expect(clockTicks == initialTicks + (mode == 0 ? 1 : 0),
                       "clock callbacks follow demanded recent cadence mode \(mode)")
            }
            cadenceClock.advance(5)
            await cadenceScheduler.runDueWork()
            let atTen = (await cadenceSource.history()).1.filter { $0.scopeId == cadenceRate.scope.id }.count
            expect(cadenceRates.last == 0 && atTen == initialQueries + (mode == 0 ? 2 : 1)
                   && cadenceScheduler.syncCount == initialSyncs,
                   "inactive recent queries at ten seconds without idle ingestion mode \(mode)")
            cadenceClock.advance(1)
            await cadenceSource.appendRecentEvent(
                at: Int64(cadenceClock.wall.timeIntervalSince1970 * 1000))
            cadenceScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///cadence"])
            cadenceClock.advance(0.11)
            await cadenceScheduler.runDueWork()
            expect(cadenceRates.last == 120 && cadenceScheduler.syncCount == initialSyncs + 1,
                   "new source generation publishes immediately between clock deadlines mode \(mode)")
            cadenceScheduler.stop()
        }
        // Changing activity during await invalidates both old values and errors.
        for initiallyActive in [false, true] {
            for fails in [false, true] {
                let transitionClock = Clock()
                transitionClock.wall.addTimeInterval(7)
                let transitionSource = SchedulerFixtureSource()
                let transitionScheduler = LocalUsageScheduler(
                    source: transitionSource, observesSources: false, automaticallySchedules: false,
                    monotonicNow: { transitionClock.monotonic }, wallNow: { transitionClock.wall })
                let transitionSession = transitionScheduler.openSession()
                let transitionRate = Query(kind: "rate")
                var publications: [Double] = []
                var failures = 0
                var ticks = 0
                let receive: @MainActor (Query, Result<LocalUsageValue, Error>) -> Void = { _, result in
                    if case .success(.rate(let value)) = result { publications.append(value) }
                    if case .failure = result { failures += 1 }
                }
                await transitionSource.appendRecentEvent(
                    at: Int64(transitionClock.wall.timeIntervalSince1970 * 1000) - 56_000)
                transitionScheduler.update(transitionSession, queries: [transitionRate],
                                           active: initiallyActive, onValue: receive)
                transitionScheduler.setClock(transitionSession) { ticks += 1 }
                await transitionScheduler.runDueWork()
                let initialTicks = ticks
                transitionClock.advance(initiallyActive ? 5 : 10)
                if fails { await transitionSource.failNextQuery(kind: "rate") }
                await transitionSource.pauseNextQuery(kind: "rate")
                let pausedTurn = Task { await transitionScheduler.runClockWork() }
                await transitionSource.waitForPausedQuery()
                transitionScheduler.update(transitionSession, queries: [transitionRate],
                                           active: !initiallyActive, onValue: receive)
                await transitionSource.resumeQuery()
                await pausedTurn.value
                expect(publications == [120] && failures == 0 && transitionScheduler.lastError == nil,
                       "activity transition rejects stale recent value/error active \(initiallyActive) failure \(fails)")
                expect(ticks == initialTicks + (initiallyActive ? 0 : 1),
                       "paused-query completion ticks using current demand cadence active \(initiallyActive)")
                await transitionScheduler.runDueWork()
                expect(publications == [120, 0] && failures == 0 && transitionScheduler.syncCount == 1,
                       "activity transition refolds current recent clock without rereading sources")
                transitionClock.advance(5)
                await transitionScheduler.runDueWork()
                expect(publications.count == (initiallyActive ? 2 : 3),
                       "transition starts a full new exact cadence rather than inheriting a wall bucket")
                if initiallyActive {
                    transitionClock.advance(5)
                    await transitionScheduler.runDueWork()
                    expect(publications == [120, 0, 0],
                           "deactivated recent query waits ten seconds from transition")
                }
                transitionScheduler.update(transitionSession, queries: [], active: false, onValue: receive)
                let emptySyncs = transitionScheduler.syncCount
                let emptyQueries = transitionScheduler.queryCount
                transitionClock.advance(1000)
                transitionScheduler.sourcesChanged(requiresTopology: true)
                await transitionScheduler.runDueWork()
                expect(transitionScheduler.syncCount == emptySyncs && transitionScheduler.queryCount == emptyQueries,
                       "minimized zero-demand session does not run source or query work")
                transitionScheduler.stop()
            }
        }
        // The immediately-due sentinel is valid even without any local queries.
        let callbackClock = Clock()
        let callbackSource = SchedulerFixtureSource()
        let callbackScheduler = LocalUsageScheduler(
            source: callbackSource, observesSources: false, automaticallySchedules: true,
            monotonicNow: { callbackClock.monotonic }, wallNow: { callbackClock.wall })
        let callbackSession = callbackScheduler.openSession()
        var callbackTicks = 0
        callbackScheduler.setClock(callbackSession) { callbackTicks += 1 }
        expect(callbackTicks == 1 && callbackScheduler.syncCount == 0 && callbackScheduler.queryCount == 0,
               "first clock-only owner receives its immediate callback without local source work")
        await callbackScheduler.runClockWork()
        await callbackSource.appendRecentEvent(
            at: Int64(callbackClock.wall.timeIntervalSince1970 * 1000))
        callbackScheduler.update(callbackSession, queries: [Query(kind: "rate")], active: true) { _, _ in }
        await callbackScheduler.runDueWork()
        await callbackSource.pauseNextQuery(kind: "rate")
        callbackClock.advance(5)
        callbackScheduler.clockChanged()
        await callbackSource.waitForPausedQuery()
        let ticksBeforeNotification = callbackTicks
        let syncsBeforeNotification = callbackScheduler.syncCount
        callbackScheduler.clockChanged()
        expect(callbackTicks == ticksBeforeNotification + 1
               && callbackScheduler.syncCount == syncsBeforeNotification,
               "clock notification immediately reaches consumers while the recent query is paused")
        await callbackSource.resumeQuery()
        await callbackScheduler.runDueWork()
        callbackScheduler.stop()
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
        clock.advance(5)
        await scheduler.runDueWork()
        history = await source.history()
        expect(history.0.count == 1 && history.1.count == 2,
               "five-second visible expiry publication is a pure query, not ingestion")
        clock.advance(25)
        await scheduler.runDueWork()
        history = await source.history()
        expect(history.0.count == 2 && !history.0[1].verifyTopology && !history.0[1].force,
               "active thirty-second fallback verifies known inputs without rediscovery")
        scheduler.sourcesChanged(requiresTopology: false)
        let beforeDebounce = scheduler.syncCount
        clock.advance(0.05)
        await scheduler.runDueWork()
        expect(scheduler.syncCount == beforeDebounce, "dirty hint does not bypass debounce")
        clock.advance(0.06)
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
        clock.advance(31)
        await scheduler.runDueWork()
        // First paint stages its exact graph scope before the slow union.
        let paintSource = SchedulerFixtureSource()
        let paintClock = Clock()
        let paintScheduler = LocalUsageScheduler(
            source: paintSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { paintClock.monotonic }, wallNow: { paintClock.wall })
        let paintSession = paintScheduler.openSession()
        let paintGraph = Query(kind: "graph", year: "2026")
        let paintRecent = Query(kind: "rate")
        var paintGraphs = 0
        paintScheduler.update(paintSession, queries: [paintGraph, paintRecent], active: true) { query, result in
            if query == paintGraph, case .success = result { paintGraphs += 1 }
        }
        // The staged path queries the graph before runDueWork returns, so the
        // pause lands on the first query, after the exact-scope sync completed.
        await paintSource.pauseNextQuery()
        let paintWork = Task { await paintScheduler.runDueWork() }
        await paintSource.waitForPausedQuery()
        let stagedScopes = (await paintSource.history()).0.last?.scopes ?? []
        expect(stagedScopes.count == 1 && stagedScopes.first?.id == paintGraph.scope.id && paintGraphs == 0,
               "first graph demand syncs its exact scope before the slow recent union")
        await paintSource.resumeQuery()
        await paintWork.value
        expect(paintGraphs >= 1, "staged graph publishes before the paused recent union runs")
        // Unrelated quota demand queues behind the captured graph publication.
        let paintQuota = quotaScope(clientId: "claude", accountKey: nil)
        paintScheduler.setQuotaScopes(paintSession, [paintQuota])
        await paintSource.pauseNextSync()
        paintClock.advance(31)
        let driftWork = Task { await paintScheduler.runDueWork() }
        await paintSource.waitForPausedSync()
        // The staged graph already published above; the paused union is the
        // only later publication, so the firstpaint count stays put while the
        // unrelated union is still blocked.
        expect(paintGraphs >= 1, "unrelated quota/session drift cannot discard a scope-valid first graph")
        await paintSource.resumeSync()
        await driftWork.value
        expect((await paintSource.history()).0.last.map { $0.scopes.contains(paintQuota) } == true,
               "secondary quota demand progresses after first paint")
        // Continuous dirty hints never starve the captured publication either.
        paintScheduler.sourcesChanged(requiresTopology: false)
        paintScheduler.sourcesChanged(requiresTopology: false)
        paintClock.advance(1)
        await paintScheduler.runDueWork()
        expect(paintGraphs >= 1, "continuous dirty hints queue rather than drop first paint")
        // Removing an unrelated hourly demand mid-flight cannot discard the
        // already-captured staged graph; exact admission only tracks the
        // staged scope's own demanders.
        let spareHourly = Query(kind: "hourly", year: "2026", clients: ["codex"])
        paintScheduler.update(paintSession, queries: [paintGraph, paintRecent, spareHourly], active: true) { _, _ in }
        paintClock.advance(31)
        await paintSource.pauseNextQuery()
        let spareWork = Task { await paintScheduler.runDueWork() }
        await paintSource.waitForPausedQuery()
        paintScheduler.update(paintSession, queries: [paintGraph, paintRecent], active: true) { _, _ in }
        await paintSource.resumeQuery()
        await spareWork.value
        expect(paintGraphs >= 1, "unrelated removed demand keeps the staged graph publication")
        // A graph demanded after an existing recent capture still stages
        // before the slow union: its side capture covers only the graph scope.
        let warmSource = SchedulerFixtureSource()
        let warmClock = Clock()
        let warmScheduler = LocalUsageScheduler(
            source: warmSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { warmClock.monotonic }, wallNow: { warmClock.wall })
        let warmSession = warmScheduler.openSession()
        var warmGraphs = 0
        warmScheduler.update(warmSession, queries: [paintRecent], active: true) { _, _ in }
        await warmScheduler.runDueWork()
        warmScheduler.update(warmSession, queries: [paintGraph, paintRecent], active: true) { query, result in
            if query == paintGraph, case .success = result { warmGraphs += 1 }
        }
        await warmSource.pauseNextQuery()
        let warmWork = Task { await warmScheduler.runDueWork() }
        await warmSource.waitForPausedQuery()
        let warmStaged = (await warmSource.history()).0.last?.scopes ?? []
        expect(warmStaged.count == 1 && warmStaged.first?.id == paintGraph.scope.id && warmGraphs == 0,
               "graph demanded after a live recent capture still stages its exact scope first")
        await warmSource.resumeQuery()
        await warmWork.value
        expect(warmGraphs >= 1, "warm-capture staged graph publishes before the union")
        warmScheduler.stop()
        // Native unchanged-token reuse, exact scenario: seed REPORT-only with
        // graph(year 2026) and retain its installed token. A new graph(year
        // 2025, same report scope) plus recent demand stages a side capture
        // over the already-live report scope, which borrows the seeded token.
        let reuseSource = SchedulerFixtureSource()
        let reuseClock = Clock()
        let reuseScheduler = LocalUsageScheduler(
            source: reuseSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { reuseClock.monotonic }, wallNow: { reuseClock.wall })
        let reuseSession = reuseScheduler.openSession()
        let graph2026 = Query(kind: "graph", year: "2026")
        let graph2025 = Query(kind: "graph", year: "2025")
        reuseScheduler.update(reuseSession, queries: [graph2026], active: true) { _, _ in }
        await reuseScheduler.runDueWork()
        let seededQueries = (await reuseSource.history()).1
        let seededToken = seededQueries.last?.leaseToken ?? ""
        expect(!seededToken.isEmpty, "report-only seed installs a live lease")
        var reuseGraphs = 0
        reuseScheduler.update(reuseSession, queries: [graph2025, paintRecent], active: true) { query, result in
            if query == graph2025, case .success = result { reuseGraphs += 1 }
        }
        reuseClock.advance(31)
        await reuseSource.pauseNextQuery()
        let reuseWork = Task { await reuseScheduler.runDueWork() }
        await reuseSource.waitForPausedQuery()
        // The side capture covers only the already-live report scope, so the
        // paused side query must borrow the seeded token.
        let sideToken = (await reuseSource.history()).1.last?.leaseToken ?? ""
        expect(sideToken == seededToken && !seededToken.isEmpty,
               "side capture over the live report scope borrows the seeded token")
        // Arm the union behind a sync pause; while it is blocked, the side
        // query's completion must still deliver the graph without releasing
        // the borrowed token.
        await reuseSource.pauseNextSync()
        await reuseSource.resumeQuery()
        await reuseSource.waitForPausedSync()
        expect(reuseGraphs >= 1, "borrowed-token side capture publishes first paint before the union")
        expect(!(await reuseSource.history()).2.contains(seededToken),
               "borrowed live token is never released by side staging")
        // The union runs once resumed; native treats the expired
        // previousLeaseToken as a fresh capture, so no rejection occurs.
        await reuseSource.resumeSync()
        await reuseWork.value
        expect(reuseGraphs >= 1, "union completion keeps the staged first paint")
        // Root change while borrowed side state is warm: supersession must
        // hold the old token until in-flight use ends and release exactly once.
        reuseScheduler.rootsChanged()
        reuseClock.advance(1)
        await reuseScheduler.runDueWork()
        let reuseReleases = (await reuseSource.history()).2
        let seededReleases = reuseReleases.filter({ $0 == seededToken }).count
        expect(seededReleases <= 1,
               "borrowed-token root supersession never double-releases")
        reuseScheduler.stop()
        // Stale query failures die like stale successes: owner A's paused
        // query throws only after A closed and B reopened the same demand, so
        // B must see no failure and the fresh capture must still publish.
        let staleSource = SchedulerFixtureSource()
        let staleClock = Clock()
        let staleScheduler = LocalUsageScheduler(
            source: staleSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { staleClock.monotonic }, wallNow: { staleClock.wall })
        let ownerA = staleScheduler.openSession()
        var failuresA = 0
        var successesB = 0
        staleScheduler.update(ownerA, queries: [graph], active: true) { _, result in
            if case .failure = result { failuresA += 1 }
        }
        await staleScheduler.runDueWork()
        // New generation (not just the clock) forces a real refresh miss, so
        // the paused query is genuinely reached.
        await staleSource.appendRecentEvent(at: Int64(staleClock.wall.timeIntervalSince1970 * 1000))
        staleClock.advance(31)
        await staleSource.pauseNextQuery()
        await staleSource.failNextQuery(kind: "graph")
        let staleWork = Task { await staleScheduler.runDueWork() }
        await staleSource.waitForPausedQuery()
        staleScheduler.release(ownerA)
        let ownerB = staleScheduler.openSession()
        staleScheduler.update(ownerB, queries: [graph], active: true) { _, result in
            if case .success = result { successesB += 1 }
            if case .failure = result { failuresA += 100 }
        }
        await staleSource.resumeQuery()
        await staleWork.value
        await staleScheduler.runDueWork()
        expect(failuresA == 0, "a closed owner's query failure never reaches any callback")
        expect(successesB == 1, "replacement demand still publishes after a stale failure drops")
        staleScheduler.stop()
        // Keep the source worker free while an ordinary query is suspended.
        // A new quota scope then proves replacement installation before the
        // old query fails, without observing private scheduler state.
        let leaseFailSource = SchedulerFixtureSource()
        let leaseFailClock = Clock()
        let leaseFailScheduler = LocalUsageScheduler(
            source: leaseFailSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { leaseFailClock.monotonic }, wallNow: { leaseFailClock.wall })
        let liveSession = leaseFailScheduler.openSession()
        var liveFailures = 0
        leaseFailScheduler.update(liveSession, queries: [graph], active: true) { _, result in
            if case .failure = result { liveFailures += 1 }
        }
        let leaseFailQuota = quotaScope(clientId: "claude", accountKey: nil)
        leaseFailScheduler.setQuotaScopes(liveSession, [leaseFailQuota])
        await leaseFailScheduler.runDueWork()
        leaseFailClock.wall.addTimeInterval(86_400)
        leaseFailScheduler.clockChanged()
        await leaseFailSource.pauseNextQuery()
        await leaseFailSource.failNextQuery(kind: "graph")
        let leaseFailWork = Task { await leaseFailScheduler.runClockWork() }
        await leaseFailSource.waitForPausedQuery()
        let oldQueryToken = (await leaseFailSource.history()).1.last!.leaseToken
        await leaseFailSource.appendRecentEvent(at: Int64(leaseFailClock.wall.timeIntervalSince1970 * 1000))
        let extraKey = "fixture-extra"
        let extraQuota = quotaScope(clientId: "claude", accountKey: extraKey)
        leaseFailScheduler.setQuotaScopes(liveSession, [leaseFailQuota, extraQuota])
        let replacementWork = Task { await leaseFailScheduler.runDueWork() }
        var capturedQuota: String?
        while capturedQuota == nil {
            if let token = await leaseFailSource.capturedQuotaToken(for: extraQuota),
               leaseFailScheduler.isCurrentQuotaSnapshot(token, session: liveSession) {
                capturedQuota = token
            } else {
                await Task.yield()
            }
        }
        let newQuotaToken = try? await leaseFailScheduler.quotaSnapshot(
            clientId: "claude", accountKey: extraKey, session: liveSession)
        let heldReleases = (await leaseFailSource.history()).2
        expect(newQuotaToken.map {
            leaseFailScheduler.isCurrentQuotaSnapshot($0, session: liveSession)
        } == true && !heldReleases.contains(oldQueryToken),
               "replacement quota is usable while the old ordinary query remains pinned")
        await leaseFailSource.resumeQuery()
        await leaseFailWork.value
        await replacementWork.value
        expect(liveFailures == 0 && leaseFailScheduler.lastError == nil,
               "an old query failure cannot invalidate the replacement lease")
        if let newQuotaToken { leaseFailScheduler.finishQuotaSnapshot(newQuotaToken) }
        leaseFailScheduler.stop()
        // Side-batch provenance: two graphs (nil year tray + 2026 dashboard)
        // plus recent stage one capture; pausing the first query, then a root
        // change, must reject BOTH old-root publishes, and replacing the
        // second graph's owner mid-await must reject its stale twin too.
        let batchSource = SchedulerFixtureSource()
        let batchClock = Clock()
        let batchScheduler = LocalUsageScheduler(
            source: batchSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { batchClock.monotonic }, wallNow: { batchClock.wall })
        let batchOwner = batchScheduler.openSession()
        let trayGraph = Query(kind: "graph")
        let dashGraph = Query(kind: "graph", year: "2026")
        var batchTray = 0
        var batchDash = 0
        batchScheduler.update(batchOwner, queries: [trayGraph, dashGraph, paintRecent], active: true) { query, result in
            if query == trayGraph, case .success = result { batchTray += 1 }
            if query == dashGraph, case .success = result { batchDash += 1 }
        }
        await batchSource.pauseNextQuery()
        let batchWork = Task { await batchScheduler.runDueWork() }
        await batchSource.waitForPausedQuery()
        batchScheduler.rootsChanged()
        // Hold the union behind a sync pause so the observation window shows
        // ONLY old-batch output: any publication here is stale leakage.
        await batchSource.pauseNextSync()
        await batchSource.resumeQuery()
        await batchSource.waitForPausedSync()
        expect(batchTray == 0 && batchDash == 0,
               "a root change during the first staged query rejects the whole old batch")
        await batchSource.resumeSync()
        await batchWork.value
        expect(batchTray == 1 && batchDash == 1,
               "the fresh capture republishes both graphs after a root change")
        batchScheduler.stop()
        // Replace the owner of whichever graph has not started yet.
        let swapSource = SchedulerFixtureSource()
        let swapClock = Clock()
        let swapScheduler = LocalUsageScheduler(
            source: swapSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { swapClock.monotonic }, wallNow: { swapClock.wall })
        let swapOwners = [trayGraph: swapScheduler.openSession(), dashGraph: swapScheduler.openSession()]
        for (query, owner) in swapOwners {
            swapScheduler.update(owner, queries: [query], active: true) { _, _ in }
        }
        let recentOwner = swapScheduler.openSession()
        swapScheduler.update(recentOwner, queries: [paintRecent], active: true) { _, _ in }
        await swapSource.pauseNextQuery()
        let swapWork = Task { await swapScheduler.runDueWork() }
        await swapSource.waitForPausedQuery()
        let pausedYear = (await swapSource.history()).1.last!.year
        let replacedQuery = pausedYear == trayGraph.year ? dashGraph : trayGraph
        swapScheduler.release(swapOwners[replacedQuery]!)
        let replacementOwner = swapScheduler.openSession()
        var replacementPublications = 0
        swapScheduler.update(replacementOwner, queries: [replacedQuery], active: true) { _, result in
            if case .success = result { replacementPublications += 1 }
        }
        await swapSource.pauseNextSync()
        await swapSource.resumeQuery()
        await swapSource.waitForPausedSync()
        expect(replacementPublications == 0,
               "a replacement owner cannot inherit another staged query's capture")
        await swapSource.resumeSync()
        await swapWork.value
        expect(replacementPublications == 1,
               "the replacement owner receives its freshly installed capture")
        swapScheduler.stop()
        let leaseSource = SchedulerFixtureSource()
        let leaseClock = Clock()
        let leaseScheduler = LocalUsageScheduler(
            source: leaseSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { leaseClock.monotonic }, wallNow: { leaseClock.wall })
        let leaseSession = leaseScheduler.openSession()
        let leaseGraph = Query(kind: "graph", year: "2026")
        var leasePublications = 0
        leaseScheduler.update(leaseSession, queries: [leaseGraph], active: true) { _, result in
            if case .success = result { leasePublications += 1 }
        }
        await leaseScheduler.runDueWork()
        // rootsChanged clears the snapshot, so the next worker turn stages a
        // side capture and pauses on its query; only then arm the pause.
        leaseScheduler.rootsChanged()
        await leaseSource.pauseNextQuery()
        let leaseWork = Task { await leaseScheduler.runDueWork() }
        await leaseSource.waitForPausedQuery()
        let liveToken = (await leaseSource.history()).1.last?.leaseToken ?? ""
        // Retire while the ordinary query is in flight: the side pin holds
        // the live token, so the drain must spawn no release of THAT token.
        // (The pre-roots initial lease was legally released already.)
        leaseScheduler.rootsChanged()
        let releasesBeforeResume = (await leaseSource.history()).2
        expect(!liveToken.isEmpty && !releasesBeforeResume.contains(liveToken),
               "an in-flight ordinary query keeps its native lease alive across supersession")
        await leaseSource.resumeQuery()
        await leaseWork.value
        await leaseSource.waitForReleaseCount(releasesBeforeResume.count + 1)
        let releases = (await leaseSource.history()).2
        expect(releases.contains(liveToken) && !liveToken.isEmpty,
               "superseded lease releases once its ordinary query completes")
        expect(leasePublications == 1, "the stale completion drops; only the fresh capture republishes")
        leaseScheduler.stop()
        paintScheduler.stop()

        await source.pauseNextSync()
        scheduler.sourcesChanged(requiresTopology: true)
        clock.advance(31)
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
        clock.advance(31)
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
        clock.advance(31)
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
        // Single-graph demand skips the side capture (staged scopes equal the
        // union), so first paint is exactly one sync with no extra scan.
        expect(closedScheduler.syncCount == 1, "closed graph demand does not inherit an active thirty-second scan clock")
        closedClock.advance(1)
        await closedScheduler.runDueWork()
        let closedHistory = await closedSource.history()
        expect(closedHistory.0.last?.verifyTopology == true,
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
        closedClock.advance(1)
        await closedScheduler.runDueWork()
        expect((await closedSource.history()).0.last?.previousLeaseToken == nil,
               "a changed installed root registry reissues capture without a mismatched native lease")

        let quota = quotaScope(clientId: "claude", accountKey: nil)
        // Exact old-token barrier: capture the installed sync lease before
        // quota demand lands; token reuse means sync counts no longer track
        // released generations one-to-one.
        let oldInstalledToken = (await closedSource.history()).1.last?.leaseToken
        closedScheduler.setQuotaScopes(tray, [quota])
        await closedScheduler.runDueWork()
        if let oldInstalledToken {
            // Exact barrier: the superseded installed lease must release
            // before the quota pin proceeds, regardless of sync counts.
            while !(await closedSource.history()).2.contains(oldInstalledToken) { await Task.yield() }
        }
        let pinnedToken = try? await closedScheduler.quotaSnapshot(clientId: "claude", accountKey: nil, session: tray)
        if let pinnedToken {
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

        // Reopen must draw the last exact lens before any new source capture,
        // retain it through a failed refresh, then replace it on recovery.
        func displayedTotal(_ model: DashboardModel, view: AppView) -> Int64? {
            switch view {
            case .models: return model.modelReport?.entries.reduce(0) { $0 + $1.total }
            case .hourly: return model.hourlyReport(for: ["claude"])?.entries.reduce(0) { $0 + $1.total }
            case .agents: return model.agents?.entries.reduce(0) { $0 + $1.total }
            default: return nil
            }
        }
        for view in [AppView.models, .hourly, .agents] {
            DashboardModel.invalidateScanDerivedCaches()
            let reopenClock = Clock()
            let reopenSource = SchedulerFixtureSource()
            await reopenSource.setReportDay("2026-01-02")
            let reopenScheduler = LocalUsageScheduler(
                source: reopenSource, observesSources: false, automaticallySchedules: false,
                monotonicNow: { reopenClock.monotonic }, wallNow: { reopenClock.wall })
            let seed = DashboardModel(
                cachesSnapshot: true, source: reopenSource, scheduler: reopenScheduler,
                localSession: reopenScheduler.openSession(), initialYear: "2026", buildIdentity: nil)
            seed.configureLocalDemand(view: view, clients: ["claude"])
            await reopenScheduler.runDueWork()
            let lastTotal = displayedTotal(seed, view: view)
            expect(lastTotal != nil, "\(view) seed has a renderable report")
            seed.releaseLocalDemand()
            let rebuilt = DashboardModel(
                cachesSnapshot: true, source: reopenSource, scheduler: reopenScheduler,
                localSession: reopenScheduler.openSession(), initialYear: "2026", buildIdentity: nil)
            expect(displayedTotal(rebuilt, view: view) == lastTotal && lastTotal != nil,
                   "\(view) reopen draws its last report before starting a task")
            await reopenSource.setReportDay("2026-01-03")
            await reopenSource.pauseNextSync()
            await reopenSource.failNextQuery(kind: view.rawValue)
            rebuilt.configureLocalDemand(view: view, clients: ["claude"])
            let reopenTurn = Task { await reopenScheduler.runDueWork() }
            await reopenSource.waitForPausedSync()
            expect(rebuilt.localPending && displayedTotal(rebuilt, view: view) == lastTotal,
                   "\(view) pending source capture does not blank the last-good lens")
            await reopenSource.resumeSync()
            await reopenTurn.value
            expect(rebuilt.localError != nil && displayedTotal(rebuilt, view: view) == lastTotal,
                   "\(view) failed refresh retains its prior totals")
            reopenClock.advance(11)
            await reopenScheduler.runDueWork()
            let nextTotal = displayedTotal(rebuilt, view: view)
            expect(rebuilt.localError == nil && nextTotal != nil && nextTotal != lastTotal,
                   "\(view) recovery replaces stale totals with the new source result")
            rebuilt.releaseLocalDemand()
            let wrongYear = DashboardModel(
                source: reopenSource, initialYear: "2025", buildIdentity: nil)
            expect(wrongYear.payload == nil && displayedTotal(wrongYear, view: view) == nil,
                   "\(view) reopen cannot reuse another year's data")
            let next = DashboardModel(
                cachesSnapshot: true, source: reopenSource, scheduler: reopenScheduler,
                localSession: reopenScheduler.openSession(), initialYear: "2026", buildIdentity: nil)
            if view == .hourly {
                expect(next.hourlyReport(for: ["codex"]) == nil,
                       "Hourly cannot show a cached Claude fold on a Codex tab")
            }
            if view == .agents {
                next.configureLocalDemand(view: view, clients: ["codex"])
                expect(next.agents == nil, "Agents discards a cached fold when the client scope changes")
            }
            next.releaseLocalDemand()
            let closedSyncs = reopenScheduler.syncCount
            reopenClock.advance(1000)
            await reopenScheduler.runDueWork()
            expect(reopenScheduler.syncCount == closedSyncs,
                   "\(view) retained display does not keep closed source work alive")
            DashboardModel.invalidateScanDerivedCaches()
            let changedRoots = DashboardModel(source: reopenSource, initialYear: "2026", buildIdentity: nil)
            expect(changedRoots.payload == nil && displayedTotal(changedRoots, view: view) == nil,
                   "\(view) changed roots invalidate the retained display")
            reopenScheduler.stop()
        }
        AttributedSeriesModel.resetForTesting()
        let seriesSource = SchedulerFixtureSource()
        let seriesScheduler = LocalUsageScheduler(
            source: seriesSource, observesSources: false, automaticallySchedules: false)
        let seriesSeed = AttributedSeriesModel(
            scheduler: seriesScheduler, localSession: seriesScheduler.openSession())
        seriesSeed.setDemand(wanted: true, confirmed: [])
        await seriesScheduler.runDueWork()
        let lastPoints = seriesSeed.points
        seriesSeed.releaseDemand()
        let seriesReopen = AttributedSeriesModel(
            scheduler: seriesScheduler, localSession: seriesScheduler.openSession())
        expect(seriesReopen.points == lastPoints && lastPoints != nil,
               "Quota trend restores its previous points before source work starts")
        await seriesSource.pauseNextSync()
        seriesReopen.setDemand(wanted: true, confirmed: [])
        let seriesTurn = Task { await seriesScheduler.runDueWork() }
        await seriesSource.waitForPausedSync()
        expect(seriesReopen.points == lastPoints,
               "Quota trend remains visible during a blocked reopen capture")
        AttributedSeriesModel.invalidateRowCache()
        let changedSeries = AttributedSeriesModel(
            scheduler: seriesScheduler, localSession: seriesScheduler.openSession())
        expect(changedSeries.points == nil, "Quota trend cannot restore rows from changed roots")
        changedSeries.releaseDemand()
        await seriesSource.resumeSync()
        await seriesTurn.value
        seriesReopen.releaseDemand()
        AttributedSeriesModel.invalidateTimeZoneProvenance()
        let changedZone = AttributedSeriesModel(
            scheduler: seriesScheduler, localSession: seriesScheduler.openSession())
        expect(changedZone.points == nil, "Quota trend cannot restore unverified timezone buckets")
        changedZone.releaseDemand()
        seriesScheduler.stop()
        AttributedSeriesModel.resetForTesting()
        // The scheduler itself retains only demanded reports; display-only
        // reopen snapshots do not retain native captures. Contracted demand
        // leaves installed coverage alone; reexpansion refolds from it, and a
        // valid demanded aggregate replays without any DB refold on demand
        // moves or irrelevant clock ticks.
        let coverageClock = Clock()
        let coverageSource = SchedulerFixtureSource()
        let coverageScheduler = LocalUsageScheduler(
            source: coverageSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { coverageClock.monotonic }, wallNow: { coverageClock.wall })
        let coverageSession = coverageScheduler.openSession()
        let overviewGraph = Query(kind: "graph", year: "2026")
        let hourlyClaude = Query(kind: "hourly", year: "2026", clients: ["claude"])
        let recentRate = Query(kind: "rate")
        var hourlyPublications = 0
        var initialRate = -1.0
        await coverageSource.appendRecentEvent(
            at: Int64(coverageClock.wall.timeIntervalSince1970 * 1000) - 59_500)
        coverageScheduler.update(coverageSession, queries: [overviewGraph, hourlyClaude, recentRate], active: true) { query, result in
            if query == hourlyClaude, case .success = result { hourlyPublications += 1 }
            if query == recentRate, case .success(.rate(let value)) = result { initialRate = value }
        }
        await coverageScheduler.runDueWork()
        expect(hourlyPublications == 1 && initialRate == 120, "union demand publishes hourly once and a live rate")
        let unionSyncs = coverageScheduler.syncCount
        let unionQueries = coverageScheduler.queryCount
        // Contract to Overview alone: the lens-away hourly is dropped, and the
        // installed capture is not recollected for the contraction itself.
        coverageScheduler.update(coverageSession, queries: [overviewGraph], active: true) { _, _ in }
        coverageClock.advance(1)
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.syncCount == unionSyncs && coverageScheduler.queryCount == unionQueries,
               "contracted demand drops the lens-away report without native recollect or refold")
        // Reopen the lens: no synchronous replay exists anymore, so the
        // historical aggregate refolds once from the installed capture.
        hourlyPublications = 0
        let reopenBaseQueries = coverageScheduler.queryCount
        coverageScheduler.update(coverageSession, queries: [overviewGraph, hourlyClaude], active: true) { query, result in
            if query == hourlyClaude, case .success = result { hourlyPublications += 1 }
        }
        expect(hourlyPublications == 0, "reopened lens-away report has no retained replay")
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.syncCount == unionSyncs && coverageScheduler.queryCount == reopenBaseQueries + 1
               && hourlyPublications == 1,
               "reopened lens refolds once from the installed capture without recollecting")
        // Demanded valid aggregates reuse: contracting and reopening plus an
        // irrelevant clock tick performs no further DB refold.
        coverageScheduler.update(coverageSession, queries: [overviewGraph], active: true) { _, _ in }
        coverageClock.advance(1)
        await coverageScheduler.runDueWork()
        let afterContractQueries = coverageScheduler.queryCount
        hourlyPublications = 0
        coverageScheduler.update(coverageSession, queries: [overviewGraph, hourlyClaude], active: true) { query, result in
            if query == hourlyClaude, case .success = result { hourlyPublications += 1 }
        }
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.queryCount == afterContractQueries + 1 && hourlyPublications == 1,
               "second reopen refolds the dropped report once more without recollecting")
        let settledQueries = coverageScheduler.queryCount
        coverageScheduler.clockChanged()
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.queryCount == settledQueries && coverageScheduler.syncCount == unionSyncs,
               "irrelevant clock tick reuses the valid demanded aggregate without DB refold")
        // Recency invariant: a dropped recent is never retained, so re-adding
        // it refolds exactly once from coverage without recollecting.
        var ratePublications = 0
        var lastRate = -1.0
        coverageScheduler.update(coverageSession, queries: [overviewGraph, hourlyClaude, recentRate], active: true) { query, result in
            if query == recentRate, case .success(.rate(let value)) = result { ratePublications += 1; lastRate = value }
        }
        expect(ratePublications == 0,
               "re-added recent has no retained replay")
        await coverageScheduler.runDueWork()
        expect(ratePublications == 1 && lastRate == 0 && coverageScheduler.syncCount == unionSyncs
               && coverageScheduler.queryCount == settledQueries + 1,
               "re-added recent refolds the expired zero from installed coverage without resync")
        // Forced post-request verification stays authoritative despite coverage.
        let forcedHourly = Task { try await coverageScheduler.acquire(hourlyClaude, session: coverageSession, force: true) }
        while coverageScheduler.pendingAcquisitions == 0 { await Task.yield() }
        await coverageScheduler.runDueWork()
        let coverageForcedValue = try? await forcedHourly.value
        let forceHistory = await coverageSource.history()
        expect(coverageForcedValue != nil && coverageScheduler.syncCount == unionSyncs + 1 && forceHistory.0.last?.force == true,
               "forced waiter recollects despite installed coverage")
        // Dirty hints stay authoritative despite coverage.
        coverageScheduler.sourcesChanged(requiresTopology: false)
        coverageClock.advance(1)
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.syncCount == unionSyncs + 2, "dirty hint reverifies despite installed coverage")
        // A new source generation refolds covered demand.
        await coverageSource.appendRecentEvent(at: Int64(coverageClock.wall.timeIntervalSince1970 * 1000))
        coverageScheduler.sourcesChanged(requiresTopology: false)
        coverageClock.advance(1)
        let beforeGenerationQueries = coverageScheduler.queryCount
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.syncCount == unionSyncs + 3 && coverageScheduler.queryCount > beforeGenerationQueries,
               "new generation recollects and refolds covered demand")
        // The next normally due sync carries current demand only.
        coverageScheduler.update(coverageSession, queries: [overviewGraph, hourlyClaude], active: true) { _, _ in }
        coverageClock.advance(30)
        await coverageScheduler.runDueWork()
        let dueScopes = (await coverageSource.history()).0.last?.scopes.map(\.id).sorted()
        expect(dueScopes == [overviewGraph.scope.id, hourlyClaude.scope.id].sorted(),
               "normally due sync reverifies current demand only, without undemanded scopes")
        // The contracted sync evicted the recent scope, so reexpansion must
        // recollect; demanded graph and hourly keep identities and skip refold
        // while only the evicted recent refolds.
        let beforeEvictedRefold = hourlyPublications
        let beforeReexpandSyncs = coverageScheduler.syncCount
        let beforeReexpandQueries = coverageScheduler.queryCount
        var evictedRate = 0
        var evictedLastRate = -1.0
        coverageScheduler.update(coverageSession, queries: [overviewGraph, hourlyClaude, recentRate], active: true) { query, result in
            if query == hourlyClaude, case .success = result { hourlyPublications += 1 }
            if query == recentRate, case .success(.rate(let value)) = result { evictedRate += 1; evictedLastRate = value }
        }
        await coverageScheduler.runDueWork()
        expect(coverageScheduler.syncCount == beforeReexpandSyncs + 1
               && coverageScheduler.queryCount == beforeReexpandQueries + 1
               && hourlyPublications == beforeEvictedRefold
               && evictedRate == 1 && evictedLastRate == 120,
               "reexpansion past a contracted sync recollects once and refolds the live recent")
        // Owner replacement publishes fresh; final close frees everything.
        coverageScheduler.release(coverageSession)
        let replacementSession = coverageScheduler.openSession()
        var replacementGraphs = 0
        coverageScheduler.update(replacementSession, queries: [overviewGraph], active: true) { query, result in
            if query == overviewGraph, case .success = result { replacementGraphs += 1 }
        }
        await coverageScheduler.runDueWork()
        expect(replacementGraphs == 1, "replacement owner publishes its fresh capture")
        let coverageLiveToken = (await coverageSource.history()).1.last?.leaseToken ?? ""
        let beforeCloseSyncs = coverageScheduler.syncCount
        let beforeCloseQueries = coverageScheduler.queryCount
        coverageScheduler.release(replacementSession)
        if !coverageLiveToken.isEmpty {
            while !(await coverageSource.history()).2.contains(coverageLiveToken) { await Task.yield() }
        }
        coverageClock.advance(1000)
        await coverageScheduler.runDueWork()
        expect(replacementGraphs == 1 && coverageScheduler.syncCount == beforeCloseSyncs
               && coverageScheduler.queryCount == beforeCloseQueries,
               "final close releases leases and stays quiescent")
        coverageScheduler.stop()
        // A blocked slow history query cannot hold live updates: pause only
        // the graph query, force a genuine history miss via a day-bucket move
        // so the next turn reaches it, age the event past 60s, then run the
        // recent lane while history is genuinely paused. Exact real values:
        // the seed publishes 120, the recent lane expires 120 to a real 0
        // with trace while the graph stays blocked.
        let stalledSource = SchedulerFixtureSource()
        let stalledClock = Clock()
        let stalledScheduler = LocalUsageScheduler(
            source: stalledSource, observesSources: false, automaticallySchedules: true,
            monotonicNow: { stalledClock.monotonic }, wallNow: { stalledClock.wall })
        let stalledSession = stalledScheduler.openSession()
        let stalledRate = Query(kind: "rate")
        let stalledTrace = Query(kind: "trace", windowSecs: 600)
        let stalledGraph = Query(kind: "graph", year: "2026")
        var stalledRates: [Double] = []
        // Seed before demand lands so the first automatic sync ingests it.
        await stalledSource.appendRecentEvent(at: Int64(stalledClock.wall.timeIntervalSince1970 * 1000))
        stalledScheduler.update(stalledSession, queries: [stalledGraph, stalledRate, stalledTrace], active: true) { query, result in
            if query == stalledRate, case .success(.rate(let value)) = result { stalledRates.append(value) }
        }
        let stalledSeedDeadline = Date().addingTimeInterval(5)
        while Date() < stalledSeedDeadline {
            if stalledRates == [120] { break }
            await Task.yield()
        }
        expect(stalledRates == [120], "seed publishes the live positive rate")
        // Genuine history miss on the next automatic turn: arm the
        // kind-scoped graph pause, then move the day bucket (selective
        // clockChanged only evicts history on a real day/timezone move) and
        // age the event past the 60s live window so the automatic turn must
        // issue the slow history query and block on it. The recent lane
        // must still expire 120 to a real 0 with trace while it stays
        // paused: automatic kick coalesces the lanes, so no manual clock
        // lane can be awaited directly while history is blocked.
        await stalledSource.pauseNextQuery(kind: "graph")
        stalledClock.advance(61)
        stalledClock.wall.addTimeInterval(86_400)
        stalledScheduler.clockChanged()
        let expiryDeadline = Date().addingTimeInterval(5)
        while Date() < expiryDeadline {
            if stalledRates == [120, 0] { break }
            await Task.yield()
        }
        expect(stalledRates == [120, 0], "recent expiry publishes 0 while the slow history query stays blocked")
        let stalledStillPaused = await stalledSource.isQueryPaused()
        let stalledPausedKind = await stalledSource.pausedQueryKindForTesting()
        expect(stalledStillPaused && stalledPausedKind == "graph",
               "the paused query is genuinely the slow history lane")
        // SAME graph query is still paused: the automatic source turn must
        // ingest via the union only (no second staged graph on the source
        // worker) and the recent lane must republish the live 120. Both
        // guards expire finitely (finite failure on the unfixed lane split,
        // which holds recent behind the blocked history query).
        await stalledSource.appendRecentEvent(at: Int64(stalledClock.wall.timeIntervalSince1970 * 1000))
        stalledScheduler.sourcesChanged(requiresTopology: false)
        stalledClock.advance(1)
        var resyncObserved = false
        let resyncDeadline = Date().addingTimeInterval(5)
        while Date() < resyncDeadline {
            if stalledRates == [120, 0, 120] { resyncObserved = true; break }
            await Task.yield()
        }
        let resyncStillPaused = await stalledSource.isQueryPaused()
        expect(resyncStillPaused, "source resync completes while the slow history query stays blocked")
        expect(resyncObserved, "new-event resync observed before the history release (bounded guard)")
        await stalledSource.resumeQuery()
        await stalledScheduler.runDueWork()
        expect(stalledRates == [120, 0, 120],
               "blocked history still lets the live header expire 120 to 0 then resync to 120 on a new event")
        stalledScheduler.stop()
        // A late error from an expired recent bucket must not clear the
        // still-valid lease or replace the header's last good rate.
        let expiredErrorSource = SchedulerFixtureSource()
        let expiredErrorClock = Clock()
        let expiredErrorScheduler = LocalUsageScheduler(
            source: expiredErrorSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { expiredErrorClock.monotonic }, wallNow: { expiredErrorClock.wall })
        let expiredErrorSession = expiredErrorScheduler.openSession()
        let expiredErrorRate = Query(kind: "rate")
        var expiredErrorRates: [Double] = []
        var expiredErrorFailures = 0
        await expiredErrorSource.appendRecentEvent(
            at: Int64(expiredErrorClock.wall.timeIntervalSince1970 * 1000))
        expiredErrorScheduler.update(expiredErrorSession, queries: [expiredErrorRate], active: true) { _, result in
            if case .success(.rate(let value)) = result { expiredErrorRates.append(value) }
            if case .failure = result { expiredErrorFailures += 1 }
        }
        await expiredErrorScheduler.runDueWork()
        expiredErrorClock.advance(11)
        await expiredErrorSource.failNextQuery(kind: "rate")
        await expiredErrorSource.pauseNextQuery(kind: "rate")
        let expiredErrorTurn = Task { await expiredErrorScheduler.runClockWork() }
        await expiredErrorSource.waitForPausedQuery()
        expiredErrorClock.advance(11)
        expiredErrorScheduler.clockChanged()
        await expiredErrorSource.resumeQuery()
        await expiredErrorTurn.value
        expect(expiredErrorFailures == 0 && expiredErrorScheduler.lastError == nil && expiredErrorRates == [120],
               "expired recent-bucket error does not replace the header's last good rate")
        await expiredErrorScheduler.runDueWork()
        expect(expiredErrorRates == [120, 120] && expiredErrorFailures == 0,
               "current recent bucket remains readable after the expired query failure")
        expiredErrorScheduler.stop()
        // Native replacement can evict a scope before the awaiting sync
        // returns its lease. A failed old rate is owned by that capture.
        let replacementSource = SchedulerFixtureSource()
        let replacementClock = Clock()
        let replacementScheduler = LocalUsageScheduler(
            source: replacementSource, observesSources: false,
            monotonicNow: { replacementClock.monotonic }, wallNow: { replacementClock.wall })
        let replacementRateSession = replacementScheduler.openSession()
        let replacementModel = DashboardModel(
            cachesSnapshot: false, source: replacementSource,
            scheduler: replacementScheduler, localSession: replacementRateSession)
        await replacementSource.appendRecentEvent(
            at: Int64(replacementClock.wall.timeIntervalSince1970 * 1000))
        replacementModel.configureLocalDemand(view: .daily, clients: [], models: false)
        await replacementScheduler.runDueWork()
        expect(replacementModel.liveRate == 120, "header receives its first real rate")
        await replacementSource.invalidateBeforeNextSync()
        await replacementSource.pauseNextSync()
        replacementClock.advance(31)
        replacementScheduler.sourcesChanged(requiresTopology: false)
        replacementClock.advance(1)
        await replacementSource.waitForPausedSync()
        await replacementScheduler.runClockWork()
        let pendingQueryCount = replacementScheduler.queryCount
        for _ in 0..<20 { await Task.yield() }
        expect(replacementModel.localError == nil && replacementModel.liveRate == 120,
               "replacement-owned stale rate preserves last-good header without a false error")
        expect(replacementScheduler.queryCount == pendingQueryCount,
               "replacement-owned stale query waits for ingestion instead of spinning")
        await replacementSource.resumeSync()
        await replacementScheduler.runDueWork()
        expect(replacementModel.liveRate == 120 && replacementModel.localError == nil,
               "replacement capture restores header publication")
        // A genuinely evicted installed lease must report failure, wait for
        // the retry deadline, then synchronize without recycling that lease.
        await replacementSource.evictCurrentLease()
        replacementClock.advance(11)
        await replacementScheduler.runClockWork()
        expect(replacementModel.localError != nil && replacementModel.liveRate == 120,
               "current lease failure remains visible and keeps last-good rate")
        let failedQueryCount = replacementScheduler.queryCount
        await replacementScheduler.runClockWork()
        expect(replacementScheduler.queryCount == failedQueryCount,
               "invalid lease cannot be queried again before resynchronization")
        replacementClock.advance(11)
        await replacementScheduler.runDueWork()
        expect(replacementModel.liveRate == 120 && replacementModel.localError == nil,
               "evicted lease automatically recovers by synchronizing")
        let recoveryHistory = await replacementSource.history()
        expect(recoveryHistory.0.last?.previousLeaseToken == nil,
               "recovery does not offer the rejected lease for reuse")
        replacementModel.releaseLocalDemand()
        let reopenedSession = replacementScheduler.openSession()
        let reopenedModel = DashboardModel(
            cachesSnapshot: false, source: replacementSource,
            scheduler: replacementScheduler, localSession: reopenedSession)
        reopenedModel.configureLocalDemand(view: .daily, clients: [], models: false)
        await replacementScheduler.runDueWork()
        expect(reopenedModel.liveRate == 120 && reopenedModel.localError == nil,
               "reopened popover model gets a fresh first rate")
        await replacementSource.appendRecentEvent(
            at: Int64(replacementClock.wall.timeIntervalSince1970 * 1000))
        replacementScheduler.sourcesChanged(requiresTopology: false)
        replacementClock.advance(1)
        await replacementScheduler.runDueWork()
        expect(reopenedModel.liveRate == 120 && reopenedModel.localError == nil,
               "failed-capture fixture starts with fresh positive usage")
        await replacementSource.invalidateBeforeNextSync()
        await replacementSource.failNextSync()
        await replacementSource.pauseNextSync()
        replacementClock.advance(11)
        replacementScheduler.sourcesChanged(requiresTopology: false)
        replacementClock.advance(1)
        await replacementSource.waitForPausedSync()
        await replacementScheduler.runClockWork()
        expect(reopenedModel.localError == nil && reopenedModel.liveRate == 120,
               "old query failure waits for the replacement capture outcome")
        await replacementSource.resumeSync()
        await replacementScheduler.runDueWork()
        expect(reopenedModel.localError != nil && reopenedModel.liveRate == 120,
               "replacement ingestion failure is reported without inventing zero usage")
        replacementClock.advance(11)
        await replacementScheduler.runDueWork()
        expect(reopenedModel.localError == nil && reopenedModel.liveRate == 120,
               "failed replacement recovers the reopened header on the next synchronization")
        reopenedModel.releaseLocalDemand()
        replacementScheduler.stop()
        // A burst keeps its first deadline; named syncs cannot reset periodic verification.
        let burstClock = Clock()
        let burstSource = SchedulerFixtureSource()
        let burstScheduler = LocalUsageScheduler(
            source: burstSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { burstClock.monotonic }, wallNow: { burstClock.wall })
        let burstSession = burstScheduler.openSession()
        burstScheduler.update(burstSession, queries: [Query(kind: "rate")], active: true) { _, _ in }
        await burstScheduler.runDueWork()
        burstScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///first"])
        for path in ["file:///second", "file:///third"] {
            burstClock.advance(0.04)
            burstScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: [path])
            await burstScheduler.runDueWork()
        }
        expect(burstScheduler.syncCount == 1, "dirty burst does not bypass the first hint deadline")
        burstClock.advance(0.03)
        await burstScheduler.runDueWork()
        let drainedBurst = (await burstSource.history()).0.last
        expect(burstScheduler.syncCount == 2
               && drainedBurst?.changedFileURLs == ["file:///first", "file:///second", "file:///third"],
               "continuous hints drain at the first dirty deadline instead of resetting it")
        for second in [5.0, 10, 15, 20, 25] {
            burstClock.advance(second - burstClock.monotonic)
            burstScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///periodic"])
            burstClock.advance(0.11)
            await burstScheduler.runDueWork()
            expect((await burstSource.history()).0.last?.changedFileURLs == ["file:///periodic"],
                   "named sync remains targeted before the original thirty-second deadline")
        }
        burstClock.advance(30 - burstClock.monotonic)
        burstScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///deadline"])
        await burstScheduler.runDueWork()
        let periodicRequest = (await burstSource.history()).0.last
        expect(burstScheduler.syncCount == 8 && periodicRequest?.changedFileURLs == nil
               && periodicRequest?.force == false && periodicRequest?.verifyTopology == false,
               "authoritative thirty-second verification outranks a fresh hint and survives every named sync")
        // Retry combines consumed evidence with callbacks received during failure.
        await burstSource.pauseNextSync()
        await burstSource.failNextSync()
        burstScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///consumed"])
        burstClock.advance(0.11)
        let failedBurst = Task { await burstScheduler.runDueWork() }
        await burstSource.waitForPausedSync()
        burstScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///inflight"])
        await burstSource.resumeSync()
        await failedBurst.value
        burstClock.advance(10)
        await burstScheduler.runDueWork()
        expect((await burstSource.history()).0.last?.changedFileURLs == ["file:///consumed", "file:///inflight"],
               "failed capture retries consumed names together with in-flight callbacks")
        burstScheduler.stop()
        // Hint plumbing: bounded coalesced file evidence rides the plain
        // dirty sync, overflow/topology/periodic syncs verify authoritatively.
        let hintClock = Clock()
        let hintSource = SchedulerFixtureSource()
        let hintScheduler = LocalUsageScheduler(
            source: hintSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { hintClock.monotonic }, wallNow: { hintClock.wall })
        let hintSession = hintScheduler.openSession()
        let hintGraph = Query(kind: "graph", year: "2026")
        hintScheduler.update(hintSession, queries: [hintGraph], active: true) { _, _ in }
        await hintScheduler.runDueWork()
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///a"])
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///b"])
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        expect((await hintSource.history()).0.last?.changedFileURLs == ["file:///a", "file:///b"],
               "coalesced hints ride the plain dirty sync sorted")
        hintScheduler.sourcesChanged(requiresTopology: false,
                                     changedFileURLs: (0..<300).map { "file:///f\($0)" })
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///after"])
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        expect((await hintSource.history()).0.last?.changedFileURLs == nil,
               "overflowing hints stay dropped when later names arrive before the sync")
        hintScheduler.sourcesChanged(requiresTopology: false)
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///later"])
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        expect((await hintSource.history()).0.last?.changedFileURLs == nil,
               "unnamed changes stay authoritative beside later named callbacks")
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///c"])
        hintScheduler.sourcesChanged(requiresTopology: true)
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        let topologyRequest = (await hintSource.history()).0.last
        expect(topologyRequest?.verifyTopology == true && topologyRequest?.changedFileURLs == nil,
               "topology fallback drops named evidence")
        // Continuous hints never postpone the periodic full check.
        hintScheduler.fullVerificationInterval = 60
        hintClock.advance(59)
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///d"])
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        let fullRequest = (await hintSource.history()).0.last
        expect(fullRequest?.force == true && fullRequest?.changedFileURLs == nil,
               "periodic full verification survives continuous hints")
        // Failed hinted syncs retry the same evidence, not an empty sync.
        hintScheduler.fullVerificationInterval = 100_000
        await hintSource.failNextSync()
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///e"])
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        hintClock.advance(11)
        await hintScheduler.runDueWork()
        expect((await hintSource.history()).0.last?.changedFileURLs == ["file:///e"],
               "failed hinted sync requeues its evidence for the retry")
        // Late hints during an in-flight capture queue for the next sync.
        await hintSource.pauseNextSync()
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///f"])
        hintClock.advance(1)
        let hintedWork = Task { await hintScheduler.runDueWork() }
        await hintSource.waitForPausedSync()
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///g"])
        await hintSource.resumeSync()
        await hintedWork.value
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        expect((await hintSource.history()).0.last?.changedFileURLs == ["file:///g"],
               "late hints survive in-flight capture for the next sync")
        // Frequent named events must not starve the all-known-input deadline.
        hintClock.advance(10)
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///h"])
        await hintScheduler.runDueWork()
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        expect((await hintSource.history()).0.last?.changedFileURLs == ["file:///h"],
               "named events remain targeted before the known-input deadline")
        hintClock.advance(10)
        hintScheduler.sourcesChanged(requiresTopology: false, changedFileURLs: ["file:///i"])
        hintClock.advance(1)
        await hintScheduler.runDueWork()
        let knownRequest = (await hintSource.history()).0.last
        expect(knownRequest?.changedFileURLs == nil && knownRequest?.force == false,
               "continuous named events cannot postpone active all-known-input verification")
        hintScheduler.stop()
        // History order: graph before hourly/agents, models last.
        let orderClock = Clock()
        let orderSource = SchedulerFixtureSource()
        let orderScheduler = LocalUsageScheduler(
            source: orderSource, observesSources: false, automaticallySchedules: false,
            monotonicNow: { orderClock.monotonic }, wallNow: { orderClock.wall })
        let orderSession = orderScheduler.openSession()
        let orderGraph = Query(kind: "graph", year: "2026")
        let orderAgents = Query(kind: "agents", year: "2026", clients: ["claude"])
        let orderHourly = Query(kind: "hourly", year: "2026", clients: ["claude"])
        let orderModels = Query(kind: "models", year: "2026")
        orderScheduler.update(orderSession,
                              queries: [orderModels, orderAgents, orderHourly, orderGraph],
                              active: false) { _, _ in }
        await orderScheduler.runDueWork()
        let kinds = (await orderSource.history()).1.map(\.kind)
        let graphFirst = kinds.firstIndex(of: "graph") ?? .max
        expect(kinds.firstIndex(of: "graph") == graphFirst && graphFirst < (kinds.firstIndex(of: "agents") ?? .max)
               && graphFirst < (kinds.firstIndex(of: "hourly") ?? .max)
               && (kinds.firstIndex(of: "models") ?? .min) > (kinds.firstIndex(of: "agents") ?? .max)
               && (kinds.firstIndex(of: "models") ?? .min) > (kinds.firstIndex(of: "hourly") ?? .max),
               "history runs graph before agents/hourly and models last")
        orderScheduler.stop()
        LocalSourceHints.selfTest(expect)
    }
}
