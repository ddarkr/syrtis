import SwiftUI
import TokenBarCore

private struct PopoverScrollViewportKey: EnvironmentKey {
    static let defaultValue: CGRect? = nil
}

extension EnvironmentValues {
    var popoverScrollViewport: CGRect? {
        get { self[PopoverScrollViewportKey.self] }
        set { self[PopoverScrollViewportKey.self] = newValue }
    }
}

enum PopoverTooltipPlacement {
    static let edgeInset: CGFloat = 4
    static let cursorGap: CGFloat = 12
    /// Local Y ratio inside the source container. Above this, prefer placing
    /// the tooltip above the cursor (dodge); below it, prefer below. Matches
    /// the pre-viewport chart dodge (`chartHeight * 0.45`) and Models feel.
    static let preferAboveRatio: CGFloat = 0.45

    /// Position a tooltip in its local overlay while keeping it inside the
    /// visible ScrollView viewport. Prefers the side that dodges the cursor
    /// (region-based), then falls back / clamps so it never sits under the
    /// footer. Container and viewport share global coordinates; the returned
    /// offset is local to the container.
    static func offset(
        anchor: CGPoint,
        tooltipSize: CGSize,
        containerFrame: CGRect,
        viewport: CGRect?
    ) -> CGSize? {
        guard tooltipSize.width > 0, tooltipSize.height > 0 else { return nil }

        let anchorGlobal = CGPoint(
            x: containerFrame.minX + anchor.x,
            y: containerFrame.minY + anchor.y)

        // Freshness: continuous hover only fires over the *visible* scroll
        // area, so a live viewport must contain the anchor (tiny slack for
        // float edges). Mere container intersection is not enough — a
        // pre-resize short viewport can still overlap the card while the
        // newly exposed hover point sits below it, inventing a fake dodge.
        let viewport: CGRect = {
            guard let candidate = viewport, !candidate.isEmpty else { return containerFrame }
            let slack: CGFloat = 2
            if candidate.insetBy(dx: -slack, dy: -slack).contains(anchorGlobal) {
                return candidate
            }
            return containerFrame
        }()
        guard !viewport.isEmpty else { return nil }
        let visible = viewport.insetBy(dx: edgeInset, dy: edgeInset)
        guard visible.width > 0, visible.height > 0 else { return nil }
        let horizontalMin = max(containerFrame.minX, visible.minX)
        let horizontalMax = min(containerFrame.maxX, visible.maxX)
        guard horizontalMax > horizontalMin else { return nil }

        let maxX = horizontalMax - tooltipSize.width
        let originX = maxX >= horizontalMin
            ? min(max(anchorGlobal.x - tooltipSize.width / 2, horizontalMin), maxX)
            : horizontalMin

        let minY = visible.minY
        let maxY = visible.maxY - tooltipSize.height
        let belowY = anchorGlobal.y + cursorGap
        let aboveY = anchorGlobal.y - tooltipSize.height - cursorGap
        // Region dodge uses the source container (chart / rows), not the
        // full scroll viewport — "below if the viewport still has room"
        // always wins on a tall popover and feels like sticky follow.
        let preferBelow = containerFrame.height > 0
            ? anchor.y < containerFrame.height * preferAboveRatio
            : true
        let originY: CGFloat
        if tooltipSize.height >= visible.height {
            originY = minY
        } else if preferBelow {
            if belowY <= maxY {
                originY = max(belowY, minY)
            } else if aboveY >= minY {
                originY = min(aboveY, maxY)
            } else {
                originY = clampedPreferredY(
                    belowY: belowY, aboveY: aboveY,
                    anchorGlobalY: anchorGlobal.y,
                    minY: minY, maxY: maxY, visible: visible)
            }
        } else if aboveY >= minY {
            originY = min(aboveY, maxY)
        } else if belowY <= maxY {
            originY = max(belowY, minY)
        } else {
            originY = clampedPreferredY(
                belowY: belowY, aboveY: aboveY,
                anchorGlobalY: anchorGlobal.y,
                minY: minY, maxY: maxY, visible: visible)
        }

        return CGSize(
            width: originX - containerFrame.minX,
            height: originY - containerFrame.minY)
    }

