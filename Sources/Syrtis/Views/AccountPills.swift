import SwiftUI
import TokenBarCore

/// What one client's card needs to know about its accounts, resolved once per
/// body pass by `DashboardModel.cardAccountContext(for:)` from the PUBLISHED
/// payload. A value, so the views never read the payload or the preference
/// themselves and cannot disagree with the model about which account is shown.
struct CardAccountContext: Equatable, Sendable {
    let clientId: String
    /// Live accounts in payload order (`WindowCardAccount.accounts`).
    let accounts: [String?]
    /// The account the card shows. Nil is the primary.
    let resolved: String?
    /// Whether the tab has local records at all (`WindowCardGate`). A quota-only
    /// tab has none: a scan there returns zeros that read as "nothing used", so
    /// no account on it, the primary included, shows local usage.
    var tabHasLocalRecords = true
    /// The primary snapshot's own email (the IDE route's, or the captured
    /// account's when `AntigravityDedup` merged them), so the primary's pill
    /// names the account rather than the client (maintainer, 2026-10-03).
    var primaryEmail: String? = nil

    var identity: AccountIdentity { AccountIdentity(clientId: clientId, accountKey: resolved) }
    var isPrimary: Bool { identity.isPrimary }
    /// Nil for the primary, whose card header is unchanged.
    var label: String? { identity.accountLabel }
    var tooltip: String? { identity.accountTooltip }
    var showsPills: Bool {
        !AccountPills.options(clientId: clientId, accounts: accounts, primaryEmail: primaryEmail).isEmpty
    }
    var localUsageAttributable: Bool { identity.hasLocalUsage }

    /// What stands where local usage numbers go (rule 6 and its amendment), in
    /// one place for the window card and the history rows. A primary keeps its
    /// own text on a failed scan; a non-primary account that cannot be scoped
    /// locally, or whose scan failed, gets the shared fixed line. No context
    /// (no live account) behaves as the primary.
    enum LocalUsageSlot: Equatable { case numbers, spinner, unreadable, notAttributable }

    static func localUsageSlot(
        _ context: CardAccountContext?, hasUsage: Bool, scanFailed: Bool
    ) -> LocalUsageSlot {
        if let context, !context.tabHasLocalRecords { return .notAttributable }
        if let context, !context.isPrimary,
           !context.localUsageAttributable || scanFailed {
            return .notAttributable
        }
        if hasUsage { return .numbers }
        return scanFailed ? .unreadable : .spinner
    }

    /// The label a pill shows: the account's own; for the primary its email
    /// when the payload carries one; else the client's name.
    static func pillLabel(clientId: String, account: String?, primaryEmail: String? = nil) -> String {
        if let label = AccountIdentity(clientId: clientId, accountKey: account).accountLabel {
            return label
        }
        if account == nil, let email = primaryEmail?.trimmingCharacters(in: .whitespaces),
           !email.isEmpty {
            return email
        }
        return ClientRegistry.tabDisplayName(clientId)
    }
}

/// The account switcher. Built for the window card and written to be reused by
/// any later multi-account surface: it takes the accounts and the selection,
/// and knows nothing about the card it sits on.
struct AccountPills: View {
    let clientId: String
    let accounts: [String?]
    var primaryEmail: String? = nil
    let selected: String?
    let select: (String?) -> Void

    /// The pills, or none: rule 1 says a row only with at least two accounts, so
    /// this is the one statement of that threshold. Value "" is the primary.
    /// UI-free pure helper (resolved labels only), so `nonisolated`: the
    /// Sendable card context's `showsPills` calls it off the main actor.
    nonisolated static func options(
        clientId: String, accounts: [String?], primaryEmail: String? = nil
    ) -> [(value: String, label: String)] {
        guard accounts.count >= 2 else { return [] }
        return accounts.map {
            (value: $0 ?? "", label: CardAccountContext.pillLabel(
                clientId: clientId, account: $0, primaryEmail: primaryEmail))
        }
    }

    /// The header label for a card showing a non-primary account (spec 1b).
    static func headerLabel(_ context: CardAccountContext?) -> String? { context?.label }

    var body: some View {
        let options = Self.options(clientId: clientId, accounts: accounts, primaryEmail: primaryEmail)
        if !options.isEmpty {
            SegmentedPicker(
                selection: Binding(get: { selected ?? "" }, set: { select($0.isEmpty ? nil : $0) }),
                options: options,
                help: Dictionary(
                    accounts.compactMap { account in
                        AccountIdentity(clientId: clientId, accountKey: account).accountTooltip
                            .map { (account ?? "", $0) }
                    }, uniquingKeysWith: { first, _ in first }),
                wraps: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
