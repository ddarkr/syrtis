import CTB
import Foundation

public enum DiskUsageAction: String, CaseIterable, Sendable {
    case inspect, clearCache, optimize, resetUsage
}

/// Allocated file bytes, not logical SQLite table sizes or source-log sizes.
public struct DiskUsageReport: Decodable, Sendable {
    public let usageDirectory: String
    public let usageBytes: UInt64
    public let recoveryBytes: UInt64
    public let cacheBytes: UInt64
    public let otherBytes: UInt64
    public let clients: [DiskUsageClient]
    public let measuredAtMs: Int64

    public var totalBytes: UInt64 {
        [usageBytes, recoveryBytes, cacheBytes, otherBytes].reduce(0, Self.adding)
    }

    public var recordCount: UInt64 {
        clients.reduce(0) { Self.adding($0, $1.recordCount) }
    }

    private static func adding(_ lhs: UInt64, _ rhs: UInt64) -> UInt64 {
        let (value, overflow) = lhs.addingReportingOverflow(rhs)
        return overflow ? .max : value
    }
}

public struct DiskUsageClient: Decodable, Sendable {
    public let client: String
    public let recordCount: UInt64
    public let firstTimestampMs: Int64?
    public let lastTimestampMs: Int64?
}

extension TBCore {
    /// Local maintenance only: no log ingestion or provider requests.
    public static func diskUsage(_ action: DiskUsageAction = .inspect) async throws -> DiskUsageReport {
        try await Task.detached(priority: .utility) {
            try "{\"action\":\"\(action.rawValue)\"}".withCString { request in
                try unwrap(tb_disk_usage(request))
            }
        }.value
    }
}