    private static func clampedPreferredY(
        belowY: CGFloat,
        aboveY: CGFloat,
        anchorGlobalY: CGFloat,
        minY: CGFloat,
        maxY: CGFloat,
        visible: CGRect
    ) -> CGFloat {
        let belowSpace = visible.maxY - anchorGlobalY
        let aboveSpace = anchorGlobalY - visible.minY
        let preferred = belowSpace >= aboveSpace ? belowY : aboveY
        return min(max(preferred, minY), maxY)
    }
}

/// Readable content surface beneath the native glass shell: a quiet adaptive
/// fill, not nested glass (glass nested inside the panel's glass rendered as
/// a murky dark blob). Glass lives on the chrome (panel, navigation), never
/// as a fake glow on rows. Signature stable; macOS 14-safe, no glassEffect.
struct GlassCardBackground: ViewModifier {
    var cornerRadius: CGFloat = 10

    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.colorSchemeContrast) private var colorSchemeContrast
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.inGlassPanel) private var inGlassPanel

    func body(content: Content) -> some View {
        content
            .background(cardFill, in: RoundedRectangle(cornerRadius: cornerRadius))
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius)
                    .strokeBorder(borderFill, lineWidth: colorSchemeContrast == .increased ? 1 : 0.5))
    }

    private var cardFill: Color {
        if reduceTransparency || colorSchemeContrast == .increased {
            return Color(nsColor: .controlBackgroundColor)
        }
        // Translucent system fill: the panel's glass stays visible through it
        // while content stays legible over any wallpaper in either appearance.
        return Color(nsColor: .controlBackgroundColor).opacity(inGlassPanel ? 0.72 : 0.85)
    }

    private var borderFill: Color {
        colorSchemeContrast == .increased
            ? Color(nsColor: .separatorColor)
            : Color.primary.opacity(colorScheme == .dark ? 0.12 : 0.08)
    }
}

extension View {
    func glassCard(cornerRadius: CGFloat = 10) -> some View {
        modifier(GlassCardBackground(cornerRadius: cornerRadius))
    }

    /// Shows `cursor` while the pointer is over this view. `onHoverChange` is for
    /// callers that also drive their own hover styling.
    ///
    /// Uses `set()`, not `push()`/`pop()`: that stack is app-wide and `pop()`
    /// removes whatever is on top rather than the entry this view pushed, so a
    /// view torn down mid-hover — closing the popover or the Settings window
    /// replaces the whole hosting root, with no exit callback — either leaks its
    /// push or pops another view's cursor, and no amount of local bookkeeping
    /// fixes that. `set()` keeps no stack, so the worst case is a stale cursor
    /// that the next region the pointer enters corrects.
    ///
    /// AppKit cursor rects would be tidier still, but `NSHostingView` manages the
    /// cursor itself and `addCursorRect` does not take effect inside it (measured:
    /// the pointing hand never appeared).
    func hoverCursor(
        _ cursor: NSCursor, onHoverChange: ((Bool) -> Void)? = nil
    ) -> some View {
        onHover { inside in
            onHoverChange?(inside)
            if inside { cursor.set() } else { NSCursor.arrow.set() }
        }
    }
}

/// Shared dashboard-card chrome: rounded glass panel, matching the Tauri
/// dashboard's card stack.
struct DashCard<Content: View>: View {
    let title: String
    var subtitle: String?
    /// Secondary text set directly after the title on the same line (the
    /// account a window card shows, when it is not the primary). Not part of
    /// `subtitle`, which stays as it was, and not `trailing`, which sits at
    /// the far end of the header where a label reads as unrelated.
    var titleAccessory: AnyView?
    @ViewBuilder var trailing: () -> AnyView?
    @ViewBuilder var content: () -> Content

    init(
        _ title: String, subtitle: String? = nil, titleAccessory: AnyView? = nil,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.title = title
        self.subtitle = subtitle
        self.titleAccessory = titleAccessory
        self.trailing = { nil }
        self.content = content
    }

