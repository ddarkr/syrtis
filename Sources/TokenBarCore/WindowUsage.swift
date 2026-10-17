import Foundation

/// Immutable wire ownership shared by scheduler flight keys and the native call.
/// The terminator is appended before these bytes gain any shared owners.
public struct EncodedWindowUsageRequest: Sendable {
    public let request: WindowUsageRequest
    public let canonicalBytes: Data

    public init(_ request: WindowUsageRequest) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        var bytes = try encoder.encode(request)
        bytes.append(0)
        self.request = request
        self.canonicalBytes = bytes
    }
}

/// One exact account query. Provider policy and Unicode model matching stay in Swift.
public struct WindowUsageRequest: Codable, Equatable, Sendable {
    public var accountKey: String?
    public var fromMs: Int64
    public var untilMs: Int64
    public var snapshotToken: String?
    public var includeModelCatalog: Bool
    public var attribution: [Attribution]
    public var modelGroups: [ModelGroup]
    public var inference: [Inference]
    public var windows: [Window]
    public var diagnostics: Diagnostics?

    public init(accountKey: String?, fromMs: Int64, untilMs: Int64,
                snapshotToken: String? = nil, includeModelCatalog: Bool = false,
                confirmed: [UsageAttribution.Record] = [], modelGroups: [ModelGroup] = [],
                inference: [Inference] = [], windows: [Window] = [], diagnostics: Diagnostics? = nil) {
        self.accountKey = accountKey; self.fromMs = fromMs; self.untilMs = untilMs
        self.snapshotToken = snapshotToken; self.includeModelCatalog = includeModelCatalog
        self.attribution = confirmed.map(Attribution.init(record:)); self.modelGroups = modelGroups
        self.inference = inference; self.windows = windows; self.diagnostics = diagnostics
    }

