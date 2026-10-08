/// Display-grouping identity for model ids that arrive raw.
///
/// The engine's model, monthly and hourly reports already group through
/// `normalize_model_for_grouping`, but the graph payload keys each row by the
/// raw `canonical_model_id`, so Swift surfaces that group graph rows by model
/// have to apply the same fold themselves or they disagree with the Models
/// view. This mirrors the engine's one built-in rule (`builtin_grouping` in
/// `crates/syrtis_core/src/model_alias.rs`): Grok Build keys turn usage by
/// `grok-<version>-build` while the session names `grok-<version>` (#118).
///
/// Presentation only. Anything that matches a model id — quota scopes and
/// attribution records — keeps the raw id. `ModelColorMap` groups on both
/// sides, construction and lookup, so a raw entry and a raw lookup still meet
/// and the two ids share one shade. `Tests/fixtures/model-grouping-cases.json` is checked against both
/// this function (SelfTest) and the engine (tb_core_ffi test). That binds the
/// two on those inputs only: an engine rule that widens beyond the table, or
/// a user alias map once the app installs one, would not be caught by it.
public enum ModelGrouping {
    /// `grok-<version>-build` → `grok-<version>`, where the version is one or
    /// more dot-separated runs of ASCII digits. Every other id is returned
    /// unchanged. Expects the lowercase `canonical_model_id` spelling the
    /// graph payload carries.
    public static func groupID(_ canonicalID: String) -> String {
        // Longer than "grok--build" so the prefix and suffix cannot overlap
        // ("grok-build") and the version between them is non-empty.
        guard canonicalID.count > "grok--build".count,
              canonicalID.hasPrefix("grok-"), canonicalID.hasSuffix("-build")
        else { return canonicalID }
        let version = canonicalID.dropFirst("grok-".count).dropLast("-build".count)
        let isVersion = version.split(separator: ".", omittingEmptySubsequences: false)
            .allSatisfy { part in !part.isEmpty && part.allSatisfy { $0.isASCII && $0.isNumber } }
        return isVersion ? "grok-\(version)" : canonicalID
    }
}