    init<T: View>(
        _ title: String, subtitle: String? = nil, titleAccessory: AnyView? = nil,
        @ViewBuilder trailing: @escaping () -> T,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.title = title
        self.subtitle = subtitle
        self.titleAccessory = titleAccessory
        self.trailing = { AnyView(trailing()) }
        self.content = content
    }

    var body: some View {
        VStack(alignment: .leading, spacing: NativeDesign.spacing) {
            HStack(alignment: .firstTextBaseline) {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(title.localized)
                            .font(NativeDesign.sectionFont)
                        if let titleAccessory { titleAccessory }
                    }
                    if let subtitle {
                        Text(subtitle.localized)
                            .font(NativeDesign.detailFont)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                trailing()
            }
            content()
        }
        .padding(NativeDesign.cardPadding)
        .glassCard()
    }
}

/// A card's "still fetching" line: spinner plus label, at the same weight as
/// the empty-state text it alternates with. Shared so the several places that
/// wait on the same dashboard fetch cannot drift into looking like different
/// kinds of waiting — a bare label reads as a final state, not a pending one.
struct LoadingLine: View {
    let title: String

    var body: some View {
        HStack(spacing: 7) {
            ProgressView()
                .controlSize(.small)
                .frame(width: 14, height: 14)
            Text(title.localized)
                .font(NativeDesign.detailFont)
                .foregroundStyle(.secondary)
        }
    }
}

/// The three-cell totals row (Total / Tokens / Best day), ported from
/// TokenUsageCard.tsx in its bare form.
struct TokenUsageRow: View {
    let stats: UsageStats

    var body: some View {
        HStack(spacing: 8) {
            cell(
                Format.usd(stats.totalCost), "Total",
                "\(Format.mmdd(stats.dateRange.start)) → \(Format.mmdd(stats.dateRange.end))")
            cell(
                Format.compactTokens(stats.totalTokens), "Tokens",
                "%lld active days".localized(stats.activeDays))
            cell(
                stats.bestDay.map { Format.usd($0.cost) } ?? "$0.00", "Best day",
                stats.bestDay.map { Format.monthDay($0.date) } ?? "—")
        }
    }