    public struct Attribution: Codable, Equatable, Sendable {
        public let client: String
        public let provider: String
        public let model: String?
        public let state: String
        public let target: String?
        public init(record: UsageAttribution.Record) {
            client = record.client; provider = record.provider; model = record.model
            switch record.state {
            case .assigned(let subscription): state = "assigned"; target = subscription
            case .excluded: state = "excluded"; target = nil
            case .unassigned: state = "unassigned"; target = nil
            }
        }
    }
    public struct ModelGroup: Codable, Equatable, Sendable {
        public let modelId: String
        public let groupId: String
        public init(modelId: String, groupId: String) { self.modelId = modelId; self.groupId = groupId }
    }
    public struct Inference: Codable, Equatable, Sendable {
        public let id: String
        public let subscription: String?
        public let fromMs: Int64
        public let untilMs: Int64
        public init(id: String, subscription: String?, fromMs: Int64, untilMs: Int64) {
            self.id = id; self.subscription = subscription; self.fromMs = fromMs; self.untilMs = untilMs
        }
    }
    public struct Window: Codable, Equatable, Sendable {
        public let id: String
        public let subscription: String
        public let admittedModelIds: [String]?
        public let current: Current?
        public let cycles: [Cycle]
        public init(id: String, subscription: String, admittedModelIds: [String]? = nil,
                    current: Current? = nil, cycles: [Cycle] = []) {
            self.id = id; self.subscription = subscription; self.admittedModelIds = admittedModelIds
            self.current = current; self.cycles = cycles
        }
    }
    public struct Current: Codable, Equatable, Sendable {
        public let fromMs: Int64
        public let untilMs: Int64
        public let observed: Observed?
        public let zones: [Zone]
        public init(fromMs: Int64, untilMs: Int64, observed: Observed? = nil, zones: [Zone] = []) {
            self.fromMs = fromMs; self.untilMs = untilMs; self.observed = observed; self.zones = zones
        }
    }
    public struct Observed: Codable, Equatable, Sendable {
        public let fromMs: Int64
        public let untilMs: Int64
        public init(fromMs: Int64, untilMs: Int64) { self.fromMs = fromMs; self.untilMs = untilMs }
    }
    public struct Zone: Codable, Equatable, Sendable {
        public let index: Int64
        public let loMs: Int64
        public let hiMs: Int64
        public let includeLo: Bool
        public init(index: Int64, loMs: Int64, hiMs: Int64, includeLo: Bool) {
            self.index = index; self.loMs = loMs; self.hiMs = hiMs; self.includeLo = includeLo
        }
    }
    public struct Cycle: Codable, Equatable, Sendable {
        public let resetAtMs: Int64
        public let evidenceStartMs: Int64
        public let firstSampleMs: Int64
        public let lastSampleMs: Int64
        public init(resetAtMs: Int64, evidenceStartMs: Int64, firstSampleMs: Int64, lastSampleMs: Int64) {
            self.resetAtMs = resetAtMs; self.evidenceStartMs = evidenceStartMs
            self.firstSampleMs = firstSampleMs; self.lastSampleMs = lastSampleMs
        }
        public init(_ cycle: QuotaCycle) {
            self.init(resetAtMs: cycle.resetAtMs, evidenceStartMs: cycle.evidenceStartMs,
                      firstSampleMs: cycle.firstSampleMs, lastSampleMs: cycle.lastSampleMs)
        }
    }
    public struct Diagnostics: Codable, Equatable, Sendable {
        public var intervals: [DiagnosticInterval]
        public var buckets: [Buckets]
        public init(intervals: [DiagnosticInterval] = [], buckets: [Buckets] = []) {
            self.intervals = intervals; self.buckets = buckets
        }
    }
    public struct DiagnosticInterval: Codable, Equatable, Sendable {
        public let id: String
        public let fromMs: Int64
        public let untilMs: Int64
        public let includeFrom: Bool
        public let includeUntil: Bool
        public let subscription: String?
        public let admittedModelIds: [String]?
        public init(id: String, fromMs: Int64, untilMs: Int64, includeFrom: Bool = true,
                    includeUntil: Bool = false, subscription: String? = nil, admittedModelIds: [String]? = nil) {
            self.id = id; self.fromMs = fromMs; self.untilMs = untilMs
            self.includeFrom = includeFrom; self.includeUntil = includeUntil
            self.subscription = subscription; self.admittedModelIds = admittedModelIds
        }
    }
    public struct Buckets: Codable, Equatable, Sendable {
        public let id: String
        public let fromMs: Int64
        public let untilMs: Int64
        public let bucketMs: Int64
        public let count: Int64
        public init(id: String, fromMs: Int64, untilMs: Int64, bucketMs: Int64, count: Int64) {
            self.id = id; self.fromMs = fromMs; self.untilMs = untilMs
            self.bucketMs = bucketMs; self.count = count
        }
    }
}

/// Compact facts from one pinned source/root/pricing snapshot; never source rows.
public struct WindowUsageBatch: Codable, Equatable, Sendable {
    public let snapshotToken: String
    public let sourceGeneration: UInt64
    public let rootGeneration: UInt64
    public let priceGeneration: UInt64
    public let modelCatalog: [String]?
    public let coverage: Coverage
    public let undatedCount: UInt32
    public let processingTimeMs: UInt32
    public let inference: [Inference]
    public let windows: [Window]
    public let diagnostics: Diagnostics?

