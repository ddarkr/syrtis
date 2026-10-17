import Foundation

// Stacked-bar series for the Token Usage chart, ported from the grouping logic
// in the Tauri app's src/components/UsageBarGraph2D.tsx. UI-free so the
// builder is unit-testable; the SwiftUI chart just renders the result.

public enum StackBy: String, Sendable, CaseIterable {
    case model
    case agent
}

/// Whether bar length encodes token count or spend.
public enum ChartMetric: String, Sendable, CaseIterable {
    case tokens
    case cost
}

public struct DaySegment: Sendable {
    public let key: String
    public let label: String
    /// Hex color (provider shade for model stacking, brand color for agent).
    public let color: String
    public var tokens: Int64
    public var cost: Double
}

public struct DayBar: Sendable {
    public let date: String
    public var totalTokens: Int64 { segments.reduce(0) { $0.saturatingAdding($1.tokens) } }
    public var totalCost: Double { segments.reduce(0) { $0 + $1.cost } }
    public var segments: [DaySegment]
    public var isEmpty: Bool { segments.isEmpty }
}

public enum DayBars {
    public static let window = 30

    /// Build the trailing `window` calendar days ending today for all-time
    /// and current-year views, including empty days without token/cost activity.
    /// Other years retain the last selected metric day as their anchor;
    /// empty historical slices use the selected range end or year end.
    public static func build(
        payload: UsagePayload,
        clientIds: [String],
        stackBy: StackBy,
        colors: ModelColorMap,
        rangeEnd: String,
        year: String?,
        today: String
    ) -> [DayBar] {
        let allowed = Set(clientIds)
        // The anchor needs only selected metric presence, not model segments.
        // Eligible rows have at least one positive metric, so retain the same
        // grouped/saturating sums when deciding whether a historical day counts.
        func hasActivity(_ contribution: Contribution) -> Bool {
            var grouped: [String: (tokens: Int64, cost: Double)] = [:]
            for client in contribution.clients where allowed.contains(client.client) {
                let tokens = client.tokens.total
                guard tokens > 0 || client.cost > 0 else { continue }
                let key = stackBy == .model
                    ? (client.modelId.isEmpty ? "unknown" : ModelGrouping.groupID(client.modelId))
                    : client.client
                var slot = grouped[key] ?? (0, 0)
                slot.tokens = slot.tokens.saturatingAdding(tokens)
                slot.cost += client.cost
                grouped[key] = slot
            }
            var tokens: Int64 = 0
            var cost = 0.0
            for key in grouped.keys.sorted() {
                let slot = grouped[key]!
                tokens = tokens.saturatingAdding(slot.tokens)
                cost += slot.cost
            }
            return tokens > 0 || cost > 0
        }

        let end: String
        if let year, year != String(today.prefix(4)) {
            end = payload.contributions.lazy.filter { hasActivity($0) }.map(\.date).max()
                ?? (rangeEnd.isEmpty ? "\(year)-12-31" : rangeEnd)
        } else {
            end = today
        }
        guard let endDay = ISODay(end) else { return [] }
        let start = ISODay(number: endDay.number - (window - 1)).iso
        var byDate: [String: DayBar] = [:]
        for contribution in payload.contributions
            where contribution.date >= start && contribution.date <= end {
            let day = dayBar(contribution, allowed: allowed, stackBy: stackBy, colors: colors)
            if day.totalTokens > 0 || day.totalCost > 0 { byDate[day.date] = day }
        }
        return (0..<window).map { i in
            let date = ISODay(number: endDay.number - (window - 1) + i).iso
            return byDate[date] ?? DayBar(date: date, segments: [])
        }
    }

    static func dayBar(
        _ contribution: Contribution, allowed: Set<String>, stackBy: StackBy,
        colors: ModelColorMap
    ) -> DayBar {
        // Group each day either by model (tokscale-style provider shades) or
        // by agent/client (brand colors). Color + label follow the mode.
        var grouped: [String: DaySegment] = [:]
        for client in contribution.clients {
            guard allowed.contains(client.client) else { continue }
            let tokens = client.tokens.total
            if tokens <= 0 && client.cost <= 0 { continue }
            let model = client.modelId.isEmpty ? "unknown" : ModelGrouping.groupID(client.modelId)
            let key = stackBy == .model ? model : client.client
            var slot = grouped[key] ?? {
                switch stackBy {
                case .model:
                    return DaySegment(
                        key: key, label: model,
                        color: colors.color(client.providerId, model), tokens: 0, cost: 0)
                case .agent:
                    return DaySegment(
                        key: key, label: ClientRegistry.shortName(client.client),
                        color: ClientRegistry.style(client.client).color, tokens: 0, cost: 0)
                }
            }()
            slot.tokens = slot.tokens.saturatingAdding(tokens)
            slot.cost += client.cost
            grouped[key] = slot
        }
        // Stable stacking order across days: sort by key.
        return DayBar(
            date: contribution.date,
            segments: grouped.values.sorted { $0.key < $1.key })
    }

    /// Aggregate every segment across the visible window for the legend,
    /// heaviest-first by the active metric.
    public static func legend(bars: [DayBar], metric: ChartMetric) -> [DaySegment] {
        var agg: [String: DaySegment] = [:]
        for bar in bars {
            for seg in bar.segments {
                var slot = agg[seg.key]
                    ?? DaySegment(key: seg.key, label: seg.label, color: seg.color, tokens: 0, cost: 0)
                slot.tokens = slot.tokens.saturatingAdding(seg.tokens)
                slot.cost += seg.cost
                agg[seg.key] = slot
            }
        }
        return agg.values.sorted {
            metric == .cost ? $0.cost > $1.cost : $0.tokens > $1.tokens
        }
    }
}
