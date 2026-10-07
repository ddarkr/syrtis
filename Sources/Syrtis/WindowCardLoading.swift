import Foundation
import TokenBarCore

/// The card loads in two halves because they cost four orders of magnitude
/// apart: reading the persisted quota samples is ~2ms, scanning local usage is
/// 6-12s. Loading them together made the whole card wait for the slow one --
/// and worse, both waited on a network fetch neither of them needs.

/// Stage 1. Everything derivable without a scan: the window, the line, the
/// headline. Needs an `AgentUsagePayload` that is already in memory; it never
/// waits on one being fetched.
struct WindowQuotaHalf: Sendable {
    let clientId: String
    let cardId: String
    let windowLabel: String
    let candidates: [(cardId: String, label: String)]
    let resolution: WindowResolution
    let samples: [QuotaSample]
    /// The provider anchor this half was resolved against. Carried so stage 2
    /// can re-resolve `.idle` with usage in hand without re-reading the payload.
    let resetMs: Int64?
    let durationMs: Int64?
    /// True when stage 1 resolved `.idle` — which the scan may still refine to
    /// `.inferred`. Shown as pending rather than as "no window running", so the
    /// card cannot state one answer and then replace it with another.
    let placementPending: Bool
    let nowMs: Int64
    /// The provider-declared model scope of this window, carried from stage 1
    /// so stage 2 filters the usage it displays to the model the allowance is
    /// actually about. Nil for every window the provider did not narrow.
    let modelScope: String?
}

/// Stage 2. Compact native counters and Swift-owned presentation geometry.
struct WindowUsageHalf: Sendable {
    let counter: WindowUsageBatch.Counter
    let observed: WindowUsageBatch.Observed?
    let zones: [WindowUsageBatch.Zone]
    let bars: [BarRect]
    let hits: [HitZone]
    /// Rows the scan could not place in time — a property of the SCAN, not of
    /// this card. It is the same number on every card by construction, and must
    /// be presented as one: an undated row has no timestamp, so it belongs to
    /// no window and cannot be attributed to one subscription's totals rather
    /// than another's. Carried so the card can disclose the omission, never so
    /// it can claim the omission was its own.
    var undatedCount: Int = 0
    /// The window declared a model scope and NOTHING in this subscription's
    /// usage matched it, while unscoped usage exists.
    ///
    /// Carried because the scope is a join between the provider's display name
    /// and the transcript's model id, and a join that finds nothing looks
    /// exactly like a week with no work in it. Drawing empty bars under a
    /// moving curve would state a fact about the user's week that this card
    /// cannot support; saying the scope matched nothing states what happened.
    var scopeMatchedNothing: Bool = false
}

/// What the card is, at any moment. Five cases, exhaustive by construction:
/// there is no "absent", because absence is what this slice exists to remove.
enum WindowCardState: Sendable {
    /// Which window this state is about, when it is about one.
    ///
    /// Retention decisions have to be made on the identity the CARD shows, not
    /// on the client: two window selections share a client, so keeping "the
    /// card we already had for this client" across a transient read failure
    /// could leave the chart on the window the user just navigated away from
    /// while the picker highlighted the new one.
    var cardId: String? {
        switch self {
        case let .noQuotaHistory(_, _, _, cardId): return cardId
        // `WindowQuotaHalf.cardId` is already `"<clientId>|<cardId>"`; building
        // the pair again here produced `codex|codex|session.v1`, which matched
        // nothing and quietly disabled the retention it was added for.
        case let .quotaOnly(half, _): return half.cardId
        case let .ready(half, _): return half.cardId
        case .loading, .blocked: return nil
        }
    }

    /// No quota payload has arrived yet, not even a failed one.
    case loading
    /// A provider reported an error instead of windows.
    case blocked(clientId: String, reason: String)
    /// The payload arrived but this window has no recorded quota history — a
    /// terminal answer, not a slow one. `copilot/chat.v1` is this case, and
    /// mapping it to `loading` would spin forever.
    case noQuotaHistory(clientId: String, windowLabel: String,
                        candidates: [(cardId: String, label: String)], cardId: String)
    /// Window placed; the usage half is not in.
    ///
    /// `scanFailed` separates "still scanning" from "asked and failed". Without
    /// it a persistently failing scan rendered identically to a slow one, so
    /// the card sat under a drawn chart saying "Reading local usage…" forever.
    case quotaOnly(WindowQuotaHalf, scanFailed: Bool)
    /// Both halves in.
    case ready(WindowQuotaHalf, WindowUsageHalf)
}

extension WindowCardState {
    var quotaHalf: WindowQuotaHalf? {
        switch self {
        case let .quotaOnly(q, _), let .ready(q, _): return q
        case .loading, .blocked, .noQuotaHistory: return nil
        }
    }

    var usageHalf: WindowUsageHalf? {
        if case let .ready(_, u) = self { return u }
        return nil
    }
}
