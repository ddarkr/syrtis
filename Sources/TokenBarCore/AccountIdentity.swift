import Foundation

/// Identifies one quota-bearing account within a client. `accountKey` is nil
/// for the primary account, the `CLAUDE_CONFIG_DIR` absolute path for an extra
/// Claude account, and a 64-hex key for a captured Antigravity account. The
/// Antigravity key is derived from the Google account id and is not for
/// display: `accountLabel` shows the registry's label instead.
///
/// A pair, never an encoded string. `ClientRegistry.parseIdSet`,
/// `ClientRegistry.style(_:)`, `quotaExcludedClients()` and `QuotaResolver`
/// all compare `clientId` alone for equality; folding the account into that
/// string would silently stop matching in every one of them, and none of them
/// would report an error — the client would just look absent.
public struct AccountIdentity: Hashable, Sendable {
    public let clientId: String
    public let accountKey: String?

    public init(clientId: String, accountKey: String?) {
        self.clientId = clientId
        self.accountKey = accountKey
    }

    /// True for the primary account of `clientId` — every account today,
    /// except an extra Claude config directory.
    public var isPrimary: Bool { accountKey == nil }

    /// The dictionary key and `Identifiable.id` for one window of one account.
    ///
    /// The primary keeps the exact two-part shape callers have always built by
    /// hand (`"<clientId>|<cardId>"`), so nothing a plain client stores or
    /// looks up changes; only an extra account's key grows a third segment.
    ///
    /// It lives here, in the lower target, because both halves of the quota
    /// lens need it: `DashboardModel` stores the grids and `QuotaOverview`'s
    /// row ids select into them. Two definitions of the same key is how one
    /// side stores under three parts while the other looks up under two, which
    /// finds nothing and reports nothing.
    public func windowKey(cardId: String) -> String {
        guard let accountKey else { return "\(clientId)|\(cardId)" }
        return "\(clientId)|\(accountKey)|\(cardId)"
    }

    /// A short label naming which account this is, for any surface that shows
    /// the client's display name and would otherwise render two accounts
    /// identically. Nil for the primary, which needs no qualifier.
    ///
    /// The basename, not the path. The value is a directory under the user's
    /// home, and the surfaces that use this are the ones that end up in
    /// screenshots; the full path stays reachable through a tooltip for the
    /// case where two directories share a last component.
    ///
    /// It lives here rather than beside one of its callers because the label
    /// two views derive separately is the label they eventually derive
    /// differently, and the reader has no way to tell that the "work" in the
    /// limits card and the "work" in the overview line mean the same account.
    public var accountLabel: String? {
        guard let accountKey else { return nil }
        if clientId == Self.antigravityClientId {
            // Never the key: it is derived from the Google account id. A key
            // the registry no longer holds (removed while a payload built
            // before the removal is still on screen) gets a generic label.
            return Self.antigravityLabel(accountKey) ?? "Antigravity account".localized
        }
        let name = (accountKey as NSString).lastPathComponent
        return name.isEmpty ? accountKey : name
    }

    /// The tooltip for `accountLabel`: the full config directory for a Claude
    /// account, whose label is only the basename, and the label itself for a
    /// captured Antigravity account, whose key must not be shown.
    public var accountTooltip: String? {
        clientId == Self.antigravityClientId ? accountLabel : accountKey
    }

    /// Whether this account's usage can be read from local logs. Only the
    /// primary of each client and extra Claude accounts (whose key is a config
    /// directory) qualify; a captured Antigravity account has no local logs,
    /// and its key in a `WindowUsageRequest` would be looked up as a Claude
    /// config directory instead of joining that captured account.
    public var hasLocalUsage: Bool { accountKey == nil || clientId == "claude" }

    public static let antigravityClientId = "antigravity"

    /// Label for a captured Antigravity account's key, from the app's account
    /// registry (`AntigravityAccounts.installLabelResolver`). Nil when the key
    /// is not registered. Set once at launch, or by the selftest.
    nonisolated(unsafe) public static var antigravityLabel: @Sendable (String) -> String? = { _ in nil }
}

extension AgentUsageSnapshot {
    public var accountIdentity: AccountIdentity {
        AccountIdentity(clientId: clientId, accountKey: accountKey)
    }
}
