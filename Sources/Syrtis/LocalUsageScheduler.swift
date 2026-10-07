import AppKit
import Foundation
import TokenBarCore

/// One app-owned source lane. The wall clock changes questions; the monotonic
/// clock schedules checks. Native queries never ingest or contact providers.
@MainActor
final class LocalUsageScheduler {
    struct Session: Hashable, Sendable { fileprivate let id = UUID() }
    struct Query: Hashable, Sendable {
        let kind: String
        let year: String?
        let clients: [String]?
        let windowSecs: Int64?
        let scope: LocalUsageScope
        init(kind: String, year: String? = nil, clients: [String]? = nil, windowSecs: Int64? = nil) {
            self.kind = kind; self.year = year
            let selection = clients.flatMap { $0.isEmpty ? nil : Array(Set($0)).sorted() }
            self.clients = selection
            self.windowSecs = windowSecs
            let scopeKind = kind == "rate" || kind == "trace" ? "recent" : "report"
            let selectionID = selection.map { String(data: try! JSONEncoder().encode($0), encoding: .utf8)! } ?? "all"
            self.scope = LocalUsageScope(id: "\(scopeKind):\(selectionID)", kind: scopeKind, clients: selection)
        }
        var isRecent: Bool { kind == "rate" || kind == "trace" }
    }
    private struct Owner {
        var queries: Set<Query> = []
        var quotas: Set<LocalUsageScope> = []
        var active = false
        var callback: ((Query, Result<LocalUsageValue, Error>) -> Void)?
        var clock: (() -> Void)?
        var status: ((Bool, String?) -> Void)?
    }
    private struct Waiting {
        let id: UUID
        let session: Session
        let query: Query?
        let quota: LocalUsageScope?
        let force: UInt64
        let continuation: CheckedContinuation<LocalUsageValue?, Error>
    }
    private struct Cached {
        let value: LocalUsageValue
        let identity: String
    }
    private let source: any UsageDataSource
    private let observesSources: Bool
    private let automaticallySchedules: Bool
    private let monotonicNow: () -> TimeInterval
    private let wallNow: () -> Date
    private let hints = LocalSourceHints()
    private var owners: [Session: Owner] = [:] { didSet { demandDirty = true } }
    private var waiting: [UUID: Waiting] = [:] { didSet { demandDirty = true } }
    private var demandDirty = true
    private var demandedQueries: Set<Query> = []
    private var demandedScopes: Set<LocalUsageScope> = []
    private var activeDemand = false
    private var results: [Query: Cached] = [:]
    private var sync: LocalUsageSyncResult?
    private var syncedScopes: Set<LocalUsageScope> = []
    private var worker: Task<Void, Never>?
    private var queryWorker: Task<Void, Never>?
    private var deadline: Task<Void, Never>?
    private var observers: [(NotificationCenter, NSObjectProtocol)] = []
    private var revision: UInt64 = 0
    private var rootEpoch: UInt64 = 0
    private var dirtySequence: UInt64 = 1
    private var checkedSequence: UInt64 = 0
    private var dirtyAt: TimeInterval?
    private var topologyRequired = true
    private var forceRequested: UInt64 = 0
    private var forceVerified: UInt64 = 0
    private var lastCheck = -TimeInterval.infinity
    private var lastFull = -TimeInterval.infinity
    private var lastTopology = -TimeInterval.infinity
    private var lastPricing = -TimeInterval.infinity
    private var lastClock = -TimeInterval.infinity
    private var retryAt: TimeInterval = 0
    private var stopped = false
    private var previousLeaseUsable = true
    private var windowFlights: [Data: Task<WindowUsageBatch, Error>] = [:]
    private var windowFlightIDs: [Data: UUID] = [:]
    private var ignoredOutputs: Set<String> = []
    private var ignoredWriteRules: Set<LocalSourceOwnedWriteRule> = []
    private var installedManifest: LocalSourceWatchManifest?
    private var lastHintInstall = -TimeInterval.infinity
    private var quotaPins: [String: Int] = [:]
    private var retiredSnapshots: [(token: String, quotas: [String])] = []

