import SwiftUI
import TokenBarCore

/// The "Quota" lens: everything about a subscription's usage windows in one
/// place, rather than the current window on Overview and its history somewhere
/// else.
///
/// The two tabs answer different questions and so render different things.
/// Across all agents the question is comparative — which subscription is
/// tightest, which is burning fastest — and `AgentLimitsCard` already answers
/// it, one sparkline per window. On a single client the question is specific:
/// this window, then the windows before it.
struct QuotaView: View {
    /// The open client tab, or nil on the all-agent view.
    let singleClient: String?
    let clientIds: [String]
    let trace: [TraceBucket]
    let agentUsage: AgentUsagePayload?
    var usageAttempted = true
    /// Whether the stage-two usage scan settled with an error, so the history
    /// card can say so instead of spinning under rows that are already drawn.
    var scanFailed = false
    var curveUnreadable = false
    /// Quota readings per `"<clientId>|<cardId>"`, for the sparklines.
    var windowCurves: [String: [QuotaSample]] = [:]
    /// The selected window's card state on a single-client tab.
    var windowCard: WindowCardState?
    /// The open client's accounts: pill row, header label, and which local-usage
    /// line the card and its history show. Nil when no account has live windows.
    var accountContext: CardAccountContext?
    /// Called with the account the reader picked (nil = primary).
    var onSelectAccount: (String?) -> Void = { _ in }
    /// Recorded reset cycles of that window, newest first.
    var quotaCycles: [QuotaCycle] = []
    /// Those cycles joined to local usage; empty while the scan is out.
    var quotaHistory: [QuotaHistoryRow] = []
    /// The cycles above belong to another account or window than the card
    /// shows now (an account pick before its refresh landed); draw loading.
    var historyPending = false
    /// Shared model palette, so a model keeps one colour across the app.
    var colors: ModelColorMap = ModelColorMap(entries: [])
    /// Daily spend stacked by declared subscription, for the all-agent view.
    /// Nil until the graph payload has been folded.
    /// Recorded-cycle strips for every displayed window.
    var windowSummaries: [QuotaWindowSummary] = []
    /// Clients with an unread window the strip has nothing to draw for.
    var stripUnreadableClients: Set<String> = []
    /// Weekday-by-hour consumption per window, keyed as the window's id.
    var heatmaps: [String: QuotaHeatmap] = [:]
    /// Which windows have a grid at all. Not derived from `windowSummaries`.
    var heatmapWindows: [QuotaHeatmapWindow] = []
    var equivalences: [String: WindowEquivalence.Row] = [:]
    var trend: SubscriptionTrend?

    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true

    var body: some View {
        VStack(spacing: NativeDesign.spacing) {
            if let singleClient {
                if let windowCard {
                    // Above its siblings: a `zIndex` set inside the card orders
                    // that card's children, not the card among these.
                    WindowUsageCard(
                        state: windowCard, account: accountContext,
                        onSelectAccount: onSelectAccount).zIndex(1)
                }
                if limitsEnabled {
                    AgentLimitsCard(
                        clients: clientIds, trace: trace, agentUsage: agentUsage,
                        usageAttempted: usageAttempted,
                        title: "%@ limits".localized(
                            ClientRegistry.tabDisplayName(singleClient)),
                        note: "Session / weekly / model limits",
                        restrict: true, curves: windowCurves)
                }
                if windowCard == nil {
                    // No window card to list a history for: a Bot-only Grok
                    // install, or a tab whose client reports no quota. These
                    // folds need quota history only. A grouped tab with a
                    // window card (Grok Build & Bot, Antigravity) takes the
                    // window history below, the same card as every other
                    // client tab; its other member's windows stay on the
                    // all-agent Quota lens.
                    QuotaHistoryStripCard(
                        summaries: windowSummaries.filter { clientIds.contains($0.clientId) },
                        equivalences: equivalences, attempted: usageAttempted,
                        unreadable: !stripUnreadableClients.isDisjoint(with: clientIds))
                    QuotaHeatmapCard(
                        windows: heatmapWindows.filter { clientIds.contains($0.clientId) },
                        heatmaps: heatmaps, equivalences: equivalences,
                        attempted: usageAttempted,
                        unreadable: !stripUnreadableClients.isDisjoint(with: clientIds))
                } else {
                    QuotaHistoryCard(
                        clientId: singleClient, cycles: quotaCycles,
                        rows: quotaHistory, colors: colors,
                        attempted: usageAttempted && !historyPending,
                        scanFailed: scanFailed, curveUnreadable: curveUnreadable,
                        account: accountContext)
                        // The card holds per-window state — how many rows the
                        // reader has grown the list to, and which row is open — and
                        // switching windows inside one client does not by itself
                        // rebuild it. Keyed on the RESOLVED window rather than the
                        // stored preference: a choice saved for another client does
                        // not move this one's window, and a window vanishing from
                        // the payload moves it without the preference changing.
                        // Same resolution the cycle list itself went through, so
                        // the key cannot name a window other than the one the rows
                        // came from.
                        // The account is part of the identity (history vocabulary:
                        // three-part for a non-primary account), so switching
                        // account rebuilds the card instead of keeping the
                        // other account's expanded rows.
                        .id(WindowCardLoader.historyCardId(
                            payload: agentUsage, clientId: singleClient,
                            accountKey: accountContext?.resolved))
                }
            } else {
                // Trend first: it answers "where is my spend going" across
                // subscriptions, which the window-by-window card below cannot.
                SubscriptionTrendCard(trend: trend)
                QuotaHistoryStripCard(
                    summaries: windowSummaries, equivalences: equivalences,
                    attempted: usageAttempted,
                    unreadable: !stripUnreadableClients.isEmpty)
                // After the strip, not before: the strip says how much each
                // window consumed, and this says when. "When" is only a
                // question once "how much" has an answer.
                QuotaHeatmapCard(
                    windows: heatmapWindows, heatmaps: heatmaps,
                    equivalences: equivalences, attempted: usageAttempted,
                    unreadable: !stripUnreadableClients.isEmpty)
                if limitsEnabled {
                    AgentLimitsCard(
                    clients: clientIds, trace: trace, agentUsage: agentUsage,
                        usageAttempted: usageAttempted,
                        reorderable: true, curves: windowCurves)
                }
            }
        }
    }
}
