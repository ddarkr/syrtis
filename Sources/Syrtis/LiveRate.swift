import Foundation
import TokenBarCore

/// Shared rate policy. A compact per-client trace preserves dynamic clients
/// when hiding clients, without opening a separate ingestion lane.
enum LiveRate {
    /// Unscoped queries preserve unknown and dynamic unhidden client ids.
    /// A mixed rate is only usable unchanged when no client is hidden.
    /// Match the live-session card's ten-minute average, not the native one-minute default.
    static func query() -> LocalUsageScheduler.Query? {
        ClientRegistry.hiddenTabClients().isEmpty
            ? .init(kind: "rate", windowSecs: 600)
            : .init(kind: "trace", windowSecs: 600)
    }

    static func rate(
        from value: LocalUsageValue,
        hidden: Set<String> = ClientRegistry.hiddenTabClients()
    ) -> Double? {
        switch value {
        case let .rate(rate): return hidden.isEmpty ? rate : nil
        case let .trace(rows): return TraceBucket.totalRate(rows, hidden: hidden)
        default: return nil
        }
    }

    /// Diagnostic one-shot only; production rate demand uses its shared scheduler.
    @MainActor
    static func current(source: any UsageDataSource) async throws -> Double {
        guard let query = query() else { throw CocoaError(.coderInvalidValue) }
        let scheduler = LocalUsageScheduler(source: source, observesSources: false)
        let session = scheduler.openSession()
        defer { scheduler.release(session) }
        let value = try await scheduler.acquire(query, session: session)
        guard let rate = rate(from: value) else {
            throw CocoaError(.coderInvalidValue)
        }
        return rate
    }
}
