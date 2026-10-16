import SwiftUI
import TokenBarCore

/// "Agents by cost" lens, port of AgentsView.tsx: named sub-agents (plus a
/// "Main" bucket for unattributed messages) ranked by cost, showing the
/// source clients they ran under, message count, total tokens, and cost.
struct AgentsView: View {
    let report: AgentsReport?
    /// Restrict to agents that ran under these clients; empty selects none.
    var clientIds: [String] = []

    var body: some View {
        let allow = Set(clientIds)
        let rows = (report?.entries ?? [])
            .filter { $0.clients.contains { allow.contains($0) } }
        let totalCost = rows.reduce(0) { $0 + $1.cost }
        let maxCost = max(rows.map(\.cost).max() ?? 1, 0.000001)

        DashCard(
            "Agents by cost",
            trailing: {
                Text((rows.count == 1 ? "%lld agent · %@" : "%lld agents · %@")
                    .localized(rows.count, Format.usd(totalCost)))
                    .font(NativeDesign.detailFont)
                    .foregroundStyle(.secondary)
            }
        ) {
            if report == nil {
                LoadingLine(title: "Loading…")
            } else if rows.isEmpty {
                Text("No agent activity in this range")
                    .font(NativeDesign.bodyFont)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 10) {
                    ForEach(rows, id: \.agent) { entry in
                        row(entry, totalCost: totalCost, maxCost: maxCost)
                    }
                }
            }
        }
    }

    private func row(_ entry: AgentReportEntry, totalCost: Double, maxCost: Double) -> some View {
        let share = totalCost > 0 ? entry.cost / totalCost * 100 : 0
        let sources = entry.clients.map { ClientRegistry.style($0).displayName }
            .joined(separator: " · ")
        return VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(entry.agent)
                    .font(NativeDesign.bodyFont)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(entry.agent)
                Spacer(minLength: 8)
                Text(String(format: "%.1f%%", share))
                    .font(NativeDesign.detailFont.monospacedDigit())
                    .foregroundStyle(.tertiary)
            }
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 2)
                        .fill(.quaternary.opacity(0.5))
                    RoundedRectangle(cornerRadius: 2)
                        .fill(Color.accentColor.opacity(0.7))
                        .frame(width: geo.size.width * CGFloat(entry.cost / maxCost))
                }
            }
            .frame(height: 6)
            HStack(spacing: 4) {
                Text(sources)
                    .font(NativeDesign.detailFont)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .help(sources)
                Spacer(minLength: 8)
                Text("%@ msgs".localized(entry.messages.formatted()) + " · \(Format.compactTokens(entry.total)) · \(Format.money(tokens: entry.total, cost: entry.cost))")
                    .font(NativeDesign.detailFont.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .layoutPriority(1)
            }
        }
    }
}
