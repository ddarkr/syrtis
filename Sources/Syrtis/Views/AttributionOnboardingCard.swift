import SwiftUI
import TokenBarCore

/// The card that makes usage attribution findable without opening Settings.
///
/// Like `GrokBotKeychainConsent`, the flag
/// records an ANSWER — "not now" — never the fact of having been shown. A
/// user who has not yet decided keeps seeing the card every time Overview
/// opens, because nothing here is a one-time interruption; it is a standing
/// invitation until either it is declined or the thing it is inviting the
/// user to do (attribute something) has happened.
enum AttributionOnboardingCard {
    static let dismissedKey = "tokenbar.usage.attribution.onboardingDismissed"

    /// Suggestion lines beyond this fold into "and N more" rather than
    /// growing the card without bound.
    static let maxVisibleLines = 4


    enum Copy {
        static let title = "Attribute usage to subscriptions"
        static let subtitle = "Quota history shows no tokens or API-equivalent value until usage is attributed."
        /// source client · provider → target
        static let suggestionLine = "%@ · %@ → %@"
        static let moreCount = "and %lld more"
        static let unsuggestedHint = "Without a suggestion: %lld — set them in Settings."
        static let notNow = "Not now"
        static let setUpManually = "Set up manually…"
        static let applySuggestions = "Apply suggestions"
        /// Under the window history, where every row reads 0 / $0.00 until
        /// something is attributed: the zeros are the missing attribution,
        /// not a quiet window.
        static let historyZeroNote = "The tokens and amounts below stay at 0 until usage is attributed to this subscription."
        static let setUpLink = "Set up usage attribution…"

        static var all: [String] {
            [
                title, subtitle, suggestionLine, moreCount, unsuggestedHint,
                notNow, setUpManually, applySuggestions, historyZeroNote, setUpLink,
            ]
        }
    }

    /// The gates that do not need the dashboard's data, stated once. They
    /// decide both whether the card can appear and whether the Quota lens
    /// fetches the model report for it, so the two cannot disagree.
    ///
    /// A confirmed table this codec cannot read (a newer build's format, a
    /// foreign value) is not "nothing confirmed": it is treated as configured,
    /// because inviting that user would end in a write the codec refuses.
    static func mayShow(
        confirmed: UsageAttribution.Table, dismissed: Bool, arguments: [String]
    ) -> Bool {
        confirmed.isWritable && confirmed.records.isEmpty && !dismissed
            && !BuildIdentity.isNonUserRuntime(arguments)
    }

    /// Reads `object(forKey:)`, not a String default: an absent key is
    /// "nothing confirmed", but an empty string does not parse and would read
    /// as a foreign value.
    static func mayShow(
        defaults: UserDefaults = .standard, arguments: [String] = CommandLine.arguments
    ) -> Bool {
        mayShow(
            confirmed: UsageAttribution.confirmed(defaults: defaults),
            dismissed: defaults.object(forKey: dismissedKey) as? Bool == true,
            arguments: arguments)
    }

    /// With the data in hand: something to offer. With nothing confirmed,
    /// every attributable row is either a proposal or counted as unsuggested.
    static func isVisible(
        mayShow: Bool, summary: UsageAttributionSettings.OnboardingSummary?
    ) -> Bool {
        guard mayShow, let summary else { return false }
        return !summary.records.isEmpty || summary.unsuggestedCount > 0
    }

    /// The proposals the card would offer for this data, or nil before it
    /// has loaded. Shared by the card and the setup header's count.
    static func summary(
        modelReport: ModelReport?, agentUsage: AgentUsagePayload?
    ) -> UsageAttributionSettings.OnboardingSummary? {
        guard let modelReport, let agentUsage else { return nil }
        return UsageAttributionSettings.onboardingSummary(
            entries: modelReport.entries,
            confirmed: [],
            subscriptionClients: UsageAttributionSettings.subscriptionClients(from: agentUsage),
            routedSubscriptions: UsageAttributionSettings.routedSubscriptions(from: agentUsage))
    }

    /// Whether the card is on screen for this data right now.
    static func shows(modelReport: ModelReport?, agentUsage: AgentUsagePayload?) -> Bool {
        let may = mayShow()
        return isVisible(
            mayShow: may, summary: may ? summary(modelReport: modelReport, agentUsage: agentUsage) : nil)
    }

    /// This flag records an ANSWER ("Not now", or "Skip setup"), not the fact
    /// of having been shown, so a user who has not yet decided keeps seeing
    /// the card on every open.
    static func markDismissed(defaults: UserDefaults = .standard) {
        defaults.set(true, forKey: dismissedKey)
    }

    /// One proposal line. The record's own `state` IS the proposed target —
    /// `acceptanceRecords` already resolved it — so this only has to render
    /// it, never re-derive it.
    static func suggestionLine(_ record: UsageAttribution.Record) -> String {
        let providerLabel = (record.provider.isEmpty
            ? UsageAttributionSettings.Copy.unspecifiedProvider : record.provider
        ).localized
        let targetLabel: String
        switch record.state {
        case let .assigned(target):
            targetLabel = ClientRegistry.style(target).displayName
        case .excluded:
            targetLabel = UsageAttributionSettings.Copy.excluded.localized
        case .unassigned:
            targetLabel = UsageAttributionSettings.Copy.unassigned.localized
        }
        return Copy.suggestionLine.localized(
            ClientRegistry.style(record.client).displayName, providerLabel, targetLabel)
    }
}