    func ignoreSnapshotOutput(in directory: URL) {
        ignoredOutputs.insert(directory.appendingPathComponent("dashboard-snapshot.json").absoluteString)
        ignoredWriteRules.insert(LocalSourceOwnedWriteRule(
            parentURL: directory.absoluteString,
            pattern: "^\\.dashboard-snapshot\\.json\\.[0-9]+\\.[0-9]+\\.tmp$"))
    }

    func finishQuotaSnapshot(_ token: String) {
        let count = (quotaPins[token] ?? 1) - 1
        quotaPins[token] = count > 0 ? count : nil
        drainRetiredSnapshots()
    }

    private func retire(_ snapshot: LocalUsageSyncResult) {
        retiredSnapshots.append((snapshot.leaseToken, snapshot.scopes.compactMap(\.quotaSnapshotToken)))
        drainRetiredSnapshots()
    }

    private func drainRetiredSnapshots() {
        let ready = retiredSnapshots.filter { entry in !entry.quotas.contains { quotaPins[$0] != nil } }
        retiredSnapshots.removeAll { entry in !entry.quotas.contains { quotaPins[$0] != nil } }
        for entry in ready { Task { [source] in await source.releaseLocalUsage(entry.token) } }
    }
    var fullVerificationInterval: TimeInterval = 1800 {
        didSet { kick() }
    }
    private(set) var lastError: String?
    private(set) var syncCount = 0
    private var sourcePending = false
    var sourceHintDiagnostics: LocalSourceHints.Diagnostics { hints.diagnostics }
    private(set) var queryCount = 0
    var pendingAcquisitions: Int { waiting.count }