    public init(snapshotToken: String, sourceGeneration: UInt64 = 0, rootGeneration: UInt64 = 0,
                priceGeneration: UInt64 = 0, modelCatalog: [String]? = nil, coverage: Coverage,
                undatedCount: UInt32 = 0, processingTimeMs: UInt32 = 0,
                inference: [Inference] = [], windows: [Window] = [], diagnostics: Diagnostics? = nil) {
        self.snapshotToken = snapshotToken; self.sourceGeneration = sourceGeneration
        self.rootGeneration = rootGeneration; self.priceGeneration = priceGeneration
        self.modelCatalog = modelCatalog; self.coverage = coverage
        self.undatedCount = undatedCount; self.processingTimeMs = processingTimeMs
        self.inference = inference; self.windows = windows; self.diagnostics = diagnostics
    }
    public struct Coverage: Codable, Equatable, Sendable {
        public let fromMs: Int64
        public let untilMs: Int64
        public let state: String
        public init(fromMs: Int64, untilMs: Int64, state: String = "complete") {
            self.fromMs = fromMs; self.untilMs = untilMs; self.state = state
        }
    }
    public struct Inference: Codable, Equatable, Sendable {
        public let id: String
        public let state: String
        public let firstAssignedMs: Int64?
        public init(id: String, state: String = "covered", firstAssignedMs: Int64? = nil) {
            self.id = id; self.state = state; self.firstAssignedMs = firstAssignedMs
        }
    }
    public struct Counter: Codable, Equatable, Sendable {
        public let messageCount: Int64
        public let turnCount: Int64
        public let tokens: Int64
        public let tokensExCacheRead: Int64
        public let breakdown: TokenBreakdown
        public let cost: Double
        public init(messageCount: Int64 = 0, turnCount: Int64 = 0, tokens: Int64 = 0,
                    tokensExCacheRead: Int64 = 0, breakdown: TokenBreakdown = TokenBreakdown(), cost: Double = 0) {
            self.messageCount = messageCount; self.turnCount = turnCount; self.tokens = tokens
            self.tokensExCacheRead = tokensExCacheRead; self.breakdown = breakdown; self.cost = cost
        }
    }
    public struct Observed: Codable, Equatable, Sendable {
        public let tokens: Int64
        public let cost: Double
        public init(tokens: Int64 = 0, cost: Double = 0) { self.tokens = tokens; self.cost = cost }
    }
    public struct Zone: Codable, Equatable, Sendable {
        public let index: Int64
        public let messageCount: Int64
        public let turnCount: Int64
        public let tokens: Int64
        public let tokensExCacheRead: Int64
        public let breakdown: TokenBreakdown
        public let cost: Double
        public init(index: Int64, counter: Counter = Counter()) {
            self.index = index; self.messageCount = counter.messageCount; self.turnCount = counter.turnCount
            self.tokens = counter.tokens; self.tokensExCacheRead = counter.tokensExCacheRead
            self.breakdown = counter.breakdown; self.cost = counter.cost
        }
        public var counter: Counter {
            Counter(messageCount: messageCount, turnCount: turnCount, tokens: tokens,
                    tokensExCacheRead: tokensExCacheRead, breakdown: breakdown, cost: cost)
        }
    }
    public struct Current: Codable, Equatable, Sendable {
        public let state: String
        public let unscopedAssignedMessageCount: Int64
        public let scopeMatchedNothing: Bool
        public let mine: Counter
        public let observed: Observed?
        public let zones: [Zone]
        public init(state: String = "covered", unscopedAssignedMessageCount: Int64 = 0,
                    scopeMatchedNothing: Bool = false, mine: Counter = Counter(),
                    observed: Observed? = nil, zones: [Zone] = []) {
            self.state = state; self.unscopedAssignedMessageCount = unscopedAssignedMessageCount
            self.scopeMatchedNothing = scopeMatchedNothing; self.mine = mine
            self.observed = observed; self.zones = zones
        }
    }
    public struct Window: Codable, Equatable, Sendable {
        public let id: String
        public let current: Current?
        public let cycles: [Cycle]
        public init(id: String, current: Current? = nil, cycles: [Cycle] = []) {
            self.id = id; self.current = current; self.cycles = cycles
        }
    }
    public struct Model: Codable, Equatable, Sendable {
        public let providerId: String
        public let modelId: String
        public let tokens: Int64
        public let breakdown: TokenBreakdown
        public let cost: Double
        public init(providerId: String, modelId: String, tokens: Int64 = 0,
                    breakdown: TokenBreakdown = TokenBreakdown(), cost: Double = 0) {
            self.providerId = providerId; self.modelId = modelId; self.tokens = tokens
            self.breakdown = breakdown; self.cost = cost
        }
    }
    public struct Cycle: Codable, Equatable, Sendable {
        public let resetAtMs: Int64
        public let state: String
        public let mineTokens: Int64
        public let mineTokensExCacheRead: Int64
        public let mineBreakdown: TokenBreakdown
        public let mineCost: Double
        public let mineMessageCount: Int64
        public let mineTurnCount: Int64
        public let spanTokens: Int64
        public let spanCost: Double
        public let otherTokens: Int64
        public let otherCost: Double
        public let otherHasAssigned: Bool
        public let otherHasExcluded: Bool
        public let otherHasUnattributed: Bool
        public let models: [Model]
        public init(resetAtMs: Int64, state: String = "covered", mineTokens: Int64 = 0,
                    mineTokensExCacheRead: Int64 = 0, mineBreakdown: TokenBreakdown = TokenBreakdown(),
                    mineCost: Double = 0, mineMessageCount: Int64 = 0, mineTurnCount: Int64 = 0,
                    spanTokens: Int64 = 0, spanCost: Double = 0, otherTokens: Int64 = 0, otherCost: Double = 0,
                    otherHasAssigned: Bool = false, otherHasExcluded: Bool = false,
                    otherHasUnattributed: Bool = false, models: [Model] = []) {
            self.resetAtMs = resetAtMs; self.state = state; self.mineTokens = mineTokens
            self.mineTokensExCacheRead = mineTokensExCacheRead; self.mineBreakdown = mineBreakdown
            self.mineCost = mineCost; self.mineMessageCount = mineMessageCount; self.mineTurnCount = mineTurnCount
            self.spanTokens = spanTokens; self.spanCost = spanCost; self.otherTokens = otherTokens
            self.otherCost = otherCost; self.otherHasAssigned = otherHasAssigned
            self.otherHasExcluded = otherHasExcluded; self.otherHasUnattributed = otherHasUnattributed
            self.models = models
        }
    }
    public struct Diagnostics: Codable, Equatable, Sendable {
        public let intervals: [DiagnosticInterval]
        public let buckets: [Buckets]
        public init(intervals: [DiagnosticInterval] = [], buckets: [Buckets] = []) {
            self.intervals = intervals; self.buckets = buckets
        }
    }
    public struct Assigned: Codable, Equatable, Sendable {
        public let target: String
        public let counter: Counter
        public init(target: String, counter: Counter = Counter()) { self.target = target; self.counter = counter }
    }
    public struct DiagnosticComponents: Codable, Equatable, Sendable {
        public let inputOutputReasoning: Double
        public let inputOutputReasoningCacheWrite: Double
        public let incrementalInputOutputReasoning: Int64
        public let incrementalExCacheRead: Int64
        public let inputOutputReasoningTokens: Int64
        public let exCacheRead: Double
        public let full: Double
        public let input: Double
        public let output: Double
        public let cacheRead: Double
        public let cacheWrite: Double
        public let reasoning: Double
        public let cost: Double
        public init(inputOutputReasoning: Double = 0, inputOutputReasoningCacheWrite: Double = 0,
                    incrementalInputOutputReasoning: Int64 = 0, incrementalExCacheRead: Int64 = 0,
                    inputOutputReasoningTokens: Int64 = 0, exCacheRead: Double = 0, full: Double = 0,
                    input: Double = 0, output: Double = 0, cacheRead: Double = 0,
                    cacheWrite: Double = 0, reasoning: Double = 0, cost: Double = 0) {
            self.inputOutputReasoning = inputOutputReasoning; self.exCacheRead = exCacheRead; self.full = full
            self.inputOutputReasoningCacheWrite = inputOutputReasoningCacheWrite
            self.incrementalInputOutputReasoning = incrementalInputOutputReasoning
            self.incrementalExCacheRead = incrementalExCacheRead
            self.inputOutputReasoningTokens = inputOutputReasoningTokens
            self.input = input; self.output = output; self.cacheRead = cacheRead
            self.cacheWrite = cacheWrite; self.reasoning = reasoning; self.cost = cost
        }
    }
    public struct DiagnosticInterval: Codable, Equatable, Sendable {
        public let id: String
        public let all: Counter
        public let mine: Counter
        public let assigned: [Assigned]
        public let excluded: Counter
        public let unassigned: Counter
        public let components: DiagnosticComponents
        public init(id: String, all: Counter = Counter(), mine: Counter = Counter(), assigned: [Assigned] = [],
                    excluded: Counter = Counter(), unassigned: Counter = Counter(),
                    components: DiagnosticComponents = DiagnosticComponents()) {
            self.id = id; self.all = all; self.mine = mine; self.assigned = assigned
            self.excluded = excluded; self.unassigned = unassigned; self.components = components
        }
    }
    public struct Buckets: Codable, Equatable, Sendable {
        public let id: String
        public let tokens: [Int64]
        public let remainderTokens: Int64
        public init(id: String, tokens: [Int64], remainderTokens: Int64 = 0) {
            self.id = id; self.tokens = tokens; self.remainderTokens = remainderTokens
        }
    }

