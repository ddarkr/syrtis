import SwiftUI
import TokenBarCore

/// Asks, once, how much token traffic the animated menu-bar icon should be
/// scaled for (`AnimationPace`). Picking a pace applies it at once so it can be
/// tried on the live icon; "Done" answers the card. Until a pace is picked the
/// icon runs at `.moderate`.
enum AnimationPaceOnboarding {
    enum Copy {
        static let title = "How busy are your agents?"
        static let body = "The menu-bar animation follows your live token rate. Pick the range that fits how you work; you can change it later in Settings."
        static let recommended = "Most people: Moderate"
    }

    /// Set by "Done" (or "Skip setup"). Choosing a pace alone does not answer
    /// the card: the maintainer wanted to try several before confirming.
    static let answeredKey = OnboardingSetup.keyPrefix + "pace"

    /// Shown for an animated icon that is animating, before any pace was
    /// picked, in a user runtime. Independent of the other onboarding cards:
    /// the maintainer asked for every card to show at once rather than one
    /// appearing only after another is answered.
    static func isVisible(
        style: String, animate: Bool, answered: Bool, isNonUserRuntime: Bool
    ) -> Bool {
        TrayAnimator.animatedStyles.contains(style) && animate && !answered && !isNonUserRuntime
    }
}

struct AnimationPaceOnboardingCardView: View {
    @AppStorage(TrayAnimator.styleKey) private var style = "cat"
    @AppStorage(TrayAnimator.animateKey) private var animate = true
    @AppStorage(AnimationPace.storageKey) private var paceRaw = ""
    @AppStorage(AnimationPaceOnboarding.answeredKey) private var answered = false

    private var visible: Bool {
        AnimationPaceOnboarding.isVisible(
            style: style, animate: animate, answered: answered,
            isNonUserRuntime: BuildIdentity.isNonUserRuntime(CommandLine.arguments))
    }

    var body: some View {
        OnboardingCardContainer(visible: visible) {
            DashCard(AnimationPaceOnboarding.Copy.title) {
                VStack(alignment: .leading, spacing: 8) {
                    Text(AnimationPaceOnboarding.Copy.body.localized)
                        .font(NativeDesign.detailFont)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    // Display the running icon's effective pace without
                    // committing an unanswered or unknown stored choice.
                    Picker(selection: Binding(
                        get: { (AnimationPace(rawValue: paceRaw) ?? .default).rawValue },
                        set: { paceRaw = $0 }
                    )) {
                        ForEach(AnimationPace.allCases, id: \.self) { pace in
                            VStack(alignment: .leading, spacing: 1) {
                                Text(pace.label.localized)
                                    .font(NativeDesign.bodyFont)
                                Text(pace.detail.localized)
                                    .font(NativeDesign.detailFont)
                                    .foregroundStyle(.secondary)
                            }
                            .fixedSize(horizontal: false, vertical: true)
                            .tag(pace.rawValue)
                        }
                    } label: {
                        EmptyView()
                    }
                    .pickerStyle(.radioGroup)
                    .labelsHidden()
                    .accessibilityLabel(AnimationPaceOnboarding.Copy.title.localized)
                    HStack {
                        Text(AnimationPaceOnboarding.Copy.recommended.localized)
                            .font(NativeDesign.detailFont)
                            .foregroundStyle(.tertiary)
                        Spacer()
                        Button(OnboardingSetupCopy.done.localized) {
                            if AnimationPace(rawValue: paceRaw) == nil {
                                paceRaw = AnimationPace.default.rawValue
                            }
                            answered = true
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                    }
                }
            }
        }
    }
}
