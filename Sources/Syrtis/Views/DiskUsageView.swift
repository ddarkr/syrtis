import AppKit
import SwiftUI
import TokenBarCore

struct DiskUsageView: View {
    var perform: (@MainActor (DiskUsageAction) async throws -> DiskUsageReport)?

    @State private var report: DiskUsageReport?
    @State private var busy = false
    @State private var failure: String?
    @State private var outcome: String?
    @State private var confirmation: DiskUsageAction?
    @State private var recordsExpanded = false

    private struct Category: Identifiable {
        let id: String
        let bytes: UInt64
        let color: Color
    }

    private func categories(_ report: DiskUsageReport) -> [Category] {
        var rows = [
            Category(id: "Disk.Records", bytes: report.usageBytes, color: .blue),
            Category(id: "Disk.Recovery", bytes: report.recoveryBytes, color: .orange),
            Category(id: "Disk.Cache", bytes: report.cacheBytes, color: .purple),
        ]
        if report.otherBytes > 0 {
            rows.append(Category(id: "Disk.Other", bytes: report.otherBytes, color: .secondary))
        }
        return rows
    }

    var body: some View {
        Section {
            if let report {
                VStack(alignment: .leading, spacing: 12) {
                    Text(bytes(report.totalBytes))
                        .font(.title.bold())
                        .accessibilityLabel("Disk.Total".localized + ": " + exactBytes(report.totalBytes))
                    Text("Disk.Clearable".localized(bytes(report.cacheBytes)))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    Text("Disk.Allocated".localized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if report.totalBytes == 0 {
                        Text("Disk.Empty".localized).foregroundStyle(.secondary)
                    } else {
                        composition(report)
                    }
                }
                ForEach(categories(report)) { category in
                    categoryRow(category, total: report.totalBytes)
                }
                DisclosureGroup(isExpanded: $recordsExpanded) {
                    if report.clients.isEmpty {
                        Text("Disk.NoRecords".localized).foregroundStyle(.secondary)
                    }
                    ForEach(Array(report.clients.enumerated()), id: \.offset) { _, client in
                        LabeledContent(ClientRegistry.style(client.client).displayName) {
                            VStack(alignment: .trailing, spacing: 3) {
                                Text("Disk.RecordCount".localized(Int64(clamping: client.recordCount)))
                                Text(dateRange(client))
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                    Text("Disk.SharedBytes".localized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } label: {
                    Text("Disk.Collected".localized(Int64(clamping: report.recordCount)))
                }
                LabeledContent("Disk.Measured".localized) {
                    Text(date(report.measuredAtMs).formatted(date: .abbreviated, time: .standard))
                        .foregroundStyle(.secondary)
                }
                LabeledContent("Disk.Directory".localized) {
                    VStack(alignment: .trailing, spacing: 6) {
                        Text(report.usageDirectory)
                            .textSelection(.enabled)
                            .font(.caption)
                        Button("Disk.Finder".localized) {
                            NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: report.usageDirectory)
                        }
                        .disabled(!FileManager.default.fileExists(atPath: report.usageDirectory))
                    }
                }
            } else if perform == nil {
                Text("Disk.Unavailable".localized).foregroundStyle(.secondary)
            } else if failure == nil {
                Text("Disk.Loading".localized).foregroundStyle(.secondary)
            }
            if busy {
                HStack {
                    ProgressView().controlSize(.small)
                    Text("Disk.Working".localized).foregroundStyle(.secondary)
                }
            }
            if let failure {
                VStack(alignment: .leading, spacing: 5) {
                    Label((report == nil ? "Disk.Failed" : "Disk.Stale").localized,
                          systemImage: "exclamationmark.triangle")
                    Text(failure).font(.caption).textSelection(.enabled)
                }
                .foregroundStyle(.secondary)
            }
            if let outcome {
                Text(outcome).font(.callout).textSelection(.enabled)
            }
            Button((failure == nil ? "Disk.Refresh" : "Disk.Retry").localized) {
                run(.inspect)
            }
            .disabled(busy || perform == nil)
        } header: {
            Text("Disk Usage".localized)
        }

        Section {
            Text("Disk.Preserve".localized).font(.callout).foregroundStyle(.secondary)
            Button("Disk.Clear".localized) { confirmation = .clearCache }
            Button("Disk.Optimize".localized) { run(.optimize) }
            Text("Disk.OptimizeHint".localized).font(.caption).foregroundStyle(.secondary)
        } header: {
            Text("Disk.Management".localized)
        }
        .disabled(busy || perform == nil || report == nil)

        Section {
            Text("Disk.ResetHint".localized).font(.callout).foregroundStyle(.secondary)
            Button("Disk.Reset".localized, role: .destructive) { confirmation = .resetUsage }
        } header: {
            Text("Disk.ResetSection".localized)
        }
        .disabled(busy || perform == nil || report == nil)
        .confirmationDialog(
            (confirmation == .resetUsage ? "Disk.ResetQuestion" : "Disk.ClearQuestion").localized,
            isPresented: Binding(get: { confirmation != nil }, set: { if !$0 { confirmation = nil } }),
            titleVisibility: .visible
        ) {
            if let action = confirmation {
                Button((action == .resetUsage ? "Disk.Reset" : "Disk.Clear").localized,
                       role: action == .resetUsage ? .destructive : nil) {
                    confirmation = nil
                    run(action)
                }
            }
            Button("Cancel".localized, role: .cancel) { confirmation = nil }
        } message: {
            Text((confirmation == .resetUsage ? "Disk.ResetWarning" : "Disk.ClearWarning").localized)
        }
        .task {
            guard report == nil, !busy, perform != nil else { return }
            await execute(.inspect)
        }
    }

    private func composition(_ report: DiskUsageReport) -> some View {
        GeometryReader { geometry in
            HStack(spacing: 0) {
                ForEach(categories(report)) { category in
                    if category.bytes > 0 {
                        Rectangle()
                            .fill(category.color)
                            .frame(width: geometry.size.width * Double(category.bytes) / Double(report.totalBytes))
                            .overlay(Rectangle().strokeBorder(.background, lineWidth: 1))
                            .help(categoryDescription(category, total: report.totalBytes))
                    }
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 4))
        }
        .frame(height: 16)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Disk.Composition".localized)
        .accessibilityValue(categories(report).map { categoryDescription($0, total: report.totalBytes) }.joined(separator: "; "))
    }

    private func categoryRow(_ category: Category, total: UInt64) -> some View {
        HStack {
            Circle().fill(category.color).frame(width: 8, height: 8).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(category.id.localized)
                Text((category.id + "Hint").localized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Text(bytes(category.bytes)).monospacedDigit()
            Text(percentage(category.bytes, total: total))
                .foregroundStyle(.secondary).monospacedDigit()
        }
        .help(categoryDescription(category, total: total))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(category.id.localized)
        .accessibilityValue(exactBytes(category.bytes) + ", " + percentage(category.bytes, total: total))
        .focusable()
    }

    private func categoryDescription(_ category: Category, total: UInt64) -> String {
        category.id.localized + ": " + exactBytes(category.bytes) + ", " + percentage(category.bytes, total: total)
    }

    private func bytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .file)
    }

    private func exactBytes(_ value: UInt64) -> String {
        "Disk.Bytes".localized(value.formatted())
    }

    private func percentage(_ value: UInt64, total: UInt64) -> String {
        (total == 0 ? 0 : Double(value) / Double(total)).formatted(.percent.precision(.fractionLength(1)))
    }

    private func date(_ milliseconds: Int64) -> Date {
        Date(timeIntervalSince1970: Double(milliseconds) / 1_000)
    }

    private func dateRange(_ client: DiskUsageClient) -> String {
        guard let first = client.firstTimestampMs, let last = client.lastTimestampMs else {
            return "Disk.UnknownDates".localized
        }
        return date(first).formatted(date: .abbreviated, time: .omitted)
            + " – " + date(last).formatted(date: .abbreviated, time: .omitted)
    }

    private func run(_ action: DiskUsageAction) {
        guard !busy else { return }
        // Reserve before starting the task so repeated clicks cannot enqueue operations.
        busy = true
        Task { await execute(action) }
    }

    @MainActor
    private func execute(_ action: DiskUsageAction) async {
        guard let perform else { return }
        busy = true
        failure = nil
        outcome = nil
        let previous = report
        defer { busy = false }
        do {
            let measured = try await perform(action)
            report = measured
            if action != .inspect {
                let title = (action == .clearCache ? "Disk.Cleared" : action == .optimize ? "Disk.Optimized" : "Disk.ResetDone").localized
                if let previous {
                    outcome = title + " " + "Disk.Outcome".localized(exactBytes(previous.totalBytes), exactBytes(measured.totalBytes))
                } else {
                    outcome = title
                }
            }
        } catch {
            if let coreError = error as? TBCoreError, case .bridge(let message) = coreError {
                failure = message
            } else {
                failure = error.localizedDescription
            }
        }
    }
}