    public enum QueryError: Error { case incompleteCoverage, invalidSnapshot, missingJob }

    /// Treat partial coverage/missing jobs as failure, not as zero usage.
    public func validate(_ request: WindowUsageRequest, snapshot: WindowUsageBatch? = nil) throws {
        guard request.fromMs < request.untilMs,
              coverage.state == "complete", coverage.fromMs <= request.fromMs,
              coverage.untilMs >= request.untilMs else { throw QueryError.incompleteCoverage }
        guard !snapshotToken.isEmpty, request.snapshotToken == nil || request.snapshotToken == snapshotToken,
              !request.includeModelCatalog || modelCatalog != nil else { throw QueryError.invalidSnapshot }
        if let snapshot {
            guard snapshotToken == snapshot.snapshotToken, sourceGeneration == snapshot.sourceGeneration,
                  rootGeneration == snapshot.rootGeneration, priceGeneration == snapshot.priceGeneration
            else { throw QueryError.invalidSnapshot }
        }
        guard inference.count == request.inference.count, windows.count == request.windows.count,
              Set(inference.map(\.id)).count == inference.count,
              Set(windows.map(\.id)).count == windows.count else { throw QueryError.missingJob }
        for job in request.inference {
            guard let result = inference.first(where: { $0.id == job.id }), result.state == "covered",
                  result.firstAssignedMs.map({ $0 >= job.fromMs && $0 < job.untilMs }) ?? true
            else { throw QueryError.missingJob }
        }
        for job in request.windows {
            guard let result = windows.first(where: { $0.id == job.id }),
                  result.cycles.count == job.cycles.count,
                  Set(result.cycles.map(\.resetAtMs)).count == result.cycles.count,
                  (result.current != nil) == (job.current != nil) else { throw QueryError.missingJob }
            for cycle in job.cycles {
                guard result.cycles.contains(where: { $0.resetAtMs == cycle.resetAtMs && $0.state == "covered" })
                else { throw QueryError.missingJob }
            }
            if let current = job.current, let found = result.current {
                guard found.state == "covered", found.zones.count == current.zones.count,
                      Set(found.zones.map(\.index)) == Set(current.zones.map(\.index)),
                      (found.observed != nil) == (current.observed != nil)
                else { throw QueryError.missingJob }
            }
        }
        if let jobs = request.diagnostics {
            guard let results = diagnostics,
                  Set(results.intervals.map(\.id)) == Set(jobs.intervals.map(\.id)),
                  results.intervals.count == jobs.intervals.count,
                  Set(results.buckets.map(\.id)) == Set(jobs.buckets.map(\.id)),
                  results.buckets.count == jobs.buckets.count else { throw QueryError.missingJob }
            for job in jobs.buckets {
                guard results.buckets.first(where: { $0.id == job.id })?.tokens.count == Int(exactly: job.count)
                else { throw QueryError.missingJob }
            }
        } else if diagnostics != nil { throw QueryError.missingJob }
    }