struct AttributionOnboardingCardView: View {
    var modelReport: ModelReport?
    var agentUsage: AgentUsagePayload?

    /// Observed so an Apply here or in Settings redraws the card; the value
    /// itself is read through `defaults.object`, see `mayShow(defaults:)`.
    @AppStorage(UsageAttribution.confirmedKey) private var confirmedRaw = ""
    @AppStorage(AttributionOnboardingCard.dismissedKey) private var dismissed = false
    @State private var applyFailure: String?

    /// Computed once per body from THIS surface's own inputs — the model
    /// report and the agent-usage payload the popover already polls — never
    /// from the stored suggestions table, which Settings alone fills.
    private var summary: UsageAttributionSettings.OnboardingSummary? {
        AttributionOnboardingCard.summary(modelReport: modelReport, agentUsage: agentUsage)
    }

    var body: some View {
        let _ = (confirmedRaw, dismissed)
        let mayShow = AttributionOnboardingCard.mayShow()
        let summary = mayShow ? summary : nil
        OnboardingCardContainer(
            visible: AttributionOnboardingCard.isVisible(mayShow: mayShow, summary: summary)
        ) {
            if let summary {
                DashCard(AttributionOnboardingCard.Copy.title) {
                    content(summary)
                }
            }
        }
    }

    @ViewBuilder
    private func content(_ summary: UsageAttributionSettings.OnboardingSummary) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(AttributionOnboardingCard.Copy.subtitle.localized)
                .font(NativeDesign.detailFont)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            if let applyFailure {
                Text(applyFailure.localized)
                    .font(NativeDesign.detailFont)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }

            VStack(alignment: .leading, spacing: 3) {
                ForEach(
                    Array(summary.records.prefix(AttributionOnboardingCard.maxVisibleLines).enumerated()),
                    id: \.offset
                ) { _, record in
                    Text(AttributionOnboardingCard.suggestionLine(record))
                        .font(NativeDesign.bodyFont)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if summary.records.count > AttributionOnboardingCard.maxVisibleLines {
                    Text(AttributionOnboardingCard.Copy.moreCount.localized(
                        Int64(summary.records.count - AttributionOnboardingCard.maxVisibleLines)))
                        .font(NativeDesign.detailFont)
                        .foregroundStyle(.secondary)
                }
                if summary.unsuggestedCount > 0 {
                    Text(AttributionOnboardingCard.Copy.unsuggestedHint.localized(
                        Int64(summary.unsuggestedCount)))
                        .font(NativeDesign.detailFont)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            HStack(spacing: 10) {
                Button(AttributionOnboardingCard.Copy.notNow.localized) {
                    AttributionOnboardingCard.markDismissed()
                }
                .buttonStyle(.plain)
                .font(NativeDesign.detailFont)
                .foregroundStyle(.secondary)

                Button(AttributionOnboardingCard.Copy.setUpManually.localized) {
                    SettingsWindowController.shared.showFromPopover(scrollingTo: .usageAttribution)
                }
                .buttonStyle(.plain)
                .font(NativeDesign.detailFont)

                Spacer()

                if !summary.records.isEmpty {
                    Button(AttributionOnboardingCard.Copy.applySuggestions.localized) {
                        applyFailure = UsageAttributionSettings.accept(
                            summary.records, defaults: .standard)
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                }
            }
        }
    }
}

/// The link every "nothing attributed yet" surface uses to reach the page.
struct AttributionSetupLink: View {
    var body: some View {
        Button(AttributionOnboardingCard.Copy.setUpLink.localized) {
            SettingsWindowController.shared.showFromPopover(scrollingTo: .usageAttribution)
        }
        .buttonStyle(.link)
        .font(NativeDesign.detailFont)
    }
}

/// Hosts an onboarding card and animates it away when it is answered: it
/// keeps the needed fade and closes the gap over the same curve, instead of
/// the card vanishing in one frame. The maintainer asked for a dismissal
/// animation on every card; reduced motion keeps the fade but drops the
/// slide and scale.
struct OnboardingCardContainer<Card: View>: View {
    let visible: Bool
    @ViewBuilder let card: () -> Card

    /// The gap below a visible card. It lives inside the conditional, so a
    /// dismissed card takes its gap with it: parents stack these with no
    /// spacing of their own. Stack spacing on the parent kept a gap for every
    /// hidden card, and the space grew with each card answered.
    static var gap: CGFloat { 12 }

    /// Long enough to read as a deliberate exit, short enough not to hold up
    /// the dashboard behind it.
    static var dismissAnimation: Animation { .easeInOut(duration: 0.32) }

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var removal: AnyTransition {
        reduceMotion
            ? .opacity
            : .opacity
                .combined(with: .move(edge: .top))
                .combined(with: .scale(scale: 0.96, anchor: .top))
    }

    var body: some View {
        VStack(spacing: 0) {
            if visible {
                card()
                    .padding(.bottom, Self.gap)
                    .transition(.asymmetric(insertion: .opacity, removal: removal))
            }
        }
        .animation(reduceMotion ? nil : Self.dismissAnimation, value: visible)
    }
}
