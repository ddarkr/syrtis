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
    struct DisplaySnapshot {
        let value: LocalUsageValue
        let identity: String
        let capturedAt: Date
        let scanRoots: String
        fileprivate var requiresRefresh = false
    }
    private struct RecentClock {
        let origin: TimeInterval
        let interval: TimeInterval
        let revision: UInt64
        func bucket(at now: TimeInterval) -> Int64 {
            Int64(max(0, (now - origin) / interval))
        }
        func deadline(at now: TimeInterval) -> TimeInterval {
            origin + Double(bucket(at: now) + 1) * interval
        }
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
    private var activeRecentQueries: Set<Query> = []
    private var recentClocks: [Query: RecentClock] = [:]
    private var recentClockRevision: UInt64 = 0
    private var demandedScopes: Set<LocalUsageScope> = []
    private var activeDemand = false
    private var results: [Query: DisplaySnapshot] = [:]
    private var sync: LocalUsageSyncResult?
    private var syncedScopes: Set<LocalUsageScope> = []
    private var capturingScopes: Set<LocalUsageScope> = []
    private var worker: Task<Void, Never>?
    private var recentQueryWorker: Task<Void, Never>?
    private var historyQueryWorker: Task<Void, Never>?
    private var deadline: Task<Void, Never>?
    private var observers: [(NotificationCenter, NSObjectProtocol)] = []
    private var rootEpoch: UInt64 = 0
    private var dirtySequence: UInt64 = 1
    private var checkedSequence: UInt64 = 0
    private var dirtyAt: TimeInterval?
    private var topologyRequired = true
    /// Bounded coalesced file evidence from FSEvents; never postpones the
    /// periodic full check and never survives force/topology/side syncs.
    private var pendingHintURLs: Set<String>?
    /// Sticky until consumed: an unnamed or overflowed batch stays dropped,
    /// since later names cannot replace the missing evidence.
    private var pendingHintsOverflowed = false
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
    private var leasePins: [String: Int] = [:]
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

    private func pinLease(_ token: String) { leasePins[token, default: 0] += 1 }
    private func unpinLease(_ token: String) {
        let count = (leasePins[token] ?? 1) - 1
        if count > 0 { leasePins[token] = count } else { leasePins[token] = nil }
        drainRetiredSnapshots()
    }
    private func retire(_ snapshot: LocalUsageSyncResult) {
        // A snapshot identical to the live one is borrowed, not owned: the
        // live lifecycle frees it, so only unpinned superseded generations
        // enter the drain. Prevents double-release of a reused native token.
        guard snapshot.leaseToken != sync?.leaseToken,
              !retiredSnapshots.contains(where: { $0.token == snapshot.leaseToken }) else { return }
        retiredSnapshots.append((snapshot.leaseToken, snapshot.scopes.compactMap(\.quotaSnapshotToken)))
        drainRetiredSnapshots()
    }
    private func drainRetiredSnapshots() {
        let ready = retiredSnapshots.filter { entry in
            leasePins[entry.token] == nil && !entry.quotas.contains { quotaPins[$0] != nil }
        }
        retiredSnapshots.removeAll { entry in
            leasePins[entry.token] == nil && !entry.quotas.contains { quotaPins[$0] != nil }
        }
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

    /// Display-only last good history. It never satisfies a freshness barrier.
    /// Root changes and demand removal evict it through the ordinary cache.
    func displaySnapshot(for query: Query) -> DisplaySnapshot? {
        guard !stopped, !query.isRecent, let held = results[query],
              held.scanRoots == ClaudeExtraRoots.appliedPayloadJSON else { return nil }
        return held
    }

    private func cachedIdentity(_ query: Query) -> String? {
        guard let held = results[query], !held.requiresRefresh else { return nil }
        return held.identity
    }

    func update(_ session: Session, queries: Set<Query>, active: Bool,
                onValue: @escaping @MainActor (Query, Result<LocalUsageValue, Error>) -> Void) {
        guard var owner = owners[session], !stopped else { return }
        let changed = owner.queries != queries || owner.active != active
        owner.queries = queries; owner.active = active; owner.callback = onValue
        owners[session] = owner
        if changed { pruneResults() }
        for query in queries {
            if !needsSync, let held = results[query], cachedIdentity(query) == identity(query),
               syncedScopes.contains(query.scope) {
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
        if owners.isEmpty {
            for (center, token) in observers { center.removeObserver(token) }
            observers.removeAll()
        }
        // Late completions for this owner die here; a reopened session has a
        // fresh identity and only receives work issued under it.
        for entry in waiting.values.filter({ $0.session == session }) {
            waiting.removeValue(forKey: entry.id)?.continuation.resume(throwing: CancellationError())
        }
        pruneResults()
        if scopes.isEmpty {
            hints.stop(); deadline?.cancel(); deadline = nil
            // Only the empty-demand lane retires its snapshot; in-flight
            // query pins keep their own leases alive through the drain.
            releaseSnapshot()
        }
        kick()
    }

    func acquire(_ query: Query, session: Session, force: Bool = false) async throws -> LocalUsageValue {
        guard contains(session) else { throw CancellationError() }
        let barrier = requestForce(force)
        // Exact held results stay usable while unrelated scopes keep global
        // needsSync true; starvation by unrelated demand is a scheduler bug.
        if !force, let held = results[query], cachedIdentity(query) == identity(query),
           syncedScopes.contains(query.scope) {
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

    func sourcesChanged(requiresTopology: Bool, changedFileURLs: [String]? = nil) {
        dirtySequence &+= 1
        dirtyAt = dirtyAt ?? monotonicNow()
        topologyRequired = topologyRequired || requiresTopology
        if requiresTopology || changedFileURLs == nil {
            // The stronger fallback owns the sync: named evidence is
            // meaningless beside topology or unbounded verification.
            pendingHintURLs = nil
            pendingHintsOverflowed = true
        } else if let urls = changedFileURLs, !pendingHintsOverflowed {
            var merged = pendingHintURLs ?? []
            merged.formUnion(urls)
            // ponytail: fixed 256-path bound; overflow drops names, flags keep fallback.
            if merged.count > 256 { pendingHintURLs = nil; pendingHintsOverflowed = true }
            else { pendingHintURLs = merged }
        }
        kick()
    }

    func rootsChanged() {
        // Late in-flight outputs carry the old epoch and die on return. The
        // replaced snapshot retires through the pin drain: ordinary query
        // pins hold it alive, expired generations are never revived.
        rootEpoch &+= 1
        previousLeaseUsable = false
        let replaced = sync
        sync = nil; results.removeAll(); syncedScopes.removeAll()
        pendingHintURLs = nil
        pendingHintsOverflowed = false
        if let replaced { retire(replaced) }
        topologyRequired = true; dirtySequence &+= 1; dirtyAt = nil
        kick()
    }

    func clockChanged(reverify: Bool = false) {
        // Selective expiry: recents always expire by clock; historical display
        // aggregates expire only when their day/timezone bucket actually moved,
        // so irrelevant ticks reuse the valid held aggregate without DB refold.
        refreshDemand()
        for query in demandedQueries where query.isRecent {
            resetRecentClock(query, interval: recentExpiryInterval(for: query))
        }
        let expired = results.filter { query, cached in
            query.isRecent || cached.identity != identity(query)
        }.map(\.key)
        for query in expired {
            if query.isRecent { results.removeValue(forKey: query) }
            else { results[query]?.requiresRefresh = true }
        }
        lastClock = -TimeInterval.infinity
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
    /// The source worker owns ingestion only: it launches both pure query
    /// lanes without awaiting them, so a blocked graph/models query never
    /// holds source verification. runDueWork then awaits the full turn
    /// (worker plus clock lanes) for deterministic test seams.
    func runDueWork() async {
        if let worker { await worker.value }
        else {
            let task = Task { await self.runWorker() }
            worker = task
            await task.value
        }
        await runClockWork()
    }

    private func refreshDemand() {
        guard demandDirty else { return }
        demandDirty = false
        demandedQueries.removeAll(keepingCapacity: true)
        activeRecentQueries.removeAll(keepingCapacity: true)
        demandedScopes.removeAll(keepingCapacity: true)
        activeDemand = false
        for owner in owners.values {
            demandedQueries.formUnion(owner.queries)
            if owner.active {
                for query in owner.queries where query.isRecent { activeRecentQueries.insert(query) }
            }
            demandedScopes.formUnion(owner.quotas)
            activeDemand = activeDemand || (owner.active && (!owner.queries.isEmpty || !owner.quotas.isEmpty))
        }
        for entry in waiting.values {
            if let query = entry.query { demandedQueries.insert(query) }
            if let quota = entry.quota { demandedScopes.insert(quota) }
        }
        for query in demandedQueries { demandedScopes.insert(query.scope) }
        for query in recentClocks.keys.filter({ !demandedQueries.contains($0) }) {
            recentClocks.removeValue(forKey: query)
        }
        for query in demandedQueries where query.isRecent {
            let interval: TimeInterval = activeRecentQueries.contains(query) ? 5 : 10
            if recentClocks[query]?.interval != interval { resetRecentClock(query, interval: interval) }
        }
    }
    private func resetRecentClock(_ query: Query, interval: TimeInterval) {
        recentClockRevision &+= 1
        recentClocks[query] = RecentClock(origin: monotonicNow(), interval: interval,
                                        revision: recentClockRevision)
    }
    private var scopes: Set<LocalUsageScope> { refreshDemand(); return demandedScopes }
    private var active: Bool { refreshDemand(); return activeDemand }
    private func recentExpiryInterval(for query: Query) -> TimeInterval {
        refreshDemand()
        return activeRecentQueries.contains(query) ? 5 : 10
    }
    private var clockExpiryInterval: TimeInterval {
        refreshDemand()
        return activeRecentQueries.isEmpty ? 10 : 5
    }
    private var checkInterval: TimeInterval { active ? 30 : 300 }
    private var needsPricing: Bool {
        scopes.contains { $0.kind == "report" || $0.kind == "quota" }
            && monotonicNow() - lastPricing >= 3600
    }
    private var needsSync: Bool {
        let now = monotonicNow()
        // Coverage, not equality: a contracted subset still covered by the
        // installed scopes needs no native recollect. Expansion still syncs;
        // force, interval, topology, pricing, and dirty hints stay authoritative.
        return !scopes.isEmpty && (sync == nil || !scopes.isSubset(of: syncedScopes)
            || forceVerified < forceRequested || now - lastCheck >= checkInterval
            || now - lastFull >= fullVerificationInterval || now - lastTopology >= 300
            || needsPricing
            || (dirtySequence > checkedSequence && (dirtyAt == nil || now >= dirtyAt! + 0.1)))
    }
    private func requestForce(_ force: Bool) -> UInt64 {
        if force { forceRequested &+= 1; retryAt = 0 }
        return forceRequested
    }
    private func cancelWaiter(_ id: UUID) {
        waiting.removeValue(forKey: id)?.continuation.resume(throwing: CancellationError())
        pruneResults(); kick()
    }
    // RAM-first: no dormant retention. Only demanded compact display aggregates
    // stay; lens-away/closed reports drop immediately so reopening refolds
    // from the installed capture or recollects. Demanded entries stay
    // regardless of identity; staleness refolds on the query path.
    private func pruneResults() {
        refreshDemand()
        if results.allSatisfy({ demandedQueries.contains($0.key) }) { return }
        // Dictionary.filter returns pairs, not a Dictionary: collect the
        // dropped keys first, then remove (never mutate during iteration).
        for query in results.keys.filter({ !demandedQueries.contains($0) }) {
            results.removeValue(forKey: query)
        }
    }
    /// Owner IDs demanding one query. Captured before the await; late
    /// completions intersect current IDs so a close plus reopen (fresh
    /// identity) rejects old outputs while unrelated changes keep overlap.
    private func demanders(of query: Query) -> Set<Session> {
        var ids = Set<Session>()
        for (session, owner) in owners where owner.queries.contains(query) { ids.insert(session) }
        for entry in waiting.values where entry.query == query { ids.insert(entry.session) }
        return ids
    }
    /// Owner IDs demanding at least one of these scopes, via queries or quota.
    private func demanders(ofScopes scopes: Set<LocalUsageScope>) -> Set<Session> {
        var ids = Set<Session>()
        for (session, owner) in owners
        where !owner.queries.filter({ scopes.contains($0.scope) }).isEmpty
            || !owner.quotas.intersection(scopes).isEmpty { ids.insert(session) }
        for entry in waiting.values where entry.query.map({ scopes.contains($0.scope) }) == true
            || entry.quota.map({ scopes.contains($0) }) == true { ids.insert(entry.session) }
        return ids
    }
    /// Ordinary waiters complete on their own exact result; unrelated scopes
    /// keeping global needsSync true must not starve them. Forced barriers stay.
    private func resumeReadyWaiters() {
        for entry in Array(waiting.values) where contains(entry.session) && entry.force <= forceVerified {
            if let query = entry.query, let value = results[query],
               cachedIdentity(query) == identity(query), syncedScopes.contains(query.scope) {
                waiting.removeValue(forKey: entry.id)?.continuation.resume(returning: value.value)
            } else if let quota = entry.quota, syncedScopes.contains(quota) {
                waiting.removeValue(forKey: entry.id)?.continuation.resume(returning: nil)
            }
        }
    }
    /// Ordinary (non-forced) waiters on one query resume after side staging.
    private func resumeQueryWaiters(_ query: Query) {
        for entry in Array(waiting.values) where entry.query == query
            && contains(entry.session) && entry.force <= forceVerified {
            if let value = results[query], cachedIdentity(query) == identity(query),
               syncedScopes.contains(query.scope) {
                waiting.removeValue(forKey: entry.id)?.continuation.resume(returning: value.value)
            }
        }
    }
    private func releaseSnapshot() {
        guard let snapshot = sync else { return }
        sync = nil; syncedScopes.removeAll(); installedManifest = nil
        retire(snapshot)
    }
    private func identity(_ query: Query) -> String {
        guard let sync else { return "missing" }
        return identity(query, in: sync)
    }
    private func identity(_ query: Query, in capture: LocalUsageSyncResult) -> String {
        guard let scope = capture.scopes.first(where: { $0.id == query.scope.id }) else { return "missing" }
        let now = wallNow()
        let calendar = Calendar.current
        let day = calendar.startOfDay(for: now).timeIntervalSince1970
        refreshDemand()
        let clock: String
        if query.isRecent, let cadence = recentClocks[query] {
            clock = "\(cadence.revision):\(cadence.bucket(at: monotonicNow()))"
        } else {
            clock = String(Int64(day))
        }
        let price = query.isRecent ? "unpriced" : capture.priceGeneration
        let zone = query.isRecent ? "absolute" : TimeZone.current.identifier
        return "\(rootEpoch)|\(capture.rootGeneration)|\(scope.sourceGeneration)|\(price)|\(zone)|\(clock)"
    }
    private var nextClockDeadline: TimeInterval {
        refreshDemand()
        let nowMonotonic = monotonicNow()
        var due = TimeInterval.infinity
        for cadence in recentClocks.values { due = min(due, cadence.deadline(at: nowMonotonic)) }
        if owners.values.contains(where: { $0.clock != nil }) {
            due = min(due, lastClock + clockExpiryInterval)
        }
        if due != .infinity { return due }
        let now = wallNow()
        let midnight = Calendar.current.date(byAdding: .day, value: 1, to: Calendar.current.startOfDay(for: now))!
        return monotonicNow() + max(0, midnight.timeIntervalSince(now))
    }

    private func kick() {
        guard !stopped, automaticallySchedules else { return }
        deadline?.cancel(); deadline = nil
        refreshDemand()
        if demandedScopes.isEmpty && !owners.values.contains(where: { $0.clock != nil }) { return }
        let now = monotonicNow()
        if worker == nil, now >= retryAt, needsSync { worker = Task { await self.runWorker() } }
        let recentMissing = demandedQueries.contains {
            $0.isRecent && syncedScopes.contains($0.scope) && cachedIdentity($0) != identity($0)
        }
        let historyMissing = demandedQueries.contains {
            !$0.isRecent && syncedScopes.contains($0.scope) && cachedIdentity($0) != identity($0)
        }
        if recentQueryWorker == nil, now >= retryAt, recentMissing || now >= nextClockDeadline {
            _ = startRecentQueryWorker()
        }
        if historyQueryWorker == nil, now >= retryAt, historyMissing {
            _ = startHistoryQueryWorker()
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
            if dirtySequence > checkedSequence { due = min(due, (dirtyAt ?? now) + 0.1) }
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
            // First paint stages missing graphs before the slow union: one
            // side capture over the missing graph scopes, then publish each,
            // then the full union sync in the same turn. The side capture never
            // replaces live snapshots, so quota/recent currency survives; the
            // empty-sync lane promotes the union capture as its snapshot.
            let reportStages = unpublishedGraphs()
            let epoch = rootEpoch
            let stagedScopes = Set(reportStages.map { $0.query.scope })
            // A live history lane already owns its graph query: staging a
            // second graph here would run slow native I/O on the ingestion
            // lane and hold the union behind it. First paint (no history
            // worker yet) still stages; a resync while history is blocked
            // skips staging and lets the union ingest first.
            if !reportStages.isEmpty, stagedScopes != scopes, historyQueryWorker == nil {
                if let side = await sideCapture(stagedScopes) {
                    for stage in reportStages {
                        guard !stopped, epoch == rootEpoch else { break }
                        await publishSideGraph(stage.query, from: side, epoch: epoch,
                                               issueDemanderIDs: stage.owners)
                    }
                    await releaseSideCapture(side)
                }
            }
            if !stopped {
                guard await syncScopes(scopes) else { return }
            }
        }
        // Ingestion done: launch both pure query lanes without awaiting
        // them, so the source worker (and sourcePending) releases even
        // while a slow history query is blocked. runDueWork awaits the
        // full turn; the automatic kick path coalesces via the workers.
        _ = startRecentQueryWorker()
        _ = startHistoryQueryWorker()
    }
    /// One native capture over exactly these scopes. Returns false when the
    /// capture was dropped (owner close, root change, no demand overlap):
    /// expired generations are never revived, stale outputs never admitted.
    private func syncScopes(_ requested: Set<LocalUsageScope>) async -> Bool {
        refreshDemand()
        let epoch = rootEpoch
        // Exact admission: only this capture's requested scopes and the owner
        // IDs demanding them matter. Unrelated removals elsewhere must not
        // discard a staged graph; a close plus reopen shares no identity.
        let issueScopes = requested
        let issueDemanderIDs = demanders(ofScopes: requested)
        let sequence = dirtySequence
        let forceBarrier = forceRequested
        let now = monotonicNow()
        let full = forceVerified < forceBarrier || now - lastFull >= fullVerificationInterval
        let topology = topologyRequired || full || now - lastTopology >= 300 || sync == nil
        let pricing = needsPricing
        // A named event cannot satisfy the periodic all-known-input check.
        // Force, topology, pricing and overdue checks omit names.
        // Capture-then-clear: hints arriving during this await queue for the
        // next sync instead of mutating the in-flight request.
        let hinted = pendingHintsOverflowed ? nil : pendingHintURLs
        pendingHintURLs = nil
        pendingHintsOverflowed = false
        let checkDue = now - lastCheck >= checkInterval
        let hintedURLs = (!full && !topology && !pricing && !checkDue) ? hinted?.sorted() : nil
        capturingScopes = requested
        defer { capturingScopes.removeAll() }
        do {
            syncCount += 1
            let captured = try await source.localUsageSync(LocalUsageSyncRequest(
                scopes: requested.sorted { $0.id < $1.id },
                previousLeaseToken: previousLeaseUsable ? sync?.leaseToken : nil,
                force: full, verifyTopology: topology, refreshPricing: pricing,
                changedFileURLs: hintedURLs))
            // its exact scopes; unrelated removals elsewhere keep overlap.
            refreshDemand()
            let admitted = !stopped && epoch == rootEpoch
                && issueScopes.isSubset(of: demandedScopes)
                && !issueDemanderIDs.isDisjoint(with: demanders(ofScopes: issueScopes))
            guard admitted else {
                retire(captured)
                return false
            }
            let previous = sync
            sync = captured
            syncedScopes = requested
            previousLeaseUsable = true
            let completedAt = monotonicNow()
            if hintedURLs == nil { lastCheck = completedAt }
            checkedSequence = sequence
            if full { lastFull = completedAt }
            if topology { lastTopology = completedAt; topologyRequired = false }
            if pricing { lastPricing = completedAt }
            forceVerified = max(forceVerified, forceBarrier)
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
                    hints.install(manifest) { [weak self] topology, urls in
                        self?.sourcesChanged(requiresTopology: topology, changedFileURLs: urls)
                    }
                }
            }
            if let previous, previous.leaseToken != captured.leaseToken { retire(previous) }
            return true
        } catch {
            // A failure only invalidates when the issuing demand still
            // overlaps; unrelated drift leaves the captured graph alone.
            refreshDemand()
            guard epoch == rootEpoch, issueScopes.isSubset(of: demandedScopes),
                  !issueDemanderIDs.isDisjoint(with: demanders(ofScopes: issueScopes)) else { return false }
            // The failed sync consumed the captured names: requeue them so the
            // retry still verifies the same evidence instead of dropping it.
            if let hinted, !full, !topology, !pricing, !pendingHintsOverflowed {
                var merged = pendingHintURLs ?? []
                merged.formUnion(hinted)
                if merged.count > 256 {
                    pendingHintURLs = nil
                    pendingHintsOverflowed = true
                } else {
                    pendingHintURLs = merged
                }
            }
            lastError = String(describing: error)
            previousLeaseUsable = false; syncedScopes.removeAll()
            retryAt = forceRequested > forceBarrier ? 0 : monotonicNow() + min(10, checkInterval)
            failWaiters(error, throughForce: forceBarrier)
            for (session, owner) in owners where contains(session) {
                for query in owner.queries {
                    // Retain compact last-good display for newly opened views,
                    // but force every query to revalidate on recovery.
                    results[query]?.requiresRefresh = true
                    owner.callback?(query, .failure(error))
                }
            }
            return false
        }
    }
    private func unpublishedGraphs() -> [(query: Query, owners: Set<Session>)] {
        refreshDemand()
        var stages: [(query: Query, owners: Set<Session>)] = []
        for query in demandedQueries where query.kind == "graph" && cachedIdentity(query) != identity(query) {
            stages.append((query, demanders(of: query)))
        }
        stages.sort { $0.query.scope.id < $1.query.scope.id }
        return stages
    }
    /// Capture without changing installed freshness or force barriers.
    /// Pin through the entire batch; cleanup preserves a borrowed live token.
    private func sideCapture(_ requested: Set<LocalUsageScope>) async -> LocalUsageSyncResult? {
        refreshDemand()
        let epoch = rootEpoch
        let issueScopes = requested
        let issueDemanderIDs = demanders(ofScopes: requested)
        do {
            syncCount += 1
            let captured = try await source.localUsageSync(LocalUsageSyncRequest(
                scopes: requested.sorted { $0.id < $1.id },
                previousLeaseToken: previousLeaseUsable ? sync?.leaseToken : nil,
                force: false, verifyTopology: false, refreshPricing: false))
            refreshDemand()
            guard !stopped, epoch == rootEpoch, issueScopes.isSubset(of: demandedScopes),
                  !issueDemanderIDs.isDisjoint(with: demanders(ofScopes: issueScopes)) else {
                retire(captured)
                return nil
            }
            pinLease(captured.leaseToken)
            return captured
        } catch {
            refreshDemand()
            guard epoch == rootEpoch, issueScopes.isSubset(of: demandedScopes),
                  !issueDemanderIDs.isDisjoint(with: demanders(ofScopes: issueScopes)) else { return nil }
            lastError = String(describing: error)
            return nil
        }
    }
    private func releaseSideCapture(_ capture: LocalUsageSyncResult) async {
        // Register retirement before unpinning so an already-retired borrowed
        // token cannot drain and then be enqueued for release a second time.
        // A borrowed live token stays alive (sync still references it, never
        // retired while live); an owned new token frees when pins drain.
        retire(capture)
        unpinLease(capture.leaseToken)
    }
    /// Staged side publish: identity derives from the side capture, cached
    /// only on exact match. Late outputs die; stale values never cached.
    /// Ordinary (non-forced) waiters resume; forced waiters await the union.
    private func publishSideGraph(_ query: Query, from side: LocalUsageSyncResult,
                                  epoch: UInt64, issueDemanderIDs: Set<Session>) async {
        refreshDemand()
        guard !stopped, epoch == rootEpoch, demandedQueries.contains(query),
              side.scopes.contains(where: { $0.id == query.scope.id }),
              !issueDemanderIDs.isDisjoint(with: demanders(of: query)) else { return }
        let key = identity(query, in: side)
        if cachedIdentity(query) == key { return }
        queryCount += 1
        let scanRoots = ClaudeExtraRoots.appliedPayloadJSON
        do {
            let value = try await source.localUsageQuery(LocalUsageQueryRequest(
                leaseToken: side.leaseToken, scopeId: query.scope.id, kind: query.kind,
                year: query.year, clients: query.clients, windowSecs: query.windowSecs,
                nowMs: Int64(wallNow().timeIntervalSince1970 * 1000)))
            guard !stopped, epoch == rootEpoch, demandedQueries.contains(query),
                  key == identity(query, in: side),
                  scanRoots == ClaudeExtraRoots.appliedPayloadJSON,
                  !issueDemanderIDs.isDisjoint(with: demanders(of: query)) else { return }
            results[query] = DisplaySnapshot(
                value: value, identity: key, capturedAt: wallNow(), scanRoots: scanRoots)
            for (session, owner) in owners where contains(session) && owner.queries.contains(query) {
                owner.callback?(query, .success(value))
            }
            resumeQueryWaiters(query)
        } catch {
            guard !stopped, epoch == rootEpoch, demandedQueries.contains(query),
                  key == identity(query, in: side),
                  !issueDemanderIDs.isDisjoint(with: demanders(of: query)) else { return }
            lastError = String(describing: error)
        }
    }
    /// Pure captured queries remain schedulable while mounted source I/O blocks.
    /// Recent (rate/trace) starts first on its own lane so a paused slow
    /// history query cannot hold clock-driven live updates behind it; both
    /// lanes are awaited so history suspension stays observable here.
    func runClockWork() async {
        let recent = startRecentQueryWorker()
        let history = startHistoryQueryWorker()
        await recent.value
        await history.value
    }

    private func startRecentQueryWorker() -> Task<Void, Never> {
        if let recentQueryWorker { return recentQueryWorker }
        let task = Task {
            defer { recentQueryWorker = nil; tickClock(); kick() }
            await runQueries(recentOnly: true)
        }
        recentQueryWorker = task
        return task
    }

    private func startHistoryQueryWorker() -> Task<Void, Never> {
        if let historyQueryWorker { return historyQueryWorker }
        let task = Task {
            defer { historyQueryWorker = nil; kick() }
            await runQueries(recentOnly: false)
        }
        historyQueryWorker = task
        return task
    }
    /// History lane order: first paint, visible lenses, deferred aggregates.
    /// Unknown kinds sort last lexically so a new lens cannot jump the graph.
    private static func historyPriority(_ kind: String) -> (Int, String) {
        switch kind {
        case "graph": return (0, kind)
        case "hourly", "agents": return (1, kind)
        case "models": return (2, kind)
        default: return (3, kind)
        }
    }

    private func runQueries(recentOnly: Bool) async {
        guard !stopped, let snapshot = sync else { return }
        refreshDemand()
        let epoch = rootEpoch
        // First paint already stages above; remaining turns keep
        // deterministic order so pure query turns are reproducible. The
        // recent lane carries only rate/trace; history covers the rest.
        // Graph paints first, then the visible hourly/agents lenses, then
        // models: lexical order would run agents before the graph it annotates.
        var ordered = Array(demandedQueries.lazy.filter { $0.isRecent == recentOnly })
        ordered.sort {
            if $0.kind != $1.kind { return Self.historyPriority($0.kind) < Self.historyPriority($1.kind) }
            if $0.scope.id != $1.scope.id { return $0.scope.id < $1.scope.id }
            if $0.year != $1.year { return ($0.year ?? "") < ($1.year ?? "") }
            return ($0.windowSecs ?? -1) < ($1.windowSecs ?? -1)
        }
        for query in ordered {
            guard epoch == rootEpoch, !stopped, sync?.leaseToken == snapshot.leaseToken else { return }
            guard demandedQueries.contains(query), syncedScopes.contains(query.scope) else { continue }
            let issueDemanderIDs = demanders(of: query)
            guard !issueDemanderIDs.isEmpty else { continue }
            // Request-time bucket: a stale completion is dropped and re-issued
            // under the current bucket instead of publishing old data. Recent
            // buckets use this query's demand cadence, history uses day/timezone: the lane
            // split only unblocks recent, it never weakens admission.
            let key = identity(query)
            if cachedIdentity(query) == key { continue }
            let token = snapshot.leaseToken
            pinLease(token)
            let scanRoots = ClaudeExtraRoots.appliedPayloadJSON
            do {
                defer { unpinLease(token) }
                queryCount += 1
                let value = try await source.localUsageQuery(LocalUsageQueryRequest(
                    leaseToken: token, scopeId: query.scope.id, kind: query.kind,
                    year: query.year, clients: query.clients, windowSecs: query.windowSecs,
                    nowMs: Int64(wallNow().timeIntervalSince1970 * 1000)))
                guard epoch == rootEpoch, !stopped, sync?.leaseToken == token else { return }
                guard demandedQueries.contains(query), syncedScopes.contains(query.scope),
                      scanRoots == ClaudeExtraRoots.appliedPayloadJSON,
                      !issueDemanderIDs.isDisjoint(with: demanders(of: query)) else { continue }
                if key != identity(query) {
                    results[query]?.requiresRefresh = true
                    continue
                }
                results[query] = DisplaySnapshot(
                    value: value, identity: key, capturedAt: wallNow(), scanRoots: scanRoots)
                for (session, owner) in owners where contains(session) && owner.queries.contains(query) {
                    owner.callback?(query, .success(value))
                }
                resumeReadyWaiters()
            } catch {
                guard epoch == rootEpoch, !stopped, sync?.leaseToken == token else { return }
                guard demandedQueries.contains(query), syncedScopes.contains(query.scope),
                      !issueDemanderIDs.isDisjoint(with: demanders(of: query)) else { continue }
                guard key == identity(query) else { continue }
                // Native ingestion can evict this generation before its new
                // lease returns. Stop querying the old scope; the capture
                // owns its eventual success or failure, not this old query.
                if capturingScopes.contains(query.scope) {
                    syncedScopes.remove(query.scope)
                    continue
                }
                lastError = String(describing: error); retryAt = monotonicNow() + 10
                previousLeaseUsable = false; syncedScopes.removeAll()
                for (session, owner) in owners where contains(session) && owner.queries.contains(query) {
                    owner.callback?(query, .failure(error))
                }
                for entry in waiting.values.filter({ $0.query == query }) {
                    waiting.removeValue(forKey: entry.id)?.continuation.resume(throwing: error)
                }
            }
        }
        resumeReadyWaiters()
        pruneResults()
    }
    private func failWaiters(_ error: Error, throughForce: UInt64) {
        for entry in Array(waiting.values) where entry.force <= throughForce {
            waiting.removeValue(forKey: entry.id)?.continuation.resume(throwing: error)
        }
    }
    private func tickClock() {
        guard monotonicNow() - lastClock >= clockExpiryInterval else { return }
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