    /// Empty local evidence for pure demo/test sources; this token has no native lease.
    public static func empty(for request: WindowUsageRequest) -> Self {
        Self(snapshotToken: request.snapshotToken ?? "demo-no-lease",
             modelCatalog: request.includeModelCatalog ? [] : nil,
             coverage: Coverage(fromMs: request.fromMs, untilMs: request.untilMs),
             inference: request.inference.map { Inference(id: $0.id) },
             windows: request.windows.map { job in
                 Window(id: job.id, current: job.current.map { current in
                     Current(observed: current.observed.map { _ in Observed() },
                             zones: current.zones.map { Zone(index: $0.index) })
                 }, cycles: job.cycles.map { Cycle(resetAtMs: $0.resetAtMs) })
             }, diagnostics: request.diagnostics.map { jobs in
                 Diagnostics(intervals: jobs.intervals.map { DiagnosticInterval(id: $0.id) },
                             buckets: jobs.buckets.map { Buckets(id: $0.id, tokens: Array(repeating: 0, count: max(0, Int(exactly: $0.count) ?? 0))) })
             })
    }
}

// Explicit nullable encoding preserves the wire distinction between no scope
// and a scoped catalog miss. Decoding remains ordinary Codable synthesis.
extension WindowUsageRequest {
    private enum WireKeys: String, CodingKey {
        case accountKey, fromMs, untilMs, snapshotToken, includeModelCatalog
        case attribution, modelGroups, inference, windows, diagnostics
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: WireKeys.self)
        try c.encode(accountKey, forKey: .accountKey)
        try c.encode(fromMs, forKey: .fromMs)
        try c.encode(untilMs, forKey: .untilMs)
        try c.encode(snapshotToken, forKey: .snapshotToken)
        try c.encode(includeModelCatalog, forKey: .includeModelCatalog)
        try c.encode(attribution, forKey: .attribution)
        try c.encode(modelGroups, forKey: .modelGroups)
        try c.encode(inference, forKey: .inference)
        try c.encode(windows, forKey: .windows)
        try c.encode(diagnostics, forKey: .diagnostics)
    }
}

