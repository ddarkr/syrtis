//! Process-wide registry of the extra Claude config directories the user has
//! configured (`CLAUDE_CONFIG_DIR`-isolated accounts). Written by
//! `tb_set_claude_config_dirs`, read by the Claude quota fetch.
//!
//! **Not the same value as `extra_scan_paths`, on purpose.** That registry
//! holds the expanded `<dir>/projects` and `<dir>/transcripts` sub-roots and
//! answers "which directories does the scanner walk". This one holds the
//! config directories themselves and answers "whose credential is this quota
//! card fetched with" — the directory selects the Keychain item
//! (`claude_keychain_service`) and therefore the durable history identity
//! (`claude_history_scope`). The two share one user input and nothing else:
//! a directory the scanner rejects is still a perfectly valid account, and
//! the registered scan subset is empty until the launch-time apply lands,
//! which is an honest answer for scanning and a wrong one for identity.
//!
//! A `RwLock` static rather than an env var, for the same reason
//! `extra_scan_paths` is one: the process is resident and a Settings edit
//! must take effect without a restart.

use std::path::Path;
use std::sync::{LazyLock, RwLock};

static CLAUDE_CONFIG_DIRS: LazyLock<RwLock<Vec<String>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Configured extra config directories, in the order the user listed them.
/// Empty by default — a process that never calls the setter fetches exactly
/// one Claude account, as before this feature existed.
pub(crate) fn snapshot() -> Vec<String> {
    CLAUDE_CONFIG_DIRS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Trailing separators change the SHA-256 the Keychain service name is derived
/// from, so `/x/.claude-work/` would read no item at all while looking
/// identical in Settings. A relative path is refused rather than resolved: it
/// would resolve against whatever the process CWD happens to be, and the
/// resulting identity would move with it.
///
/// Whitespace is deliberately NOT trimmed, though an earlier version did. The
/// Swift side sends `standardizingPath`'s output verbatim and that call does
/// not strip surrounding whitespace, so trimming here would derive the Keychain
/// service from a string that is not the directory: a folder whose name really
/// ends in a space is reachable through the picker, its transcripts would be
/// scanned under the real path, and its quota card would report no login found
/// forever. Whitespace is a legal character in a path, and the two derivations
/// of one identity have to agree before either of them has to look tidy.
fn normalize(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("empty path".to_string());
    }
    if !Path::new(raw).is_absolute() {
        return Err("path is not absolute".to_string());
    }
    let stripped = raw.trim_end_matches('/');
    if stripped.is_empty() {
        return Err("path is the filesystem root".to_string());
    }
    Ok(stripped.to_string())
}

/// Replace the whole registry from a JSON array of absolute directory paths.
/// Full-replace, not merge: `[]` clears every configured account (the Settings
/// rollback path).
///
/// Existence is deliberately not probed. Whether a directory can be read right
/// now says nothing about which account its Keychain item belongs to, and the
/// answer would go stale the moment a volume mounts. It also keeps this setter
/// off the `stat` path that made `extra_scan_paths` block its caller.
///
/// Returns the reported payload and whether the effective registry changed.
/// The comparison runs under the registry write lock: callers invalidate
/// caches only when `changed` is true, after the lock is released, so a
/// no-op reapply never disturbs live leases.
pub(crate) fn set_from_json(raw: &str) -> Result<(serde_json::Value, bool), String> {
    let input: Vec<String> =
        serde_json::from_str(raw).map_err(|e| format!("invalid Claude config dirs JSON: {}", e))?;

    let mut registered: Vec<String> = Vec::new();
    let mut rejected: Vec<serde_json::Value> = Vec::new();
    for raw_dir in input {
        match normalize(&raw_dir) {
            // A duplicate is refused rather than deduplicated silently: it
            // would otherwise produce two cards for one account, both writing
            // the same series.
            Ok(dir) if registered.contains(&dir) => rejected.push(serde_json::json!({
                "path": raw_dir,
                "reason": "duplicate directory",
            })),
            Ok(dir) => registered.push(dir),
            Err(reason) => rejected.push(serde_json::json!({
                "path": raw_dir,
                "reason": reason,
            })),
        }
    }

    let registered_count = registered.len();
    let mut guard = CLAUDE_CONFIG_DIRS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let changed = *guard != registered;
    if changed {
        *guard = registered;
    }

    Ok((
        serde_json::json!({
            "registeredCount": registered_count,
            "rejected": rejected,
        }),
        changed,
    ))
}

/// One process-wide mutex for every test that reads or writes the static, so
/// parallel `cargo test` threads do not observe each other's registry. Public
/// to the crate because `agent_usage` and `lib` both drive it.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *CLAUDE_CONFIG_DIRS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Vec::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_absolute_dirs_and_reports_what_it_refused() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();

        let (result, changed) = set_from_json(
            r#"["/Users/x/.claude-work/", "/Users/x/claude dir ", "relative/dir", "", "/Users/x/.claude-work"]"#,
        )
        .unwrap();
        assert!(changed);
        assert_eq!(result["registeredCount"], 2);
        assert_eq!(
            snapshot(),
            vec![
                "/Users/x/.claude-work".to_string(),
                "/Users/x/claude dir ".to_string()
            ],
            "a trailing separator would derive a different Keychain service, and \
             trimming the trailing SPACE would derive one for a directory that is \
             not the one Swift registered for scanning — that account's \
             transcripts would be read while its quota card reported no login"
        );
        let reasons: Vec<&str> = result["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["reason"].as_str().unwrap())
            .collect();
        assert_eq!(
            reasons,
            vec!["path is not absolute", "empty path", "duplicate directory"]
        );
        // And a path that is nothing but whitespace is still refused, by the
        // absolute-path rule rather than by trimming it into emptiness.
        let (whitespace, whitespace_changed) = set_from_json(r#"["   "]"#).unwrap();
        assert_eq!(whitespace["registeredCount"], 0);
        assert!(whitespace_changed);

        // Full-replace, including back to nothing. The whitespace-only input
        // above already replaced the registry with empty, so `[]` is a no-op.
        let (_, cleared_changed) = set_from_json("[]").unwrap();
        assert!(!cleared_changed);
        assert!(snapshot().is_empty());
        reset_for_test();
    }

    #[test]
    fn identical_reapply_reports_unchanged() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        let payload = r#"["/Users/x/.claude-work"]"#;
        let (_, first) = set_from_json(payload).unwrap();
        assert!(first);
        let (result, changed) = set_from_json(payload).unwrap();
        assert!(!changed);
        assert_eq!(result["registeredCount"], 1);
        // Trailing-separator spelling is the same effective directory.
        let (_, normalized) = set_from_json(r#"["/Users/x/.claude-work/"]"#).unwrap();
        assert!(!normalized);
        assert_eq!(snapshot(), vec!["/Users/x/.claude-work".to_string()]);
        let (_, cleared) = set_from_json("[]").unwrap();
        assert!(cleared);
        let (_, recleared) = set_from_json("[]").unwrap();
        assert!(!recleared);
        reset_for_test();
    }

    #[test]
    fn malformed_json_leaves_the_registry_untouched() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        set_from_json(r#"["/Users/x/.claude-work"]"#).unwrap();
        let error = set_from_json("{not json").unwrap_err();
        assert!(error.contains("invalid Claude config dirs JSON"), "{error}");
        assert_eq!(snapshot(), vec!["/Users/x/.claude-work".to_string()]);
        reset_for_test();
    }
}