    init(source: any UsageDataSource, observesSources: Bool = true,
         automaticallySchedules: Bool = true,
         monotonicNow: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime },
         wallNow: @escaping () -> Date = { Date() }) {
        self.source = source
        self.observesSources = observesSources && source.usesNativeLocalUsage
        self.automaticallySchedules = automaticallySchedules
        self.monotonicNow = monotonicNow; self.wallNow = wallNow
    }

    func openSession() -> Session {
        let token = Session()
        if !stopped {
            owners[token] = Owner()
            if automaticallySchedules, observers.isEmpty { installClockNotifications() }
        }
        return token
    }
    func contains(_ session: Session) -> Bool { owners[session] != nil && !stopped }

    func update(_ session: Session, queries: Set<Query>, active: Bool,
                onValue: @escaping @MainActor (Query, Result<LocalUsageValue, Error>) -> Void) {
        guard var owner = owners[session], !stopped else { return }
        let changed = owner.queries != queries || owner.active != active
        owner.queries = queries; owner.active = active; owner.callback = onValue
        owners[session] = owner
        if changed { revision &+= 1; pruneResults() }
        for query in queries {
            if !needsSync, let held = results[query], held.identity == identity(query) {
                onValue(query, .success(held.value))
            }
        }
        kick()
    }

    func setClock(_ session: Session, _ callback: @escaping @MainActor () -> Void) {
        guard owners[session] != nil else { return }
        owners[session]?.clock = callback
        kick()
    }

    func setStatus(_ session: Session, _ callback: @escaping @MainActor (Bool, String?) -> Void) {
        guard owners[session] != nil else { return }
        owners[session]?.status = callback
        callback(sourcePending, lastError)
    }

    private func publishStatus(_ pending: Bool) {
        sourcePending = pending
        for owner in owners.values { owner.status?(pending, lastError) }
    }

    func setQuotaScopes(_ session: Session, _ scopes: Set<LocalUsageScope>) {
        guard var owner = owners[session], owner.quotas != scopes else { return }
        owner.quotas = scopes; owners[session] = owner
        revision &+= 1
        kick()
    }

    static func quotaScope(clientId: String, accountKey: String?) -> LocalUsageScope {
        let account = accountKey.map { String(data: try! JSONEncoder().encode($0), encoding: .utf8)! } ?? "primary"
        // Primary account is a cross-client attribution scope. Extra Claude
        // account partitioning belongs to native registry resolution.
        return LocalUsageScope(id: "quota:\(account)", kind: "quota",
                              clientId: accountKey == nil ? nil : clientId, accountKey: accountKey)
    }

    func release(_ session: Session) {
        guard owners.removeValue(forKey: session) != nil else { return }
        revision &+= 1
        if owners.isEmpty {
            for (center, token) in observers { center.removeObserver(token) }
            observers.removeAll()
        }
        for entry in waiting.values.filter({ $0.session == session }) {
            waiting.removeValue(forKey: entry.id)?.continuation.resume(throwing: CancellationError())
        }
        pruneResults()
        if scopes.isEmpty {
            hints.stop(); deadline?.cancel(); deadline = nil
            releaseSnapshot()
        }
        kick()
    }

    func acquire(_ query: Query, session: Session, force: Bool = false) async throws -> LocalUsageValue {
        guard contains(session) else { throw CancellationError() }
        let barrier = requestForce(force)
        if !force, let held = results[query], held.identity == identity(query),
           syncedScopes.contains(query.scope), !needsSync {
            return held.value
        }
        let id = UUID()
        let value = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                waiting[id] = Waiting(id: id, session: session, query: query, quota: nil,
                                      force: barrier, continuation: continuation)
                kick()
            }
        } onCancel: {
            Task { @MainActor [weak self] in self?.cancelWaiter(id) }
        }
        guard contains(session), let value else { throw CancellationError() }
        return value
    }

    func quotaSnapshot(clientId: String, accountKey: String?, session: Session) async throws -> String {
        guard contains(session) else { throw CancellationError() }
        let scope = Self.quotaScope(clientId: clientId, accountKey: accountKey)
        if !syncedScopes.contains(scope) || needsSync {
            let id = UUID()
            _ = try await withTaskCancellationHandler {
                try await withCheckedThrowingContinuation { continuation in
                    waiting[id] = Waiting(id: id, session: session, query: nil, quota: scope,
                                          force: forceRequested, continuation: continuation)
                    kick()
                }
            } onCancel: { Task { @MainActor [weak self] in self?.cancelWaiter(id) } }
        }
        guard contains(session), let token = sync?.scopes.first(where: { $0.id == scope.id })?.quotaSnapshotToken
        else { throw WindowUsageBatch.QueryError.invalidSnapshot }
        quotaPins[token, default: 0] += 1
        return token
    }

    /// Exact compact same-token jobs coalesce independently of provider policy.
    func isCurrentQuotaSnapshot(_ token: String, session: Session) -> Bool {
        contains(session) && sync?.scopes.contains(where: { $0.quotaSnapshotToken == token }) == true
    }
    func windowUsage(_ request: WindowUsageRequest, session: Session) async throws -> WindowUsageBatch {
        let pinnedToken = request.snapshotToken.flatMap { token in
            source.usesNativeLocalUsage || quotaPins[token] != nil ? token : nil
        }
        guard contains(session),
              pinnedToken.map({ isCurrentQuotaSnapshot($0, session: session) }) ?? !source.usesNativeLocalUsage else {
            throw CancellationError()
        }
        let epoch = rootEpoch
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
        let key = try encoder.encode(request)
        let id: UUID
        let task: Task<WindowUsageBatch, Error>
        if let held = windowFlights[key], let heldID = windowFlightIDs[key] {
            task = held; id = heldID
        } else {
            id = UUID()
            task = Task { [source] in try await source.windowUsage(request) }
            windowFlights[key] = task; windowFlightIDs[key] = id
        }
        defer {
            if windowFlightIDs[key] == id {
                windowFlights[key] = nil; windowFlightIDs[key] = nil
            }
        }
        let value: WindowUsageBatch
        do { value = try await task.value }
        catch {
            if !(error is CancellationError), rootEpoch == epoch, let pinnedToken,
               isCurrentQuotaSnapshot(pinnedToken, session: session) {
                previousLeaseUsable = false; syncedScopes.removeAll()
                retryAt = monotonicNow() + 10
                kick()
            }
            throw error
        }
        guard contains(session), rootEpoch == epoch,
              pinnedToken.map({ isCurrentQuotaSnapshot($0, session: session) }) ?? true,
              !Task.isCancelled else { throw CancellationError() }
        return value
    }

    func sourcesChanged(requiresTopology: Bool) {
        dirtySequence &+= 1
        dirtyAt = dirtyAt ?? monotonicNow()
        topologyRequired = topologyRequired || requiresTopology
        kick()
    }

    func rootsChanged() {
        rootEpoch &+= 1; revision &+= 1
        previousLeaseUsable = false
        results.removeAll(); syncedScopes.removeAll()
        topologyRequired = true; dirtySequence &+= 1; dirtyAt = nil
        kick()
    }

    func clockChanged(reverify: Bool = false) {
        results.removeAll(); lastClock = -TimeInterval.infinity
        if reverify { sourcesChanged(requiresTopology: true); dirtyAt = nil }
        kick()
    }

    func stop() {
        stopped = true
        for session in Array(owners.keys) { release(session) }
        deadline?.cancel(); deadline = nil; hints.stop()
        for (center, token) in observers { center.removeObserver(token) }
        observers.removeAll()
        releaseSnapshot()
    }

    /// Deterministic tests drive real transitions without timer sleeps.
    func runDueWork() async {
        if let worker { await worker.value; return }
        let task = Task { await self.runWorker() }
        worker = task
        await task.value
    }

    private func refreshDemand() {
        guard demandDirty else { return }
        demandDirty = false
        demandedQueries.removeAll(keepingCapacity: true)
        demandedScopes.removeAll(keepingCapacity: true)
        activeDemand = false
        for owner in owners.values {
            demandedQueries.formUnion(owner.queries)
            demandedScopes.formUnion(owner.quotas)
            activeDemand = activeDemand || (owner.active && (!owner.queries.isEmpty || !owner.quotas.isEmpty))
        }
        for entry in waiting.values {
            if let query = entry.query { demandedQueries.insert(query) }
            if let quota = entry.quota { demandedScopes.insert(quota) }
        }
        for query in demandedQueries { demandedScopes.insert(query.scope) }
    }
    private var queries: Set<Query> { refreshDemand(); return demandedQueries }
    private var scopes: Set<LocalUsageScope> { refreshDemand(); return demandedScopes }
    private var active: Bool { refreshDemand(); return activeDemand }
    private var checkInterval: TimeInterval { active ? 30 : 300 }
    private var needsPricing: Bool {
        scopes.contains { $0.kind == "report" || $0.kind == "quota" }
            && monotonicNow() - lastPricing >= 3600
    }
    private var needsSync: Bool {
        let now = monotonicNow()
        return !scopes.isEmpty && (sync == nil || syncedScopes != scopes
            || forceVerified < forceRequested || now - lastCheck >= checkInterval
            || now - lastFull >= fullVerificationInterval || now - lastTopology >= 300
            || needsPricing
            || (dirtySequence > checkedSequence && (dirtyAt == nil || now >= dirtyAt! + 0.25)))
    }
    private func requestForce(_ force: Bool) -> UInt64 {
        if force { forceRequested &+= 1; retryAt = 0 }
        return forceRequested
    }
    private func cancelWaiter(_ id: UUID) {
        waiting.removeValue(forKey: id)?.continuation.resume(throwing: CancellationError())
        pruneResults(); kick()
    }
    private func pruneResults() {
        let wanted = queries
        results = results.filter { wanted.contains($0.key) }
    }
    private func releaseSnapshot() {
        guard let snapshot = sync else { return }
        sync = nil; syncedScopes.removeAll(); installedManifest = nil
        retire(snapshot)
    }
    private func identity(_ query: Query) -> String {
        guard let sync, let scope = sync.scopes.first(where: { $0.id == query.scope.id }) else { return "missing" }
        let now = wallNow()
        let calendar = Calendar.current
        let day = calendar.startOfDay(for: now).timeIntervalSince1970
        let clock = query.isRecent ? Int64(now.timeIntervalSince1970 / 10) : Int64(day)
        let price = query.isRecent ? "unpriced" : sync.priceGeneration
        let zone = query.isRecent ? "absolute" : TimeZone.current.identifier
        return "\(rootEpoch)|\(sync.rootGeneration)|\(scope.sourceGeneration)|\(price)|\(zone)|\(clock)"
    }
    private var nextClockDeadline: TimeInterval {
        if queries.contains(where: \.isRecent) || owners.values.contains(where: { $0.clock != nil }) {
            return lastClock + 10
        }
        let now = wallNow()
        let midnight = Calendar.current.date(byAdding: .day, value: 1, to: Calendar.current.startOfDay(for: now))!
        return monotonicNow() + max(0, midnight.timeIntervalSince(now))
    }

    private func kick() {
        guard !stopped, automaticallySchedules else { return }
        deadline?.cancel(); deadline = nil
        if scopes.isEmpty && !owners.values.contains(where: { $0.clock != nil }) { return }
        let now = monotonicNow()
        if worker == nil, now >= retryAt, needsSync { worker = Task { await self.runWorker() } }
        let queryMissing = queries.contains {
            syncedScopes.contains($0.scope) && results[$0]?.identity != identity($0)
        }
        if queryWorker == nil, now >= retryAt, queryMissing || now >= nextClockDeadline {
            _ = startQueryWorker()
        } else if now >= nextClockDeadline { tickClock() }
        armDeadline()
    }
    private func armDeadline() {
        guard !stopped, automaticallySchedules else { return }
        let now = monotonicNow()
        var due = nextClockDeadline
        if worker == nil, !scopes.isEmpty {
            due = min(due, lastCheck + checkInterval, lastFull + fullVerificationInterval,
                      lastTopology + 300)
            if scopes.contains(where: { $0.kind != "recent" }) { due = min(due, lastPricing + 3600) }
            if dirtySequence > checkedSequence { due = min(due, (dirtyAt ?? now) + 0.25) }
        }
        due = max(due, retryAt)
        deadline = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0.01, due - now)))
            guard !Task.isCancelled else { return }
            self?.deadline = nil; self?.kick()
        }
    }
    private func runWorker() async {
        defer {
            worker = nil
            publishStatus(false)
            if scopes.isEmpty { releaseSnapshot() }
            kick()
        }
        guard !stopped else { return }
        let now = monotonicNow()
        if needsSync && now >= retryAt {
            publishStatus(true)
            let requestedScopes = scopes
            let epoch = rootEpoch
            let issueRevision = revision
            let sequence = dirtySequence
            let forceBarrier = forceRequested
            let full = forceVerified < forceBarrier || now - lastFull >= fullVerificationInterval
            let topology = topologyRequired || full || now - lastTopology >= 300 || sync == nil
            let pricing = needsPricing
            do {
                syncCount += 1
                let captured = try await source.localUsageSync(LocalUsageSyncRequest(
                    scopes: requestedScopes.sorted { $0.id < $1.id },
                    previousLeaseToken: previousLeaseUsable ? sync?.leaseToken : nil,
                    force: full, verifyTopology: topology, refreshPricing: pricing))
                guard !stopped, epoch == rootEpoch, issueRevision == revision, requestedScopes == scopes else {
                    if captured.leaseToken != sync?.leaseToken { await source.releaseLocalUsage(captured.leaseToken) }
                    return
                }
                let previous = sync
                sync = captured; syncedScopes = requestedScopes
                previousLeaseUsable = true
                lastCheck = monotonicNow(); checkedSequence = sequence
                if full { lastFull = lastCheck }
                if topology { lastTopology = lastCheck; topologyRequired = false }
                if pricing { lastPricing = lastCheck }
                forceVerified = forceBarrier
                if checkedSequence == dirtySequence { dirtyAt = nil }
                lastError = nil; retryAt = 0
                if observesSources {
                    let manifest = LocalSourceWatchManifest(
                        roots: captured.watchManifest.roots,
                        ignoredFileURLs: Array(Set(captured.watchManifest.ignoredFileURLs).union(ignoredOutputs)).sorted(),
                        ignoredBasenameRules: Array(Set(captured.watchManifest.ignoredBasenameRules).union(ignoredWriteRules))
                            .sorted { $0.parentURL + $0.pattern < $1.parentURL + $1.pattern })
                    let differs = installedManifest.map {
                        $0.roots != manifest.roots || $0.ignoredFileURLs != manifest.ignoredFileURLs
                            || $0.ignoredBasenameRules != manifest.ignoredBasenameRules
                    } ?? true
                    if differs || (topology && hints.diagnostics.requiresVerificationFallback
                                   && monotonicNow() - lastHintInstall >= 300) {
                        installedManifest = manifest
                        lastHintInstall = monotonicNow()
                        hints.install(manifest) { [weak self] topology in
                            self?.sourcesChanged(requiresTopology: topology)
                        }
                    }
                }
                if let previous, previous.leaseToken != captured.leaseToken { retire(previous) }
            } catch {
                guard epoch == rootEpoch, issueRevision == revision else { return }
                lastError = String(describing: error)
                previousLeaseUsable = false; syncedScopes.removeAll()
                retryAt = forceRequested > forceBarrier ? 0 : monotonicNow() + min(10, checkInterval)
                failWaiters(error, throughForce: forceBarrier)
                for (session, owner) in owners where contains(session) {
                    for query in owner.queries { owner.callback?(query, .failure(error)) }
                }
                return
            }
        }
        await runClockWork()
    }

    /// Pure captured queries remain schedulable while mounted source I/O blocks.
    func runClockWork() async { await startQueryWorker().value }

    private func startQueryWorker() -> Task<Void, Never> {
        if let queryWorker { return queryWorker }
        let task = Task {
            defer { queryWorker = nil; tickClock(); kick() }
            await runQueries()
        }
        queryWorker = task
        return task
    }

    private func runQueries() async {
        guard !stopped, let snapshot = sync else { return }
        let issueRevision = revision
        let epoch = rootEpoch
        for query in queries.sorted(by: { $0.kind == "graph" && $1.kind != "graph" }) {
            guard issueRevision == revision, epoch == rootEpoch, !stopped else { return }
            guard syncedScopes.contains(query.scope) else { continue }
            let key = identity(query)
            if results[query]?.identity == key { continue }
            do {
                queryCount += 1
                let value = try await source.localUsageQuery(LocalUsageQueryRequest(
                    leaseToken: snapshot.leaseToken, scopeId: query.scope.id, kind: query.kind,
                    year: query.year, clients: query.clients, windowSecs: query.windowSecs,
                    nowMs: Int64(wallNow().timeIntervalSince1970 * 1000)))
                guard issueRevision == revision, epoch == rootEpoch, !stopped, queries.contains(query),
                      syncedScopes.contains(query.scope), sync?.leaseToken == snapshot.leaseToken,
                      key == identity(query) else { return }
                results[query] = Cached(value: value, identity: key)
                for owner in owners.values where owner.queries.contains(query) {
                    owner.callback?(query, .success(value))
                }
            } catch {
                guard issueRevision == revision, epoch == rootEpoch else { return }
                lastError = String(describing: error); retryAt = monotonicNow() + 10
                previousLeaseUsable = false; syncedScopes.removeAll()
                for owner in owners.values where owner.queries.contains(query) {
                    owner.callback?(query, .failure(error))
                }
                for entry in waiting.values.filter({ $0.query == query }) {
                    waiting.removeValue(forKey: entry.id)?.continuation.resume(throwing: error)
                }
            }
        }
        for entry in Array(waiting.values) where !needsSync && contains(entry.session) && entry.force <= forceVerified {
            if let query = entry.query, let value = results[query], value.identity == identity(query) {
                waiting.removeValue(forKey: entry.id)?.continuation.resume(returning: value.value)
            } else if let quota = entry.quota, syncedScopes.contains(quota) {
                waiting.removeValue(forKey: entry.id)?.continuation.resume(returning: nil)
            }
        }
        pruneResults()
    }
    private func failWaiters(_ error: Error, throughForce: UInt64) {
        for entry in Array(waiting.values) where entry.force <= throughForce {
            waiting.removeValue(forKey: entry.id)?.continuation.resume(throwing: error)
        }
    }
    private func tickClock() {
        guard monotonicNow() - lastClock >= 10 else { return }
        lastClock = monotonicNow()
        for owner in owners.values { owner.clock?() }
    }
    private func installClockNotifications() {
        for name in [Notification.Name.NSSystemTimeZoneDidChange, .NSSystemClockDidChange,
                     .NSCalendarDayChanged] {
            let center = NotificationCenter.default
            let token = center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.clockChanged() }
            }
            observers.append((center, token))
        }
        let center = NSWorkspace.shared.notificationCenter
        let token = center.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.clockChanged(reverify: true) }
        }
        observers.append((center, token))
    }
}