extension WindowUsageRequest.Attribution {
    private enum WireKeys: String, CodingKey { case client, provider, model, state, target }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: WireKeys.self)
        try c.encode(client, forKey: .client)
        try c.encode(provider, forKey: .provider)
        try c.encode(model, forKey: .model)
        try c.encode(state, forKey: .state)
        try c.encode(target, forKey: .target)
    }
}

extension WindowUsageRequest.Inference {
    private enum WireKeys: String, CodingKey { case id, subscription, fromMs, untilMs }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: WireKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(subscription, forKey: .subscription)
        try c.encode(fromMs, forKey: .fromMs)
        try c.encode(untilMs, forKey: .untilMs)
    }
}

extension WindowUsageRequest.Window {
    private enum WireKeys: String, CodingKey { case id, subscription, admittedModelIds, current, cycles }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: WireKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(subscription, forKey: .subscription)
        try c.encode(admittedModelIds, forKey: .admittedModelIds)
        try c.encode(current, forKey: .current)
        try c.encode(cycles, forKey: .cycles)
    }
}

extension WindowUsageRequest.Current {
    private enum WireKeys: String, CodingKey { case fromMs, untilMs, observed, zones }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: WireKeys.self)
        try c.encode(fromMs, forKey: .fromMs)
        try c.encode(untilMs, forKey: .untilMs)
        try c.encode(observed, forKey: .observed)
        try c.encode(zones, forKey: .zones)
    }
}

extension WindowUsageRequest.DiagnosticInterval {
    private enum WireKeys: String, CodingKey {
        case id, fromMs, untilMs, includeFrom, includeUntil, subscription, admittedModelIds
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: WireKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(fromMs, forKey: .fromMs)
        try c.encode(untilMs, forKey: .untilMs)
        try c.encode(includeFrom, forKey: .includeFrom)
        try c.encode(includeUntil, forKey: .includeUntil)
        try c.encode(subscription, forKey: .subscription)
        try c.encode(admittedModelIds, forKey: .admittedModelIds)
    }
}