    private func cell(_ num: String, _ label: String, _ sub: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(num)
                .font(.system(size: 15, weight: .semibold).monospacedDigit())
                .foregroundStyle(.primary)
            Text(label.localized)
                .font(NativeDesign.detailFont)
                .foregroundStyle(.secondary)
            Text(sub)
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Streak summary card, ported from StreaksCard.tsx.
struct StreaksCard: View {
    let streaks: Streaks

    var body: some View {
        DashCard("Streaks") {
            HStack(spacing: 8) {
                item(streaks.longest, "Longest")
                item(streaks.current, "Current")
            }
        }
    }

    private func item(_ days: Int, _ label: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            (Text("\(days)").font(.system(size: 17, weight: .semibold).monospacedDigit())
                + Text(" days").font(NativeDesign.detailFont).foregroundStyle(.secondary))
            Text(label.localized)
                .font(NativeDesign.detailFont)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// The token-category palette, in display order. Shared because it was already
/// written out twice inside ModelBreakdownCard and the window card would have
/// made a third copy — three places to keep in step is how a legend and a chart
/// drift apart.
enum TokenKindPalette {
    static let all: [(label: String, color: String)] = [
        ("Input", "#3b82f6"),
        ("Output", "#22c55e"),
        ("Cache read", "#f59e0b"),
        ("Cache write", "#a855f7"),
        ("Reasoning", "#ec4899"),
    ]
}

/// Compact segmented control, tighter than the native picker. Lifted out of
/// UsageChartCard when a second card needed one — including the part that is
/// not obvious: plain adaptive fills, never nested glass. Signatures, focus,
/// and AX behavior are unchanged; only the chrome is native now, with no
/// shadow glow on the thumb.
struct SegmentedPicker<Value: Hashable>: View {
    @Binding var selection: Value
    let options: [(value: Value, label: String)]
    /// Optional per-option tooltip, keyed by option value.
    var help: [Value: String] = [:]
    /// When the options do not fit on one line, wrap them onto further lines
    /// instead of running past the card (Antigravity's four grouped windows,
    /// maintainer 2026-10-03). A row that fits is drawn exactly as before.
    var wraps = false

    @Environment(\.inGlassPanel) private var inGlassPanel
    @Environment(\.colorSchemeContrast) private var colorSchemeContrast
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Namespace private var thumb

    var body: some View {
        if wraps {
            ViewThatFits(in: .horizontal) {
                singleLine
                wrapped
            }
        } else {
            singleLine
        }
    }

    @ViewBuilder
    private var singleLine: some View {
        if inGlassPanel {
            panelBody
        } else {
            plainBody
        }
    }

    /// The same buttons in a wrapping row, on a rounded track (a capsule does
    /// not hold more than one line).
    private var wrapped: some View {
        sliding(
            FlowLayout(hSpacing: 2, vSpacing: 2) {
                buttons
            }
            .padding(2)
            .background(trackFill, in: RoundedRectangle(cornerRadius: 8)))
    }

    private var buttons: some View {
        ForEach(options, id: \.value) { option in
            let on = selection == option.value
            Button { selection = option.value } label: {
                label(option.label, on: on)
                    .background(
                        on ? AnyShapeStyle(Color.primary.opacity(0.16)) : AnyShapeStyle(.clear),
                        in: RoundedRectangle(cornerRadius: 6))
                    .contentShape(RoundedRectangle(cornerRadius: 6))
            }
            .buttonStyle(.plain)
            .help(help[option.value] ?? "")
        }
    }

    private var plainBody: some View {
            HStack(spacing: 2) {
                ForEach(options, id: \.value) { option in
                    let on = selection == option.value
                    Button { selection = option.value } label: {
                        label(option.label, on: on)
                            .background(
                                on ? AnyShapeStyle(Color.primary.opacity(0.16))
                                   : AnyShapeStyle(.clear),
                                in: RoundedRectangle(cornerRadius: 4))
                            .contentShape(RoundedRectangle(cornerRadius: 4))
                    }
                    .buttonStyle(.plain)
                    .help(help[option.value] ?? "")
                }
            }
            .padding(1)
            .background(trackFill, in: RoundedRectangle(cornerRadius: 6))
    }

    /// Under the glass panel: the macOS 26 segmented shape — a capsule track
    /// with a raised thumb that slides to the selection. Still plain fills,
    /// not glass, for the nesting reason above.
    private var panelBody: some View {
        sliding(
            HStack(spacing: 2) {
                ForEach(options, id: \.value) { option in
                    let on = selection == option.value
                    Button { selection = option.value } label: {
                        label(option.label, on: on)
                            .padding(.horizontal, 2)
                            .background {
                                if on {
                                    Capsule()
                                        .fill(thumbFill)
                                        .matchedGeometryEffect(id: "thumb", in: thumb)
                                }
                            }
                            .contentShape(Capsule())
                    }
                    .buttonStyle(.plain)
                    .help(help[option.value] ?? "")
                }
            }
            .padding(2)
            .background(trackFill, in: Capsule()))
    }

    private var trackFill: Color {
        Color.primary.opacity(colorSchemeContrast == .increased || reduceTransparency ? 0.12 : 0.07)
    }

    private var thumbFill: Color {
        Color.primary.opacity(colorSchemeContrast == .increased ? 0.24 : 0.16)
    }

    /// Selection slide, skipped under reduce motion (the thumb still moves,
    /// it just lands instead of gliding).
    @ViewBuilder
    private func sliding<Body: View>(_ body: Body) -> some View {
        if reduceMotion {
            body
        } else {
            body.panelSelectionSlide(selection)
        }
    }

    private func label(_ text: String, on: Bool) -> some View {
        Text(text.localized)
            .lineLimit(1)
            .fixedSize()
            .font(.caption2.weight(on ? .semibold : .regular))
            .foregroundStyle(on ? .primary : .secondary)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
    }
}
