use super::*;
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// The primary account, spelled out so a call site reads as a choice
    /// rather than as a `None` nobody thought about.
    const PRIMARY: Account = None;

    /// Hold every lock guarding state a per-account window scan reads.
    ///
    /// The registries are process-wide statics with their own module-local test
    /// locks, and `cargo test` runs modules concurrently: a test here that
    /// installed roots while holding only this module's lock raced
    /// `extra_scan_paths::tests`, and both sides saw the other's fixture. Taken
    /// in a fixed order — this module, then the scan registry, then the config
    /// dirs — because two tests acquiring the same pair in opposite orders is
    /// the other way to make a suite hang.
    fn lock_registries() -> (
        parking_lot::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        (
            TEST_LOCK.lock(),
            crate::extra_scan_paths::TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            crate::claude_config_dirs::TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// One assistant turn with a distinct id, so a root's contribution is
    /// identifiable by its token count alone.
    fn turn(id: &str, output: i64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","requestId":"req_{id}","message":{{"id":"msg_{id}","model":"claude-3-5-sonnet","usage":{{"input_tokens":0,"output_tokens":{output}}}}}}}"#
        )
    }

    fn write_session(root: &Path, id: &str, output: i64) {
        let dir = root.join("projects").join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), turn(id, output)).unwrap();
    }

    fn output_tokens(payload: &Value) -> i64 {
        payload["diagnostics"]["intervals"][0]["all"]["breakdown"]["output"]
            .as_i64()
            .expect("aggregate output lane")
    }

    /// Primary at 1000, `work-d` at 7000 reachable by both the scan registry
    /// and a `.cc-mirror` variant, `work-e` at 500 reachable by the registry
    /// alone, `work-d/sub` at 90 reachable only by its own variant.
    ///
    /// Two extra accounts rather than one because a directory reachable two
    /// ways collapses into a single scan task and cannot tell the routes apart:
    /// `work-e` leaking is unambiguous evidence about the registry route.
    struct AccountFixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        d: PathBuf,
        e: PathBuf,
    }

    const PRIMARY_OUTPUT: i64 = 1_000;
    const D_OUTPUT: i64 = 7_000;
    const E_OUTPUT: i64 = 500;
    const SUB_OUTPUT: i64 = 90;

    fn account_fixture() -> AccountFixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let e = home.join("work-e");
        let sub = d.join("sub");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&d, "d", D_OUTPUT);
        write_session(&e, "e", E_OUTPUT);
        write_session(&sub, "sub", SUB_OUTPUT);
        for (name, config_dir) in [("v1", &d), ("v2", &sub)] {
            let variant = home.join(".cc-mirror").join(name);
            std::fs::create_dir_all(&variant).unwrap();
            std::fs::write(
                variant.join("variant.json"),
                serde_json::json!({ "configDir": config_dir.to_string_lossy() }).to_string(),
            )
            .unwrap();
        }
        // Both registries, installed through the real setters, because the
        // primary's exclusion is built from both and a test that populated one
        // would pass while the other went unread.
        let roots: Vec<String> = [&d, &e]
            .iter()
            .flat_map(|dir| {
                [
                    dir.join("projects").display().to_string(),
                    dir.join("transcripts").display().to_string(),
                ]
            })
            .collect();
        crate::extra_scan_paths::set_from_json(&serde_json::json!({ "claude": roots }).to_string())
            .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([d.display().to_string(), e.display().to_string()]).to_string(),
        )
        .unwrap();
        AccountFixture {
            _dir: dir,
            home,
            d,
            e,
        }
    }

    fn reset_registries() {
        crate::extra_scan_paths::reset_for_test();
        crate::claude_config_dirs::reset_for_test();
        invalidate();
    }

    const WINDOW_FROM: i64 = 1_767_225_600_000;
    const WINDOW_UNTIL: i64 = 1_767_312_000_000;

    fn scan(context: &crate::LocalSourceContext, account: &Account) -> Value {
        run(context, account, WINDOW_FROM, WINDOW_UNTIL).expect("window scan")
    }

    fn request(account: &Account, from_ms: i64, until_ms: i64) -> Value {
        serde_json::json!({"accountKey":account,"fromMs":from_ms,"untilMs":until_ms,
            "diagnostics":{"intervals":[{"id":"all","fromMs":from_ms,"untilMs":until_ms,
                "includeFrom":true,"includeUntil":false}],"buckets":[]}})
    }

    fn run(
        context: &crate::LocalSourceContext,
        account: &Account,
        from_ms: i64,
        until_ms: i64,
    ) -> Result<Value, String> {
        decode(run_with_context(
            Some(context),
            &request(account, from_ms, until_ms).to_string(),
        )?)
    }

    fn query(context: &crate::LocalSourceContext, request: &Value) -> Result<Value, String> {
        decode(run_with_context(Some(context), &request.to_string())?)
    }

    fn decode(payload: crate::EncodedLocalPayload) -> Result<Value, String> {
        serde_json::from_str(payload.get()).map_err(|error| error.to_string())
    }

    /// The defect issue #258 reports: with a second Claude account configured,
    /// the primary's window folds both accounts' transcripts.
    ///
    /// Asserted on both routes separately. `work-e` is reachable only through
    /// the scan registry and `work-d/sub` only through a `.cc-mirror` variant
    /// naming a directory below every registry entry, so a fix that closed one
    /// route and not the other fails here rather than passing on a machine
    /// that happens to have no mirrors.
    #[test]
    fn primary_window_excludes_every_configured_extra_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());

        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "the primary window folded another account's transcripts: {primary}"
        );
        assert_eq!(
            primary["diagnostics"]["intervals"][0]["unassigned"]["tokens"], PRIMARY_OUTPUT,
            "own-client routing is suggestion-only, not automatic attribution"
        );
        reset_registries();
    }

    /// The control for the test above: without the exclusion the fixture really
    /// does deliver all four roots, so a green result there is the exclusion
    /// working rather than the fixture being inert.
    #[test]
    fn the_fixture_delivers_every_root_before_the_exclusion_is_applied() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());

        let wide = run(&context, &PRIMARY, WINDOW_FROM, WINDOW_UNTIL);
        let _ = wide;
        // Read the roots the way the shipping primary selector does NOT: the
        // unnarrowed options every caller used before this change.
        let options = context.report_options(None, None);
        let usage = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(syrtis_core::get_window_usage(
                options,
                WINDOW_FROM,
                WINDOW_UNTIL,
            ))
            .expect("wide scan");
        let total: i64 = usage.messages.iter().map(|message| message.output).sum();

        assert_eq!(
            total,
            PRIMARY_OUTPUT + D_OUTPUT + E_OUTPUT + SUB_OUTPUT,
            "the fixture does not reach every root, so the exclusion test above \
             could pass without excluding anything"
        );
        reset_registries();
    }

    /// The two registries are installed by separate FFI calls, and
    /// `ClaudeExtraRoots.install` discards the first call's result, so they can
    /// disagree: a directory can reach `tb_set_extra_scan_paths` while
    /// `tb_set_claude_config_dirs` still holds the previous list.
    ///
    /// The primary must still refuse it. Without the registry half of
    /// `primary_exclusions` this is the shape that leaks — and it is the only
    /// shape that does, which is why it needs its own fixture rather than
    /// riding on the one above where both registries agree.
    #[test]
    fn primary_window_excludes_a_root_the_config_dir_registry_never_heard_of() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let orphan = home.join("work-orphan");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&orphan, "orphan", E_OUTPUT);

        // Scan registry knows the account; identity registry does not.
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({
                "claude": [orphan.join("projects").display().to_string()]
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            crate::claude_config_dirs::snapshot().is_empty(),
            "the fixture is meant to leave the identity registry empty"
        );

        let context = crate::LocalSourceContext::for_home(home);
        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "a scan root absent from the config-dir registry was folded into \
             the primary's window: {primary}"
        );
        reset_registries();
    }

    /// Each extra account's window is its own transcripts and nothing else —
    /// not the primary's, and not the other extra account's.
    #[test]
    fn an_extra_account_window_is_exactly_that_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());

        let d = scan(&context, &Some(fixture.d.display().to_string()));
        let e = scan(&context, &Some(fixture.e.display().to_string()));

        assert_eq!(
            output_tokens(&d),
            D_OUTPUT,
            "account D's window is not its own registered roots: {d}"
        );
        // `work-d/sub` belongs to neither window, and that is the intended
        // answer rather than a leak in the other direction. It is a config
        // directory of its own, discovered through a `.cc-mirror` variant and
        // never configured in Syrtis, so it is not the primary's usage and
        // not D's — it is a third account nobody registered. Today it silently
        // inflates the primary's window; counting it for no quota is the honest
        // result, because there is no quota reading to divide it against.
        assert!(
            output_tokens(&d) != D_OUTPUT + SUB_OUTPUT,
            "an unregistered nested account was folded into D's window: {d}"
        );
        assert_eq!(
            output_tokens(&e),
            E_OUTPUT,
            "account E's window is not its own roots: {e}"
        );
        reset_registries();
    }

    /// Restores `TOKSCALE_EXTRA_DIRS` on drop, so a panicking assertion cannot
    /// leave the variable set for whatever test runs next.
    struct ExtraDirsGuard(Option<std::ffi::OsString>);

    impl ExtraDirsGuard {
        fn set(value: &str) -> Self {
            let guard = Self(std::env::var_os("TOKSCALE_EXTRA_DIRS"));
            unsafe { std::env::set_var("TOKSCALE_EXTRA_DIRS", value) };
            guard
        }
    }

    impl Drop for ExtraDirsGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => unsafe { std::env::set_var("TOKSCALE_EXTRA_DIRS", previous) },
                None => unsafe { std::env::remove_var("TOKSCALE_EXTRA_DIRS") },
            }
        }
    }

    /// `TOKSCALE_EXTRA_DIRS` names `claude:` roots of its own, and an extra
    /// account's window must not absorb them.
    ///
    /// This is the only property `use_env_roots: false` buys on this path, and
    /// it is worth pinning precisely because the flag reads as if it bought
    /// more: Claude's declared root is `PathRoot::Home`, which the flag does
    /// not gate at all.
    #[test]
    fn an_extra_account_window_ignores_env_named_roots() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let stray = fixture.home.join("stray");
        write_session(&stray, "stray", SUB_OUTPUT);
        let _env = ExtraDirsGuard::set(&format!("claude:{}", stray.join("projects").display()));

        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let d = scan(&context, &Some(fixture.d.display().to_string()));

        assert_eq!(
            output_tokens(&d),
            D_OUTPUT,
            "an environment-named root was folded into account D's window: {d}"
        );
        reset_registries();
    }

    /// Two CONFIGURED accounts where one directory sits inside the other.
    ///
    /// Settings permits it — `isRejectedRoot` refuses only the empty path, `/`
    /// and the home directory — so `/work` and `/work/sub` can both be real
    /// accounts with real credentials. Selecting an account's roots by prefix
    /// would give the outer one the inner one's transcripts, which is this
    /// issue's defect reappearing on the selection side after being fixed on
    /// the exclusion side.
    ///
    /// Distinct from the nested case in `an_extra_account_window_is_exactly_that_account`:
    /// there the inner directory is registered with neither registry and
    /// belongs to no window, and it takes the other branch entirely.
    #[test]
    fn a_nested_configured_account_is_not_folded_into_the_outer_one() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let outer = home.join("work");
        let inner = outer.join("sub");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&outer, "outer", D_OUTPUT);
        write_session(&inner, "inner", E_OUTPUT);

        let roots: Vec<String> = [&outer, &inner]
            .iter()
            .flat_map(|d| {
                [
                    d.join("projects").display().to_string(),
                    d.join("transcripts").display().to_string(),
                ]
            })
            .collect();
        crate::extra_scan_paths::set_from_json(&serde_json::json!({ "claude": roots }).to_string())
            .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([outer.display().to_string(), inner.display().to_string()])
                .to_string(),
        )
        .unwrap();

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer_window = scan(&context, &Some(outer.display().to_string()));
        let inner_window = scan(&context, &Some(inner.display().to_string()));
        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&outer_window),
            D_OUTPUT,
            "the outer account folded the nested account's transcripts: {outer_window}"
        );
        assert_eq!(
            output_tokens(&inner_window),
            E_OUTPUT,
            "the nested account's window is not its own roots: {inner_window}"
        );
        assert_eq!(
            output_tokens(&primary),
            PRIMARY_OUTPUT,
            "the primary folded a configured account: {primary}"
        );
        reset_registries();
    }

    const INNER_OUTPUT: i64 = 3_000;

    fn claude_roots(dirs: &[&PathBuf]) -> Vec<String> {
        dirs.iter()
            .flat_map(|d| {
                [
                    d.join("projects").display().to_string(),
                    d.join("transcripts").display().to_string(),
                ]
            })
            .collect()
    }

    fn install(roots: &[String], config_dirs: &[&PathBuf]) {
        crate::extra_scan_paths::set_from_json(&serde_json::json!({ "claude": roots }).to_string())
            .unwrap();
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!(config_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>())
            .to_string(),
        )
        .unwrap();
    }

    /// An account configured at `D/.claude` beside `D` itself. `D`'s window is
    /// scanned with `D` as home, and the engine resolves `D/.claude/projects`
    /// on its own, so before the nested-account exclusion `D` read 10,000:
    /// its own 7,000 and the inner account's 3,000, which the inner window
    /// also counted.
    #[test]
    fn an_account_nested_at_dot_claude_is_not_counted_in_the_outer_window() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let inner = d.join(".claude");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&d, "d", D_OUTPUT);
        write_session(&inner, "inner", INNER_OUTPUT);
        install(&claude_roots(&[&d, &inner]), &[&d, &inner]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer = scan(&context, &Some(d.display().to_string()));
        let nested = scan(&context, &Some(inner.display().to_string()));
        let primary = scan(&context, &PRIMARY);

        assert_eq!(
            output_tokens(&outer),
            D_OUTPUT,
            "the outer account counted the account nested at .claude: {outer}"
        );
        assert_eq!(output_tokens(&nested), INNER_OUTPUT, "{nested}");
        assert_eq!(output_tokens(&primary), PRIMARY_OUTPUT, "{primary}");
        reset_registries();
    }

    /// The other half of the nested exclusion: an account at `D/.claude`
    /// whose roots reached the scan registry while its directory did not reach
    /// the config-dir registry. `ClaudeExtraRoots.install` sets the two
    /// separately and the lists can disagree (`primary_exclusions`), so the
    /// config-dir half alone would leave this one counted in D.
    #[test]
    fn a_nested_account_known_only_by_its_roots_is_excluded_too() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let inner = d.join(".claude");
        write_session(&d, "d", D_OUTPUT);
        write_session(&inner, "inner", INNER_OUTPUT);
        install(&claude_roots(&[&d, &inner]), &[&d]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer = scan(&context, &Some(d.display().to_string()));

        assert_eq!(output_tokens(&outer), D_OUTPUT, "{outer}");
        reset_registries();
    }

    /// A configured `D/.claude` that is a symlink back to `D` is nested only as
    /// spelled. The engine canonicalises exclusions, so listing it would cover
    /// D's own roots and read D as zero; it is left out instead.
    #[cfg(unix)]
    #[test]
    fn a_nested_path_that_resolves_to_the_account_itself_is_not_excluded() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        write_session(&d, "d", D_OUTPUT);
        let alias = d.join(".claude");
        std::os::unix::fs::symlink(&d, &alias).unwrap();
        install(&claude_roots(&[&d, &alias]), &[&d, &alias]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let outer = scan(&context, &Some(d.display().to_string()));

        assert_eq!(
            output_tokens(&outer),
            D_OUTPUT,
            "D's own roots were excluded: {outer}"
        );
        reset_registries();
    }

    /// A `.cc-mirror` variant under an extra account's directory may name a
    /// directory anywhere; its rows carry `cc-mirror/<variant>`, and a usage
    /// attribution declaring that id against Claude would put them on this
    /// account's card. The account window asks for exactly `claude`.
    #[test]
    fn an_extra_account_window_reads_only_the_claude_client() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let d = home.join("work-d");
        let elsewhere = home.join("elsewhere");
        write_session(&d, "d", D_OUTPUT);
        write_session(&elsewhere, "mirror", SUB_OUTPUT);
        std::fs::write(
            elsewhere.join("projects/proj/mirror.jsonl"),
            turn("mirror", SUB_OUTPUT).replace("claude-3-5-sonnet", "grok-4.20-mirror"),
        )
        .unwrap();
        let variant = d.join(".cc-mirror").join("v");
        std::fs::create_dir_all(&variant).unwrap();
        std::fs::write(
            variant.join("variant.json"),
            serde_json::json!({ "configDir": elsewhere.to_string_lossy() }).to_string(),
        )
        .unwrap();
        install(&claude_roots(&[&d]), &[&d]);

        let context = crate::LocalSourceContext::for_home(home.clone());
        let mut account_request =
            request(&Some(d.display().to_string()), WINDOW_FROM, WINDOW_UNTIL);
        account_request["includeModelCatalog"] = serde_json::json!(true);
        let window = query(&context, &account_request).unwrap();

        assert_eq!(output_tokens(&window), D_OUTPUT, "{window}");
        assert_eq!(
            window["diagnostics"]["intervals"][0]["all"]["messageCount"],
            1
        );
        assert_eq!(
            window["modelCatalog"],
            serde_json::json!(["claude-3-5-sonnet"]),
            "catalog must use the same exact account-client filter as folds and inference"
        );
        reset_registries();
    }

    /// With nothing nested and no other client under the account, the window
    /// is exactly what the options before the nested-account exclusion
    /// produced: same messages, same undated count.
    #[test]
    fn an_unnested_account_window_is_unchanged_by_the_exclusion() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        for account in [&fixture.d, &fixture.e] {
            let dir = account.display().to_string();
            let before = syrtis_core::ReportOptions {
                home_dir: Some(dir.clone()),
                use_env_roots: false,
                scanner_settings: syrtis_core::scanner::ScannerSettings {
                    extra_scan_paths: BTreeMap::from([(
                        CLAUDE.to_string(),
                        registered_roots_under(&dir),
                    )]),
                    ..Default::default()
                },
                ..Default::default()
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let old = runtime
                .block_on(syrtis_core::get_window_usage(
                    before,
                    WINDOW_FROM,
                    WINDOW_UNTIL,
                ))
                .unwrap();
            let now = scan(&context, &Some(dir.clone()));
            assert!(!old.messages.is_empty(), "{dir}: the control read nothing");
            assert_eq!(
                old.messages.iter().map(|m| m.output).sum::<i64>(),
                output_tokens(&now),
                "{dir}"
            );
            assert_eq!(
                old.messages.len() as u64,
                now["diagnostics"]["intervals"][0]["all"]["messageCount"]
                    .as_u64()
                    .unwrap(),
                "{dir}"
            );
            assert_eq!(
                i64::from(old.undated_count),
                now["undatedCount"].as_i64().unwrap(),
                "{dir}"
            );
        }
        reset_registries();
    }

    /// A configured account whose roots are not registered yet gets a failure,
    /// not an empty window.
    ///
    /// `ClaudeExtraRoots.install` installs the two registries in separate FFI
    /// calls and wakes the quota pollers between them, so a just-added account
    /// is briefly in `claude_config_dirs` and absent from `extra_scan_paths`;
    /// an account whose roots the scan setter refused outright stays that way.
    /// Rendering the empty scan would state that the account used nothing,
    /// which is the same class of wrong answer this file exists to remove.
    #[test]
    fn an_account_with_no_registered_roots_fails_rather_than_reporting_zero() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let pending = home.join("work-pending");
        write_session(&home.join(".claude"), "primary", PRIMARY_OUTPUT);
        write_session(&pending, "pending", D_OUTPUT);

        // The account is configured; its scan roots have not landed.
        crate::claude_config_dirs::set_from_json(
            &serde_json::json!([pending.display().to_string()]).to_string(),
        )
        .unwrap();
        assert!(
            crate::extra_scan_paths::snapshot().is_empty(),
            "the fixture is meant to leave the scan registry empty"
        );

        let context = crate::LocalSourceContext::for_home(home.clone());
        let account = Some(pending.display().to_string());
        let refused = run(&context, &account, WINDOW_FROM, WINDOW_UNTIL);

        assert!(
            refused.is_err(),
            "an unscannable account reported a window instead of failing: {refused:?}"
        );

        // The control: once the roots are registered the same account answers,
        // so the refusal above is about the missing registry entry and not
        // about the account being unreadable in general.
        crate::extra_scan_paths::set_from_json(
            &serde_json::json!({
                "claude": [pending.join("projects").display().to_string()]
            })
            .to_string(),
        )
        .unwrap();
        let answered = run(&context, &account, WINDOW_FROM, WINDOW_UNTIL).expect("registered");
        assert_eq!(output_tokens(&answered), D_OUTPUT);
        reset_registries();
    }

    /// One account's cached window must never answer a request for another's.
    #[test]
    fn a_cached_window_does_not_answer_for_another_account() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let d = Some(fixture.d.display().to_string());

        let first = scan(&context, &PRIMARY);
        let second = scan(&context, &d);
        let mut wrong = request(&d, WINDOW_FROM, WINDOW_UNTIL);
        wrong["snapshotToken"] = first["snapshotToken"].clone();
        assert!(
            query(&context, &wrong).is_err(),
            "primary lease answered extra account"
        );

        assert_eq!(output_tokens(&first), PRIMARY_OUTPUT);
        assert_eq!(
            output_tokens(&second),
            D_OUTPUT,
            "account D was served the primary's cached window: {second}"
        );
        reset_registries();
    }

    #[test]
    fn independent_leases_release_without_rescan_and_preserve_exact_moving_coverage() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let d = Some(fixture.d.display().to_string());
        let before = SCAN_COUNT.load(Ordering::Relaxed);
        let first = scan(&context, &PRIMARY);
        let second = scan(&context, &d);
        let third = scan(&context, &PRIMARY);
        assert_eq!(SCAN_COUNT.load(Ordering::Relaxed) - before, 3);
        let mut followup = request(&PRIMARY, WINDOW_FROM, WINDOW_UNTIL + 999);
        followup["snapshotToken"] = third["snapshotToken"].clone();
        assert!(release(first["snapshotToken"].as_str().unwrap()).unwrap()["released"] == true);
        let answer = query(&context, &followup).unwrap();
        assert_eq!(output_tokens(&answer), PRIMARY_OUTPUT);
        assert_eq!(answer["coverage"]["untilMs"], WINDOW_UNTIL + 999);
        assert_eq!(answer["sourceGeneration"], third["sourceGeneration"]);
        assert_eq!(answer["priceGeneration"], third["priceGeneration"]);
        assert_eq!(SCAN_COUNT.load(Ordering::Relaxed) - before, 3);
        let mut extra = request(&d, WINDOW_FROM, WINDOW_UNTIL);
        extra["snapshotToken"] = second["snapshotToken"].clone();
        assert_eq!(output_tokens(&query(&context, &extra).unwrap()), D_OUTPUT);
        invalidate();
        assert!(query(&context, &followup).is_err());
        assert_eq!(
            SCAN_COUNT.load(Ordering::Relaxed) - before,
            3,
            "expired lease resynchronized"
        );
        reset_registries();
    }

    fn sqlite_row(
        id: &str,
        timestamp: i64,
        model: &str,
        provider: &str,
        lanes: [i64; 5],
        cost: f64,
    ) -> Value {
        serde_json::json!({"id":id,"role":"assistant","modelID":model,"providerID":provider,
            "tokens":{"input":lanes[0],"output":lanes[1],"cache":{"read":lanes[2],"write":lanes[3]},"reasoning":lanes[4]},
            "cost":cost,"time":{"created":timestamp}})
    }

    fn disk_fixture(rows: &[Value]) -> (tempfile::TempDir, Lease) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute_batch("CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,data TEXT NOT NULL);").unwrap();
        for row in rows {
            db.execute(
                "INSERT INTO message VALUES(?1,'session',?2)",
                rusqlite::params![row["id"].as_str().unwrap(), row.to_string()],
            )
            .unwrap();
        }
        drop(db);
        let options = syrtis_core::LocalParseOptions {
            home_dir: Some(dir.path().display().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".into()]),
            scanner_settings: syrtis_core::scanner::ScannerSettings {
                opencode_db_paths: vec![db_path],
                ..Default::default()
            },
            ..Default::default()
        };
        let context = syrtis_core::ResolvedLocalSourceContext::capture(
            Some(dir.path().to_path_buf()),
            false,
            options.scanner_settings.clone(),
        )
        .unwrap();
        let snapshot = syrtis_core::synchronize_local_usage(
            &context,
            options,
            syrtis_core::LocalUsageSelection::LocalClients,
        )
        .unwrap();
        let mut undated_count = 0u32;
        syrtis_core::visit_local_usage_snapshot(
            &snapshot,
            LocalUsageQuery {
                until_ms: Some(1),
                hydrate_opencode_aliases: false,
                ..Default::default()
            },
            &|_| true,
            &mut |_| undated_count += 1,
        )
        .unwrap();
        let bytes = snapshot.metadata_bytes() + std::mem::size_of::<Lease>() + 64;
        (
            dir,
            Lease {
                account: None,
                snapshot,
                pricing: None,
                root_generation: crate::root_generation(),
                lease_id: 7,
                price_generation: 7,
                undated_count,
                bytes,
            },
        )
    }

    fn fixture_fold(request: Value, lease: &Lease) -> Value {
        let request: Request = serde_json::from_value(request).unwrap();
        request.validate().unwrap();
        serde_json::to_value(fold(&request, lease, "fixture".into()).unwrap()).unwrap()
    }

    #[test]
    fn disk_fold_preserves_boundaries_attribution_scope_zones_and_shortened_cycle_evidence() {
        let _guards = lock_registries();
        reset_registries();
        let (_dir, lease) = disk_fixture(&[
            sqlite_row("undated", 0, "grok-4.20-early", "xai", [0; 5], 0.0),
            sqlite_row("opening", 1000, "grok-4.20-early", "xai", [0; 5], 0.0),
            sqlite_row(
                "mid",
                2000,
                "grok-4.20-early",
                "xai",
                [1, 2, 30, 4, 5],
                0.25,
            ),
            sqlite_row("override", 2500, "grok-4.20-excluded", "xai", [0; 5], 0.5),
            sqlite_row("other", 2500, "other", "other", [0; 5], 0.75),
            sqlite_row("unassigned", 2500, "unknown", "unknown", [0; 5], 0.0),
            sqlite_row(
                "reset",
                3000,
                "grok-4.20-late",
                "xai",
                [7, 0, 0, 0, 0],
                0.125,
            ),
            sqlite_row(
                "clip",
                4000,
                "grok-4.20-late",
                "xai",
                [100, 0, 0, 0, 0],
                0.125,
            ),
        ]);
        let mut request = serde_json::json!({"fromMs":1000,"untilMs":5000,
            "includeModelCatalog":true,
            "attribution":[
                {"client":"opencode","provider":"xai","state":"assigned","target":"claude"},
                {"client":"opencode","provider":"xai","model":"grok-4.20-excluded","state":"excluded"},
                {"client":"opencode","provider":"other","state":"assigned","target":"codex"}],
            "modelGroups":[{"modelId":"grok-4.20-early","groupId":"grok-4.20"},
                {"modelId":"grok-4.20-late","groupId":"grok-4.20"}],
            "inference":[{"id":"infer","subscription":"claude","fromMs":1000,"untilMs":4000}],
            "windows":[{"id":"window","subscription":"claude","current":{
                "fromMs":1000,"untilMs":4000,"observed":{"fromMs":1000,"untilMs":4000},
                "zones":[{"index":0,"loMs":1000,"hiMs":2000,"includeLo":true},
                    {"index":1,"loMs":2000,"hiMs":4000,"includeLo":false}]},
                "cycles":[{"evidenceStartMs":1000,"resetAtMs":3000,"firstSampleMs":2000,"lastSampleMs":4000}]}]});
        let result = fixture_fold(request.clone(), &lease);
        assert_eq!(result["undatedCount"], 1);
        assert_eq!(
            result["inference"][0]["firstAssignedMs"], 1000,
            "zero row establishes inference"
        );
        let current = &result["windows"][0]["current"];
        assert_eq!(current["mine"]["messageCount"], 3);
        assert_eq!(current["mine"]["tokens"], 49);
        assert_eq!(current["mine"]["tokensExCacheRead"], 19);
        assert_eq!(
            current["observed"]["tokens"], 49,
            "observed excludes first, clips current end"
        );
        assert_eq!(
            current["zones"][0]["messageCount"], 2,
            "first lower bound inclusive"
        );
        assert_eq!(current["zones"][0]["tokens"], 42);
        assert_eq!(
            current["zones"][1]["tokens"], 7,
            "later lower bound exclusive"
        );
        let cycle = &result["windows"][0]["cycles"][0];
        assert_eq!(cycle["mineTokens"], 42, "history ends before reset");
        assert_eq!(
            cycle["spanTokens"], 107,
            "observed independent of evidence/reset"
        );
        assert_eq!(
            cycle["mineBreakdown"],
            serde_json::json!({"input":1,"output":2,"cacheRead":30,"cacheWrite":4,"reasoning":5})
        );
        assert_eq!(cycle["otherTokens"], 0);
        assert_eq!(cycle["otherCost"], 1.25);
        assert_eq!(cycle["otherHasAssigned"], true);
        assert_eq!(cycle["otherHasExcluded"], true);
        assert_eq!(
            cycle["otherHasUnattributed"], true,
            "zero row carries independent presence"
        );
        assert_eq!(
            cycle["models"][0]["modelId"], "grok-4.20",
            "group only after raw override"
        );
        request["windows"][0]["admittedModelIds"] = serde_json::json!([]);
        let miss = fixture_fold(request.clone(), &lease);
        assert_eq!(miss["windows"][0]["current"]["scopeMatchedNothing"], true);
        assert_eq!(
            miss["windows"][0]["current"]["unscopedAssignedMessageCount"],
            3
        );
        assert_eq!(
            miss["windows"][0]["cycles"][0]["otherHasAssigned"], false,
            "scope precedes other classification"
        );
        assert_eq!(
            miss["inference"][0]["firstAssignedMs"], 1000,
            "inference ignores scope"
        );
        request["windows"][0]["admittedModelIds"] = serde_json::json!(["grok-4.20-early"]);
        request["windows"][0]["current"]["untilMs"] = serde_json::json!(2000);
        let zero = fixture_fold(request, &lease);
        assert_eq!(zero["windows"][0]["current"]["mine"]["messageCount"], 1);
        assert_eq!(
            zero["windows"][0]["current"]["scopeMatchedNothing"], false,
            "zero scoped row is present"
        );
        reset_registries();
    }

    #[test]
    fn disk_fold_keeps_chronological_float_ties_grouping_buckets_and_uncapped_cycles() {
        let _guards = lock_registries();
        reset_registries();
        let (_dir, lease) = disk_fixture(&[
            sqlite_row("a", 1000, "grok-4.20-a", "xai", [1, 0, 0, 0, 0], 1e16),
            sqlite_row("b", 1000, "grok-4.20-b", "xai", [2, 0, 0, 0, 0], 1.0),
            sqlite_row("c", 1000, "grok-4.20-a", "xai", [3, 0, 0, 0, 0], 1.0),
            sqlite_row("d", 2000, "grok-4.20-a", "xai", [4, 0, 0, 0, 0], 1.0),
            sqlite_row("e", 2999, "grok-4.20-a", "xai", [5, 0, 0, 0, 0], 1.0),
        ]);
        let cycles: Vec<_> = (0..40)
            .map(|index| {
                serde_json::json!({"evidenceStartMs":1000,
            "resetAtMs":3000+index,"firstSampleMs":999,"lastSampleMs":2999})
            })
            .collect();
        let result = fixture_fold(
            serde_json::json!({"fromMs":999,"untilMs":4000,
                "attribution":[{"client":"opencode","provider":"xai","state":"assigned","target":"claude"}],
                "modelGroups":[{"modelId":"grok-4.20-a","groupId":"grok-4.20"},{"modelId":"grok-4.20-b","groupId":"grok-4.20"}],
                "windows":[{"id":"w","subscription":"claude","cycles":cycles}],
                "diagnostics":{"intervals":[{"id":"components","fromMs":999,"untilMs":2999,
                    "includeFrom":false,"includeUntil":true,"subscription":"claude"}],
                    "buckets":[{"id":"buckets","fromMs":1000,"untilMs":3000,"bucketMs":700,"count":2}]}
            }),
            &lease,
        );
        let cycles = result["windows"][0]["cycles"].as_array().unwrap();
        assert_eq!(cycles.len(), 40, "probe all-cycle sweep is uncapped");
        assert_eq!(
            cycles[0]["mineCost"], 1e16,
            "sequential chronological float order"
        );
        assert_eq!(
            cycles[0]["models"][0]["cost"], 1e16,
            "group in row order, not merge subtotals"
        );
        assert_eq!(cycles[0]["mineTokens"], 15);
        let diagnostic = &result["diagnostics"]["intervals"][0];
        assert_eq!(diagnostic["components"]["cost"], 1e16);
        assert_eq!(diagnostic["components"]["inputOutputReasoning"], 15.0);
        assert_eq!(
            diagnostic["components"]["incrementalInputOutputReasoning"],
            15
        );
        assert_eq!(
            result["diagnostics"]["buckets"][0]["tokens"],
            serde_json::json!([6, 4])
        );
        assert_eq!(result["diagnostics"]["buckets"][0]["remainderTokens"], 5);
        reset_registries();
    }

    #[test]
    fn reporting_overflow_is_not_signed_saturation_or_aggregate_lane_reconstruction() {
        assert_eq!(row_sum(&[i64::MIN, -1]), i64::MAX);
        assert_eq!(row_sum(&[i64::MAX, 1, -1]), i64::MAX);
        let mut row = UnifiedMessage::new(
            "client",
            "model",
            "provider",
            "session",
            1000,
            syrtis_core::TokenBreakdown {
                input: i64::MAX,
                output: 1,
                cache_read: 1,
                cache_write: -1,
                reasoning: -1,
                ..Default::default()
            },
            0.0,
        );
        row.message_count = 77;
        row.is_turn_start = true;
        let full = row_sum(&[
            row.tokens.input,
            row.tokens.output,
            row.tokens.cache_read,
            row.tokens.cache_write,
            row.tokens.reasoning,
        ]);
        let fresh = row_sum(&[
            row.tokens.input,
            row.tokens.output,
            row.tokens.cache_write,
            row.tokens.reasoning,
        ]);
        let mut counter = Counter::default();
        counter.add(&row, full, fresh);
        assert_eq!(counter.tokens, i64::MAX);
        assert_eq!(counter.tokens_ex_cache_read, i64::MAX);
        assert_ne!(
            counter.tokens_ex_cache_read,
            counter.tokens - counter.breakdown.cache_read
        );
        let mut components = DiagnosticComponents::default();
        components.add(&row, full, fresh);
        assert_eq!(components.incremental_input_output_reasoning, i64::MAX - 1);
        assert_eq!(components.incremental_ex_cache_read, i64::MAX - 2);
        assert_eq!(components.input_output_reasoning_tokens, i64::MAX - 1);
        assert_eq!(
            components.input_output_reasoning_cache_write,
            (i64::MAX - 2) as f64
        );
        let negative = UnifiedMessage::new(
            "client",
            "model",
            "provider",
            "session",
            1001,
            syrtis_core::TokenBreakdown {
                input: i64::MIN,
                ..Default::default()
            },
            0.0,
        );
        counter.add(&negative, i64::MIN, i64::MIN);
        assert_eq!(counter.tokens, -1);
        assert_eq!(counter.breakdown.input, -1);
        assert_eq!(
            counter.message_count, 2,
            "row count is not weighted message_count"
        );
        assert_eq!(counter.turn_count, 1);
    }

    #[test]
    fn disk_row_overflow_preserves_independent_full_and_ex_cache_totals() {
        let _guards = lock_registries();
        reset_registries();
        let (_dir, lease) = disk_fixture(&[sqlite_row(
            "overflow",
            1000,
            "m",
            "p",
            [i64::MAX, 1, 1, 0, 0],
            1.0,
        )]);
        let result = fixture_fold(request(&PRIMARY, 1000, 2000), &lease);
        let all = &result["diagnostics"]["intervals"][0]["all"];
        assert_eq!(all["tokens"], i64::MAX);
        assert_eq!(all["tokensExCacheRead"], i64::MAX);
        assert_eq!(all["breakdown"]["input"], i64::MAX);
        assert_eq!(all["messageCount"], 1);
        assert_eq!(
            result["diagnostics"]["intervals"][0]["components"]["full"],
            i64::MAX as f64
        );
        reset_registries();
    }

    #[test]
    fn lease_count_eviction_and_root_replacement_fail_closed_without_source_work() {
        let _guards = lock_registries();
        reset_registries();
        let fixture = account_fixture();
        let context = crate::LocalSourceContext::for_home(fixture.home.clone());
        let first = scan(&context, &PRIMARY);
        for _ in 0..MAX_LEASES {
            scan(&context, &PRIMARY);
        }
        assert_eq!(LEASES.lock().len(), MAX_LEASES);
        let before = SCAN_COUNT.load(Ordering::Relaxed);
        let mut expired = request(&PRIMARY, WINDOW_FROM, WINDOW_UNTIL);
        expired["snapshotToken"] = first["snapshotToken"].clone();
        assert!(query(&context, &expired).is_err());
        let last = scan(&context, &PRIMARY);
        let mut changed = request(&PRIMARY, WINDOW_FROM, WINDOW_UNTIL);
        changed["snapshotToken"] = last["snapshotToken"].clone();
        let generation = crate::root_generation();
        let payload = std::ffi::CString::new("{}").unwrap();
        let pointer = unsafe { crate::tb_set_extra_scan_paths(payload.as_ptr()) };
        let reply: Value = serde_json::from_str(
            unsafe { std::ffi::CStr::from_ptr(pointer) }
                .to_str()
                .unwrap(),
        )
        .unwrap();
        unsafe { crate::tb_free(pointer) };
        assert_eq!(reply["ok"], true);
        assert!(
            crate::root_generation() > generation,
            "actual setter advances the publication barrier"
        );
        assert!(
            query(&context, &changed).is_err(),
            "root replacement must expire lease"
        );
        assert_eq!(SCAN_COUNT.load(Ordering::Relaxed), before + 1);
        reset_registries();
    }

    fn fixture_pricing(rate: f64) -> Arc<syrtis_core::pricing::PricingService> {
        Arc::new(syrtis_core::pricing::PricingService::new(
            std::collections::HashMap::from([(
                "fixture-model".into(),
                syrtis_core::pricing::ModelPricing {
                    input_cost_per_token: Some(rate),
                    output_cost_per_token: Some(0.0),
                    cache_creation_input_token_cost: Some(0.0),
                    cache_read_input_token_cost: Some(0.0),
                    ..Default::default()
                },
            )]),
            std::collections::HashMap::new(),
        ))
    }

    #[test]
    fn independent_pricing_arcs_remain_pinned_and_shared_arc_is_charged_once() {
        let _guards = lock_registries();
        reset_registries();
        let (dir, mut old) = disk_fixture(&[sqlite_row(
            "row",
            1000,
            "fixture-model",
            "openai",
            [10, 0, 0, 0, 0],
            0.0,
        )]);
        let old_pricing = fixture_pricing(1.0);
        old.pricing = Some(Arc::clone(&old_pricing));
        let new = Lease {
            account: None,
            snapshot: old.snapshot.clone(),
            pricing: Some(fixture_pricing(2.0)),
            root_generation: old.root_generation,
            lease_id: 8,
            price_generation: 8,
            undated_count: old.undated_count,
            bytes: old.bytes,
        };
        let shared = Lease {
            account: None,
            snapshot: old.snapshot.clone(),
            pricing: Some(Arc::clone(&old_pricing)),
            root_generation: old.root_generation,
            lease_id: 9,
            price_generation: 7,
            undated_count: old.undated_count,
            bytes: old.bytes,
        };
        let old_bytes = old.bytes;
        {
            let mut leases = LEASES.lock();
            leases.push_back(("old".into(), Arc::new(old)));
            leases.push_back(("shared".into(), Arc::new(shared)));
            assert_eq!(
                retained_bytes(&leases),
                old_bytes * 2 + old_pricing.retained_bytes()
            );
            leases.push_back(("new".into(), Arc::new(new)));
        }
        let context = crate::LocalSourceContext::for_home(dir.path().to_path_buf());
        let mut req = request(&PRIMARY, 1000, 2000);
        req["snapshotToken"] = serde_json::json!("old");
        let first = query(&context, &req).unwrap();
        assert_eq!(first["diagnostics"]["intervals"][0]["all"]["cost"], 10.0);
        req["snapshotToken"] = serde_json::json!("new");
        assert_eq!(
            query(&context, &req).unwrap()["diagnostics"]["intervals"][0]["all"]["cost"],
            20.0
        );
        release("shared").unwrap();
        req["snapshotToken"] = serde_json::json!("old");
        let again = query(&context, &req).unwrap();
        assert_eq!(
            again["diagnostics"]["intervals"][0]["all"]["cost"], 10.0,
            "new service cannot reprice old lease"
        );
        assert_eq!(again["priceGeneration"], first["priceGeneration"]);
        reset_registries();
    }

    #[test]
    fn pricing_byte_budget_expires_oversized_owner_without_answering_zero_or_rescanning() {
        let _guards = lock_registries();
        reset_registries();
        let (dir, mut large) = disk_fixture(&[]);
        let huge_catalog = Arc::new(syrtis_core::pricing::PricingService::new(
            std::collections::HashMap::from([(
                "x".repeat(MAX_LEASE_BYTES),
                syrtis_core::pricing::ModelPricing::default(),
            )]),
            std::collections::HashMap::new(),
        ));
        assert!(huge_catalog.retained_bytes() > MAX_LEASE_BYTES);
        large.pricing = Some(huge_catalog);
        let normal = Lease {
            account: None,
            snapshot: large.snapshot.clone(),
            pricing: None,
            root_generation: large.root_generation,
            lease_id: 8,
            price_generation: 0,
            undated_count: 0,
            bytes: large.bytes,
        };
        {
            let mut leases = LEASES.lock();
            leases.push_back(("normal".into(), Arc::new(normal)));
            leases.push_back(("large".into(), Arc::new(large)));
        }
        let before = SCAN_COUNT.load(Ordering::Relaxed);
        let context = crate::LocalSourceContext::for_home(dir.path().to_path_buf());
        let mut req = request(&PRIMARY, 1000, 2000);
        req["snapshotToken"] = serde_json::json!("large");
        assert!(query(&context, &req).is_err());
        req["snapshotToken"] = serde_json::json!("normal");
        assert!(
            query(&context, &req).is_ok(),
            "unrelated owner survives oversized lease failure"
        );
        assert_eq!(SCAN_COUNT.load(Ordering::Relaxed), before);
        assert!(retained_bytes(&LEASES.lock()) <= MAX_LEASE_BYTES);
        reset_registries();
    }

    #[test]
    fn failed_source_replacement_keeps_last_good_pinned_disk_facts() {
        let _guards = lock_registries();
        reset_registries();
        let (dir, lease) = disk_fixture(&[sqlite_row(
            "row",
            1000,
            "fixture-model",
            "openai",
            [10, 0, 0, 0, 0],
            1.0,
        )]);
        let original = fixture_fold(request(&PRIMARY, 1000, 2000), &lease);
        let db_path = dir.path().join("opencode.db");
        std::fs::write(&db_path, b"not a SQLite database").unwrap();
        let options = syrtis_core::LocalParseOptions {
            home_dir: Some(dir.path().display().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".into()]),
            scanner_settings: syrtis_core::scanner::ScannerSettings {
                opencode_db_paths: vec![db_path],
                ..Default::default()
            },
            ..Default::default()
        };
        let context = syrtis_core::ResolvedLocalSourceContext::capture(
            Some(dir.path().to_path_buf()),
            false,
            options.scanner_settings.clone(),
        )
        .unwrap();
        assert!(syrtis_core::synchronize_local_usage(
            &context,
            options,
            syrtis_core::LocalUsageSelection::LocalClients
        )
        .is_err());
        let retained = fixture_fold(request(&PRIMARY, 1000, 2000), &lease);
        assert_eq!(retained["diagnostics"], original["diagnostics"]);
        assert_eq!(retained["sourceGeneration"], original["sourceGeneration"]);
        reset_registries();
    }

    #[test]
    fn invalid_or_uncovered_jobs_fail_before_capturing_sources_or_pricing() {
        let _guards = lock_registries();
        reset_registries();
        let dir = tempfile::tempdir().unwrap();
        let context = crate::LocalSourceContext::for_home(dir.path().to_path_buf());
        let before = SCAN_COUNT.load(Ordering::Relaxed);
        let mut invalid = request(&PRIMARY, 1000, 2000);
        invalid["diagnostics"]["intervals"][0]["includeUntil"] = serde_json::json!(true);
        assert!(
            query(&context, &invalid).is_err(),
            "inclusive endpoint is not covered by exclusive outer upper"
        );
        invalid["untilMs"] = serde_json::json!(500);
        assert!(query(&context, &invalid).is_err());
        let buckets: Vec<_> = (0..129)
            .map(|index| {
                serde_json::json!({
            "id":index.to_string(),"fromMs":1000,"untilMs":2000,"bucketMs":1,"count":16384})
            })
            .collect();
        assert!(
            query(
                &context,
                &serde_json::json!({"fromMs":1000,"untilMs":2000,
            "diagnostics":{"buckets":buckets}})
            )
            .is_err(),
            "expanded counter arrays are byte bounded"
        );
        assert_eq!(SCAN_COUNT.load(Ordering::Relaxed), before);
        assert!(LEASES.lock().is_empty());
        reset_registries();
    }
}
