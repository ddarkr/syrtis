import CTB
import Foundation

/// Exact producing scope. A filtered report cannot reuse a broader ownership fold.
public struct LocalUsageScope: Codable, Hashable, Sendable {
    public let id: String
    public let kind: String
    public let clients: [String]?
    public let clientId: String?
    public let accountKey: String?

    public init(
        id: String, kind: String, clients: [String]? = nil,
        clientId: String? = nil, accountKey: String? = nil
    ) {
        self.id = id
        self.kind = kind
        self.clients = clients
        self.clientId = clientId
        self.accountKey = accountKey
    }
}

public struct LocalUsageSyncRequest: Codable, Sendable {
    public let scopes: [LocalUsageScope]
    public let previousLeaseToken: String?
    public let force: Bool
    public let verifyTopology: Bool
    public let refreshPricing: Bool

    public init(
        scopes: [LocalUsageScope], previousLeaseToken: String? = nil,
        force: Bool = false, verifyTopology: Bool = true, refreshPricing: Bool = false
    ) {
        self.scopes = scopes
        self.previousLeaseToken = previousLeaseToken
        self.force = force
        self.verifyTopology = verifyTopology
        self.refreshPricing = refreshPricing
    }
}

public struct LocalSourceWatchRoot: Codable, Hashable, Sendable {
    public let sourceRootURL: String
    public let watchRootURL: String
    public let isDirectory: Bool

    public init(sourceRootURL: String, watchRootURL: String, isDirectory: Bool) {
        self.sourceRootURL = sourceRootURL
        self.watchRootURL = watchRootURL
        self.isDirectory = isDirectory
    }
}

/// A reserved writer basename grammar, only beneath its exact owned parent.
public struct LocalSourceOwnedWriteRule: Codable, Hashable, Sendable {
    public let parentURL: String
    public let pattern: String

    public init(parentURL: String, pattern: String) {
        self.parentURL = parentURL
        self.pattern = pattern
    }
}

public struct LocalSourceWatchManifest: Codable, Sendable {
    public let roots: [LocalSourceWatchRoot]
    public let ignoredFileURLs: [String]
    public let ignoredBasenameRules: [LocalSourceOwnedWriteRule]

    public init(
        roots: [LocalSourceWatchRoot], ignoredFileURLs: [String],
        ignoredBasenameRules: [LocalSourceOwnedWriteRule] = []
    ) {
        self.roots = roots
        self.ignoredFileURLs = ignoredFileURLs
        self.ignoredBasenameRules = ignoredBasenameRules
    }
}

public struct LocalUsageScopeSnapshot: Codable, Sendable {
    public let id: String
    public let sourceGeneration: UInt64
    public let quotaSnapshotToken: String?

    public init(id: String, sourceGeneration: UInt64, quotaSnapshotToken: String? = nil) {
        self.id = id
        self.sourceGeneration = sourceGeneration
        self.quotaSnapshotToken = quotaSnapshotToken
    }
}

public struct LocalUsageSyncResult: Codable, Sendable {
    public let leaseToken: String
    public let rootGeneration: UInt64
    public let priceGeneration: String
    public let verifiedAtMs: Int64
    public let scopes: [LocalUsageScopeSnapshot]
    public let watchManifest: LocalSourceWatchManifest
    public let changed: Bool

    public init(
        leaseToken: String, rootGeneration: UInt64, priceGeneration: String,
        verifiedAtMs: Int64, scopes: [LocalUsageScopeSnapshot],
        watchManifest: LocalSourceWatchManifest, changed: Bool
    ) {
        self.leaseToken = leaseToken
        self.rootGeneration = rootGeneration
        self.priceGeneration = priceGeneration
        self.verifiedAtMs = verifiedAtMs
        self.scopes = scopes
        self.watchManifest = watchManifest
        self.changed = changed
    }

    func validate(_ request: LocalUsageSyncRequest) throws {
        let expected = Set(request.scopes.map(\.id))
        let received = Set(scopes.map(\.id))
        guard !leaseToken.isEmpty, !priceGeneration.isEmpty,
              expected == received, received.count == scopes.count,
              expected.count == request.scopes.count else {
            throw TBCoreError.bridge("local usage synchronization returned a different scope")
        }
        for scope in request.scopes {
            guard let snapshot = scopes.first(where: { $0.id == scope.id }),
                  (scope.kind == "quota") == (snapshot.quotaSnapshotToken != nil) else {
                throw TBCoreError.bridge("local usage synchronization omitted its quota lease")
            }
        }
    }
}

public struct LocalUsageQueryRequest: Codable, Hashable, Sendable {
    public let leaseToken: String
    public let scopeId: String
    public let kind: String
    public let year: String?
    public let clients: [String]?
    public let windowSecs: Int64?
    public let nowMs: Int64?

    public init(
        leaseToken: String, scopeId: String, kind: String,
        year: String? = nil, clients: [String]? = nil,
        windowSecs: Int64? = nil, nowMs: Int64? = nil
    ) {
        self.leaseToken = leaseToken
        self.scopeId = scopeId
        self.kind = kind
        self.year = year
        self.clients = clients
        self.windowSecs = windowSecs
        self.nowMs = nowMs
    }
}

extension TBCore {
    /// The only source-work operation; pure query calls never enter this lane.
    public static func localUsageSync(
        _ request: LocalUsageSyncRequest
    ) async throws -> LocalUsageSyncResult {
        try await Task.detached(priority: .utility) {
            var bytes = try JSONEncoder().encode(request)
            bytes.append(0)
            let result: LocalUsageSyncResult = try bytes.withUnsafeBytes { buffer in
                try unwrap(tb_local_usage_sync(buffer.baseAddress!.assumingMemoryBound(to: CChar.self)))
            }
            do {
                try result.validate(request)
            } catch {
                if result.leaseToken != request.previousLeaseToken {
                    try? result.leaseToken.withCString { token in
                        let _: LocalUsageReleased = try unwrap(tb_local_usage_release(token))
                    }
                }
                throw error
            }
            return result
        }.value
    }

    public static func localUsageQuery<T: Decodable & Sendable>(
        _ request: LocalUsageQueryRequest, as type: T.Type
    ) async throws -> T {
        try await Task.detached(priority: .utility) {
            var bytes = try JSONEncoder().encode(request)
            bytes.append(0)
            return try bytes.withUnsafeBytes { buffer -> T in
                try unwrap(tb_local_usage_query(buffer.baseAddress!.assumingMemoryBound(to: CChar.self)))
            }
        }.value
    }

    public static func releaseLocalUsage(_ leaseToken: String) async throws {
        try await Task.detached(priority: .utility) {
            try leaseToken.withCString { token in
                let _: LocalUsageReleased = try unwrap(tb_local_usage_release(token))
            }
        }.value
    }
}

private struct LocalUsageReleased: Decodable {
    let released: Bool
}
