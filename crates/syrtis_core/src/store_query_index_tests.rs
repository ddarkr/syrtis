#[test]
fn repeated_indexed_ranges_preserve_cohorts_aliases_and_callback_publication() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let mut snapshot = publisher
        .begin_scope(
            root.path(),
            &context,
            &["amp".into(), "copilot".into(), "opencode".into()],
            None,
            SelectionMode::LocalClients,
        )
        .unwrap();
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    for (index, client, global_storage) in [
        (0, ClientId::Amp, true),
        (1, ClientId::Copilot, false),
        (2, ClientId::Copilot, true),
    ] {
        let source = home.join(format!("source-{index}"));
        std::fs::write(&source, b"usage-fixture").unwrap();
        let fingerprint = if client == ClientId::Copilot && global_storage {
            SourceFingerprint::from_copilot_desktop_path(&source)
        } else {
            SourceFingerprint::from_path_samples_only(&source)
        }
        .unwrap();
        cache
            .write_source(
                CacheIdentity::for_client(client),
                &source,
                fingerprint,
                None,
                None,
                None,
                &mut |_| {},
                &mut |db, metadata| {
                    publisher.register_cache_on(
                        db,
                        &source,
                        metadata,
                        client.as_str(),
                        index,
                        0,
                        global_storage,
                        false,
                    )
                },
                |writer| {
                    if client == ClientId::Amp {
                        for old in 0..2048 {
                            writer.push(
                                &message("amp", "session", 100, Some(&format!("old-{old}")), 1),
                                false,
                                false,
                            )?;
                        }
                        writer.push(
                            &message("amp", "session", 100, Some("duplicate"), 3),
                            false,
                            false,
                        )?;
                        writer.push(
                            &message("amp", "session", 200, Some("duplicate"), 99),
                            false,
                            false,
                        )?;
                        let mut recent = message("amp", "session", 200, Some("recent"), i64::MAX);
                        recent.tokens.output = 2;
                        writer.push(&recent, false, false)
                    } else {
                        writer.push(
                            &message(
                                "copilot",
                                "authoritative-session",
                                if global_storage { 200 } else { 100 },
                                Some("copilot"),
                                if global_storage { 99 } else { 7 },
                            ),
                            false,
                            false,
                        )
                    }
                },
            )
            .unwrap();
    }
    let db_path = home.join("opencode.db");
    let source = Connection::open(&db_path).unwrap();
    source.execute_batch("CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT); INSERT INTO session VALUES('session','/fixture');").unwrap();
    for (row_id, embedded, cost) in [("row-a", None, 0.0), ("row-b", Some("embedded"), 0.25)] {
        let mut data = serde_json::json!({"role":"assistant","sessionID":"session","modelID":"gpt-4o","providerID":"openai","time":{"created":200.0},"tokens":{"input":5,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"cost":cost});
        if let Some(embedded) = embedded {
            data["id"] = embedded.into();
        }
        source
            .execute(
                "INSERT INTO message VALUES(?1,'session',?2)",
                params![row_id, data.to_string()],
            )
            .unwrap();
    }
    drop(source);
    let oracle = crate::usage_store::materialize_opencode_source(&db_path).unwrap();
    assert_eq!(oracle.len(), 1);
    assert!(!oracle[0].dedup_aliases.is_empty());
    crate::usage_store::synchronize_opencode_sources(
        std::slice::from_ref(&db_path),
        &[],
        root.path(),
        &mut |db, path, sqlite, index, key| {
            publisher.register_opencode(db, path, sqlite, index, key)
        },
    )
    .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    let generation = snapshot.generation;
    let probe = query(root.path());
    let revision: i64 = probe
        .db
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    std::fs::remove_dir_all(&home).unwrap();
    for _ in 0..2 {
        let reader = query(root.path());
        reader.pin(&snapshot).unwrap();
        reader
            .prepare_query(
                &snapshot,
                UsageQuery {
                    from_ms: Some(200),
                    until_ms: Some(201),
                    ..Default::default()
                },
            )
            .unwrap();
        reader.unpin().unwrap();
        assert_eq!(
            reader
                .db
                .query_row("SELECT COUNT(*) FROM sq_rows", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            reader
                .db
                .query_row("SELECT COUNT(*) FROM ocq_rows", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let mut events = Vec::new();
        crate::fold_prepared_usage_snapshot(
            &snapshot,
            &reader,
            None,
            UsageQuery {
                from_ms: Some(200),
                until_ms: Some(201),
                chronological: true,
                ..Default::default()
            },
            &|m| {
                if m.client == "opencode" {
                    assert_eq!(m.dedup_aliases, oracle[0].dedup_aliases);
                }
                true
            },
            &mut |m| events.push(m.clone()),
            true,
        )
        .unwrap();
        assert_eq!(
            events.iter().map(|m| m.client.as_str()).collect::<Vec<_>>(),
            vec!["opencode", "amp"]
        );
        assert_eq!(events[0].cost, 0.25);
        assert_eq!(events[1].tokens.total(), i64::MAX);
        assert_eq!(snapshot.generation, generation);
    }
    assert_eq!(
        revision,
        probe
            .db
            .pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
            .unwrap()
    );
    let predicates = std::cell::Cell::new(0);
    let sinks = std::cell::Cell::new(0);
    let publish_other = || {
        let writer = query(root.path());
        let mut other = writer
            .begin_scope(
                root.path(),
                &context,
                &["unused".into()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        writer.publish(&mut other).unwrap();
    };
    crate::visit_local_usage_snapshot(
        &snapshot,
        UsageQuery {
            from_ms: Some(200),
            until_ms: Some(201),
            ..Default::default()
        },
        &|_| {
            if predicates.replace(predicates.get() + 1) == 0 {
                publish_other();
            }
            true
        },
        &mut |_| {
            if sinks.replace(sinks.get() + 1) == 0 {
                publish_other();
            }
        },
    )
    .unwrap();
    assert!(predicates.get() >= 2);
    assert_eq!(sinks.get(), 2);
    assert!(crate::get_model_report_from_snapshot(
        &snapshot,
        crate::ReportOptions::default(),
        None
    )
    .is_err());
}

#[test]
fn pure_priced_reports_use_only_exact_report_snapshot() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["amp".to_string()];
    let mut snapshot = publisher
        .begin_scope(root.path(), &context, &clients, None, SelectionMode::Report)
        .unwrap();
    let source = home.join("source");
    std::fs::write(&source, b"usage").unwrap();
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    cache
        .write_source(
            CacheIdentity::for_client(ClientId::Amp),
            &source,
            SourceFingerprint::from_path_samples_only(&source).unwrap(),
            None,
            None,
            None,
            &mut |_| {},
            &mut |db, metadata| {
                publisher.register_cache_on(db, &source, metadata, "amp", 0, 0, false, false)
            },
            |writer| {
                writer.push(
                    &message("amp", "session", 1_700_000_000_000, Some("message"), 10),
                    false,
                    false,
                )
            },
        )
        .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    std::fs::remove_dir_all(&home).unwrap();
    let options = crate::ReportOptions {
        clients: Some(clients),
        ..Default::default()
    };
    let pricing = crate::pricing::PricingService::new(
        std::collections::HashMap::from([(
            "model".into(),
            crate::pricing::ModelPricing {
                input_cost_per_token: Some(0.1),
                ..Default::default()
            },
        )]),
        std::collections::HashMap::new(),
    );
    let model =
        crate::get_model_report_from_snapshot(&snapshot, options.clone(), Some(&pricing)).unwrap();
    let hourly =
        crate::get_hourly_report_from_snapshot(&snapshot, options.clone(), Some(&pricing)).unwrap();
    let agents =
        crate::get_agents_report_from_snapshot(&snapshot, options.clone(), Some(&pricing)).unwrap();
    let graph = crate::generate_local_graph_from_snapshot(
        &snapshot,
        options.clone(),
        Some(&pricing),
        crate::GraphPricingMode::BestEffort,
    )
    .unwrap();
    assert_eq!(model.total_input, 10);
    assert_eq!(model.total_cost, 1.0);
    assert_eq!(hourly.total_cost, model.total_cost);
    assert_eq!(agents.total_cost, model.total_cost);
    assert_eq!(graph.graph().summary.total_tokens, 10);
    assert_eq!(
        graph.contract().cost_coverage,
        crate::CostCoverage::Complete
    );
    let local = crate::generate_local_graph_from_snapshot(
        &snapshot,
        options,
        Some(&pricing),
        crate::GraphPricingMode::LocalOnly,
    )
    .unwrap();
    assert_eq!(local.graph().summary.total_cost, 0.0);
    assert!(crate::get_hourly_report_from_snapshot(
        &snapshot,
        crate::ReportOptions::default(),
        Some(&pricing)
    )
    .is_err());
    assert!(crate::get_agents_report_from_snapshot(
        &snapshot,
        crate::ReportOptions::default(),
        Some(&pricing)
    )
    .is_err());
    assert!(crate::generate_local_graph_from_snapshot(
        &snapshot,
        crate::ReportOptions::default(),
        Some(&pricing),
        crate::GraphPricingMode::BestEffort
    )
    .is_err());
}

fn local_fixture(path: &std::path::Path, rows: &[(&str, i64)]) {
    let source = Connection::open(path).unwrap();
    source.execute_batch("CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT); INSERT INTO session VALUES('session','/fixture');").unwrap();
    for (row_id, input) in rows {
        let data = serde_json::json!({"role":"assistant","sessionID":"session","modelID":"gpt-4o","providerID":"openai","time":{"created":200.0},"tokens":{"input":input,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.0});
        source
            .execute(
                "INSERT INTO message VALUES(?1,'session',?2)",
                params![row_id, data.to_string()],
            )
            .unwrap();
    }
}

fn publish_local(
    publisher: &QueryState,
    root: &std::path::Path,
    context: &ResolvedLocalSourceContext,
    clients: &[String],
    amp_input: Option<(i64, i64)>,
    opencode: Option<(&std::path::Path, usize)>,
) -> UsageSnapshot {
    let mut snapshot = publisher
        .begin_scope(root, context, clients, None, SelectionMode::LocalClients)
        .unwrap();
    if let Some((timestamp, input)) = amp_input {
        let cache = SourceMessageCache::open_from_root(Some(root));
        let lane_path = root.join(format!("amp-{}.jsonl", snapshot.generation));
        std::fs::write(&lane_path, b"usage-fixture").unwrap();
        let fingerprint = SourceFingerprint::from_path_samples_only(&lane_path).unwrap();
        cache
            .write_source(
                CacheIdentity::for_client(ClientId::Amp),
                &lane_path,
                fingerprint,
                None,
                None,
                None,
                &mut |_| {},
                &mut |db, metadata| {
                    publisher.register_cache_on(db, &lane_path, metadata, "amp", 0, 0, false, false)
                },
                |writer| {
                    writer.push(
                        &message("amp", "session", timestamp, Some("amp"), input),
                        false,
                        false,
                    )
                },
            )
            .unwrap();
    }
    if let Some((db_path, index)) = opencode {
        crate::usage_store::synchronize_opencode_sources(
            std::slice::from_ref(&db_path.to_path_buf()),
            &[],
            root,
            &mut |db, path, sqlite, order, key| {
                assert_eq!(order, index);
                publisher.register_opencode(db, path, sqlite, order, key)
            },
        )
        .unwrap();
    }
    publisher.publish(&mut snapshot).unwrap();
    snapshot
}

fn consumer_totals(snapshot: &UsageSnapshot) -> (i64, i64, usize) {
    let mut totals = (0, 0, 0);
    crate::visit_local_usage_snapshot(snapshot, UsageQuery::compact(), &|_| true, &mut |message| {
        totals.0 += message.tokens.input;
        totals.1 += message.tokens.output;
        totals.2 += 1;
    })
    .unwrap();
    totals
}

fn consumer_inputs(snapshot: &UsageSnapshot) -> Vec<i64> {
    let mut inputs = Vec::new();
    crate::visit_local_usage_snapshot(snapshot, UsageQuery::compact(), &|_| true, &mut |message| {
        inputs.push(message.tokens.input);
    })
    .unwrap();
    inputs
}

fn consumer_providers(snapshot: &UsageSnapshot) -> Vec<String> {
    let mut providers = Vec::new();
    crate::visit_local_usage_snapshot(snapshot, UsageQuery::compact(), &|_| true, &mut |message| {
        providers.push(message.provider_id.clone());
    })
    .unwrap();
    providers
}

#[test]
fn unchanged_opencode_reuses_decisions_while_other_lane_moves() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["amp".to_string(), "opencode".to_string()];
    let db_path = home.join("opencode.db");
    local_fixture(&db_path, &[("row-a", 5)]);
    let first = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        Some((100, 7)),
        Some((&db_path, 0)),
    );
    assert_eq!(consumer_totals(&first), (12, 2, 2));
    assert_eq!(consumer_inputs(&first), vec![5, 7]);
    let second = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        Some((300, 11)),
        Some((&db_path, 0)),
    );
    assert_eq!(consumer_totals(&second), (16, 2, 2));
    assert_eq!(consumer_inputs(&second), vec![5, 11]);
}

#[test]
fn changed_opencode_source_order_and_kind_rebuild_decisions() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["opencode".to_string()];
    let first_db = home.join("first.db");
    local_fixture(&first_db, &[("row-a", 5)]);
    let second_db = home.join("second.db");
    local_fixture(&second_db, &[("row-b", 9)]);
    // Disjoint rows always both emit in traversal order, so swapping the
    // source order must flip the public emitted order even though totals
    // stay identical. A reuse that ignored source order would keep [5, 9].
    let pair = |order: &[std::path::PathBuf]| {
        let mut snapshot = publisher
            .begin_scope(
                root.path(),
                &context,
                &clients,
                None,
                SelectionMode::LocalClients,
            )
            .unwrap();
        crate::usage_store::synchronize_opencode_sources(
            order,
            &[],
            root.path(),
            &mut |db, path, sqlite, index, key| {
                publisher.register_opencode(db, path, sqlite, index, key)
            },
        )
        .unwrap();
        publisher.publish(&mut snapshot).unwrap();
        snapshot
    };
    let ordered = pair(&[first_db.clone(), second_db.clone()]);
    assert_eq!(consumer_totals(&ordered), (14, 4, 2));
    assert_eq!(consumer_inputs(&ordered), vec![5, 9]);
    let swapped = pair(&[second_db.clone(), first_db.clone()]);
    assert_eq!(consumer_totals(&swapped), (14, 4, 2));
    assert_eq!(consumer_inputs(&swapped), vec![9, 5]);
    // A new native path with the same row id is a different dependency.
    let lone = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        None,
        Some((&first_db, 0)),
    );
    assert_eq!(consumer_inputs(&lone), vec![5]);
    // Same path through the JSON lane carries different fingerprints/generations.
    let json_path = home.join("session.json");
    std::fs::write(&json_path, br#"{"role":"assistant","sessionID":"session","modelID":"gpt-4o","providerID":"openai","time":{"created":200.0},"tokens":{"input":13,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.0}"#).unwrap();
    let mut snapshot = publisher
        .begin_scope(
            root.path(),
            &context,
            &clients,
            None,
            SelectionMode::LocalClients,
        )
        .unwrap();
    crate::usage_store::synchronize_opencode_sources(
        &[],
        std::slice::from_ref(&json_path),
        root.path(),
        &mut |db, path, sqlite, order, key| {
            assert!(!sqlite);
            publisher.register_opencode(db, path, sqlite, order, key)
        },
    )
    .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    assert_eq!(consumer_totals(&snapshot), (13, 2, 1));
}
#[test]
fn empty_opencode_matches_only_empty_and_evicts_old_generations() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["amp".to_string(), "opencode".to_string()];
    let first = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        Some((100, 7)),
        None,
    );
    assert_eq!(consumer_totals(&first), (7, 0, 1));
    let second = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        Some((300, 11)),
        None,
    );
    assert_eq!(consumer_totals(&second), (11, 0, 1));
    let db_path = home.join("opencode.db");
    local_fixture(&db_path, &[("row-a", 5)]);
    let third = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        Some((400, 13)),
        Some((&db_path, 0)),
    );
    assert_eq!(consumer_totals(&third), (18, 2, 2));
    assert_eq!(consumer_inputs(&third), vec![5, 13]);
    // The superseded empty generation leaves no decision/index rows once the
    // newer non-empty publication retires it, and the old snapshot is stale.
    let probe = query(root.path());
    let decisions: i64 = probe
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_decisions WHERE scope_generation=?1",
            [second.generation],
            |r| r.get(0),
        )
        .unwrap();
    let indexes: i64 = probe
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_indexes WHERE scope_generation=?1",
            [second.generation],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((decisions, indexes), (0, 0));
    assert!(probe.pin(&second).is_err());
}
#[test]
fn opencode_provider_alias_change_rebuilds_decisions() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["opencode".to_string()];
    let db_path = home.join("opencode.db");
    local_fixture(&db_path, &[("row-a", 5)]);
    let first = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        None,
        Some((&db_path, 0)),
    );
    assert_eq!(consumer_totals(&first), (5, 2, 1));
    assert_eq!(consumer_providers(&first), vec!["openai".to_string()]);
    // A provider-only rewrite changes the collapsed fingerprint/generation, so
    // the next publication must surface the new provider authority.
    let source = Connection::open(&db_path).unwrap();
    let aliased = serde_json::json!({"role":"assistant","sessionID":"session","modelID":"gpt-4o","providerID":"anthropic","time":{"created":200.0},"tokens":{"input":5,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.0});
    source
        .execute(
            "UPDATE message SET data=?1 WHERE id='row-a'",
            [aliased.to_string()],
        )
        .unwrap();
    drop(source);
    let second = publish_local(
        &publisher,
        root.path(),
        &context,
        &clients,
        None,
        Some((&db_path, 0)),
    );
    assert_eq!(consumer_totals(&second), (5, 2, 1));
    assert_eq!(consumer_providers(&second), vec!["anthropic".to_string()]);
}

fn report_snapshot(
    publisher: &QueryState,
    root: &std::path::Path,
    context: &ResolvedLocalSourceContext,
    clients: &[String],
) -> UsageSnapshot {
    // Stage through the real ingestion path (scan + stage + publish-prepare)
    // so lane discovery, fingerprints, and scope descriptors match production.
    // Tests seed the home dir with the same layouts the scanner discovers.
    let mut snapshot = publisher
        .begin_scope(root, context, clients, None, SelectionMode::Report)
        .unwrap();
    let mut cache = SourceMessageCache::open_from_root(Some(root));
    cache.ensure_open().unwrap();
    let scan = crate::scanner::scan_all_clients_with_source_context(context, clients).unwrap();
    for (index, path) in scan.get(ClientId::Claude).iter().enumerate() {
        crate::store_query::stage_claude(publisher, &mut cache, path, context.home_dir(), index)
            .unwrap();
    }
    let headless = crate::scanner::headless_roots_with_source_context(context).unwrap();
    for (index, path) in scan.get(ClientId::Codex).iter().enumerate() {
        let is_headless = headless.iter().any(|root| path.starts_with(root));
        crate::store_query::stage_codex(publisher, &mut cache, path, index, is_headless).unwrap();
    }
    cache.check().unwrap();
    publisher.publish(&mut snapshot).unwrap();
    snapshot
}

fn report_inputs(snapshot: &UsageSnapshot) -> Vec<i64> {
    let mut inputs = Vec::new();
    crate::visit_local_usage_snapshot(snapshot, UsageQuery::compact(), &|_| true, &mut |message| {
        inputs.push(message.tokens.input);
    })
    .unwrap();
    inputs
}

fn write_report_claude(path: &std::path::Path, turns: &[(&str, i64)]) {
    let mut body = String::new();
    for (id, input) in turns {
        body.push_str(&serde_json::json!({"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":id,"message":{"id":id,"model":"claude-3-5-sonnet","usage":{"input_tokens":input,"output_tokens":2}}}).to_string());
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

fn write_report_codex(path: &std::path::Path, session: &str, input: i64) {
    // Same line shapes as the canonical `codex_plain_fixture` in lib.rs tests:
    // session_meta + turn_context + one cumulative token_count event.
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":session,"source":"interactive","model_provider":"openai"}}),
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":0,"output_tokens":2},"last_token_usage":{"input_tokens":input,"cached_input_tokens":0,"output_tokens":2}}}}),
        ),
    )
    .unwrap();
}

#[test]
fn repeated_mixed_report_queries_reuse_indexed_winners() {
    // Same generation, mixed Claude/Codex + OpenCode scope, repeated
    // graph/models/hourly/agents folds: Claude/Codex winners come from the
    // exact-generation durable index (loser replay/reselection skipped),
    // OpenCode arbitrates fresh each time, and all four consumers agree.
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-lazy")).unwrap();
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let clients = vec![
        "claude".to_string(),
        "codex".to_string(),
        "opencode".to_string(),
    ];
    // Same message/request id in every row => same dedup key across two
    // canonical sources. Scans sort lexically, so the early source stages
    // first (source_order 0) and cross-source arbitration keeps the earlier
    // row: the early source's 12 wins while the later source's same-key
    // duplicates stay canonical losers. The indexed repeat must emit
    // exactly the winner.
    std::fs::create_dir_all(home.join(".claude/projects/-home-early")).unwrap();
    let early = home.join(".claude/projects/-home-early/session.jsonl");
    let late = home.join(".claude/projects/-home-lazy/session.jsonl");
    write_report_claude(&early, &[("turn-a", 12)]);
    write_report_claude(&late, &[("turn-a", 10), ("turn-a", 11)]);
    let rollout = home.join(".codex/sessions/a.jsonl");
    write_report_codex(&rollout, "session-a", 30);
    let db_path = home.join("opencode.db");
    local_fixture(&db_path, &[("row-a", 5)]);
    let publisher = query(root.path());
    let mut snapshot = publisher
        .begin_scope(root.path(), &context, &clients, None, SelectionMode::Report)
        .unwrap();
    let mut cache = SourceMessageCache::open_from_root(Some(root.path()));
    cache.ensure_open().unwrap();
    let scan = crate::scanner::scan_all_clients_with_source_context(&context, &clients).unwrap();
    for (index, path) in scan.get(ClientId::Claude).iter().enumerate() {
        crate::store_query::stage_claude(&publisher, &mut cache, path, context.home_dir(), index)
            .unwrap();
    }
    let headless = crate::scanner::headless_roots_with_source_context(&context).unwrap();
    for (index, path) in scan.get(ClientId::Codex).iter().enumerate() {
        let is_headless = headless.iter().any(|root| path.starts_with(root));
        crate::store_query::stage_codex(&publisher, &mut cache, path, index, is_headless).unwrap();
    }
    crate::usage_store::synchronize_opencode_sources(
        std::slice::from_ref(&db_path),
        &[],
        root.path(),
        &mut |db, path, sqlite, order, key| {
            publisher.register_opencode(db, path, sqlite, order, key)
        },
    )
    .unwrap();
    cache.check().unwrap();
    publisher.publish(&mut snapshot).unwrap();
    // Only the winner survives: the later source's same-key rows stay
    // canonical losers, distinguished from the early winner in the output.
    let mut first = report_inputs(&snapshot);
    first.sort_unstable();
    assert_eq!(first, vec![5, 12, 30]);
    // Distinct Report marker/version: LocalClients ownership untouched.
    let probe = query(root.path());
    let version: i64 = probe
        .db
        .query_row(
            "SELECT version FROM sq_indexes WHERE scope_generation=?1",
            [snapshot.generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 2);
    let winners: i64 = probe
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_decisions WHERE scope_generation=?1 AND lane IN ('claude','codex') AND oc_source IS NULL",
            [snapshot.generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(winners, 2);
    let local_rows: i64 = probe
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_decisions WHERE scope_generation=?1 AND lane NOT IN ('claude','codex')",
            [snapshot.generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local_rows, 0);
    // The index is predicate-independent: a rejecting predicate folds
    // nothing while an accepting one folds the same winners.
    for accept in [true, false] {
        let mut seen = Vec::new();
        crate::visit_local_usage_snapshot(
            &snapshot,
            UsageQuery::compact(),
            &|_| accept,
            &mut |message| seen.push(message.tokens.input),
        )
        .unwrap();
        if accept {
            seen.sort_unstable();
            assert_eq!(seen, vec![5, 12, 30]);
        } else {
            assert!(seen.is_empty());
        }
    }
    // Repeated folds agree across all four trusted consumers.
    let options = crate::ReportOptions {
        clients: Some(clients),
        ..Default::default()
    };
    for _ in 0..2 {
        let model =
            crate::get_model_report_from_snapshot(&snapshot, options.clone(), None).unwrap();
        let agents =
            crate::get_agents_report_from_snapshot(&snapshot, options.clone(), None).unwrap();
        let hourly =
            crate::get_hourly_report_from_snapshot(&snapshot, options.clone(), None).unwrap();
        let graph = crate::generate_local_graph_from_snapshot(
            &snapshot,
            options.clone(),
            None,
            crate::GraphPricingMode::LocalOnly,
        )
        .unwrap();
        assert_eq!(model.total_input, 47);
        assert_eq!(agents.total_messages, model.total_messages);
        assert_eq!(hourly.total_cost, model.total_cost);
        assert_eq!(
            graph.graph().summary.total_tokens,
            model.total_input + model.total_output
        );
    }
    // Reuse actually skips loser arbitration: a fresh preparation of the
    // same generation holds only the indexed winners in Claude/Codex
    // scratch, while the canonical source still stores the duplicate
    // losers — yet folds agree exactly with the full prepare above.
    let fresh = query(root.path());
    fresh.pin(&snapshot).unwrap();
    fresh
        .prepare_query(&snapshot, UsageQuery::compact())
        .unwrap();
    let scratch_cc: i64 = fresh
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_rows WHERE lane IN ('claude','codex')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(scratch_cc, winners);
    let canonical_cc: i64 = fresh
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_scope_sources s JOIN mc_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') AND s.lane IN ('claude','codex')",
            [snapshot.generation],
            |row| row.get(0),
        )
        .unwrap();
    assert!(canonical_cc > scratch_cc);
    fresh.unpin().unwrap();
    // Deterministic behavior proof the index is consumed: corrupt one
    // canonical Claude/Codex payload and the repeated fold fails whole
    // with no partial output. The corruption hits an indexed winner row,
    // so the failure proves the fold replays the durable index (a full
    // re-parse of the live transcript would emit winners instead). The
    // checksum gate, not text matching, couples the regression to output.
    // A missing canonical loser row would equally trip the completeness
    // gate before any callback; the payload corruption here exercises the
    // same whole-failure path through an actually present row.
    let cache_path = root.path().join("source-message-cache.sqlite3");
    let (source_id, generation, ordinal, message, digest) = rusqlite::Connection::open(&cache_path)
        .unwrap()
        .query_row(
            "SELECT d.cache_source_id, d.cache_generation, d.cache_ordinal, r.message, r.digest FROM sq_decisions d JOIN mc_rows r ON r.source_id=d.cache_source_id AND r.generation=d.cache_generation AND r.ordinal=d.cache_ordinal WHERE d.scope_generation=?1 AND d.lane IN ('claude','codex') AND d.oc_source IS NULL ORDER BY d.rank LIMIT 1",
            [snapshot.generation],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .unwrap();
    rusqlite::Connection::open(&cache_path)
        .unwrap()
        .execute(
            "UPDATE mc_rows SET message = zeroblob(4), digest = zeroblob(32) WHERE source_id = ?1 AND generation = ?2 AND ordinal = ?3",
            rusqlite::params![source_id, generation, ordinal],
        )
        .unwrap();
    let mut seen = 0usize;
    let result =
        crate::visit_local_usage_snapshot(&snapshot, UsageQuery::compact(), &|_| true, &mut |_| {
            seen += 1
        });
    assert!(result.is_err());
    assert_eq!(seen, 0);
    // Deleting a genuine canonical loser row trips the same completeness
    // gate before any callback: restore the winner payload, drop one
    // non-winner row of the later source, and the fold still fails whole
    // with no output.
    rusqlite::Connection::open(&cache_path)
        .unwrap()
        .execute(
            "UPDATE mc_rows SET message = ?3, digest = ?4 WHERE source_id = ?1 AND generation = ?2 AND ordinal IN (SELECT d.cache_ordinal FROM sq_decisions d WHERE d.scope_generation = ?5 AND d.cache_source_id = ?1 AND d.cache_generation = ?2)",
            rusqlite::params![source_id, generation, message, digest, snapshot.generation],
        )
        .unwrap();
    let loser: Option<(i64, i64, i64)> = rusqlite::Connection::open(&cache_path)
        .unwrap()
        .query_row(
            "SELECT r.source_id, r.generation, r.ordinal FROM sq_scope_sources s JOIN mc_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') AND s.lane IN ('claude','codex') AND NOT EXISTS(SELECT 1 FROM sq_decisions d WHERE d.scope_generation=?1 AND d.cache_source_id=r.source_id AND d.cache_generation=r.generation AND d.cache_ordinal=r.ordinal) LIMIT 1",
            [snapshot.generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap();
    let (loser_source, loser_generation, loser_ordinal) =
        loser.expect("later source keeps canonical loser rows beside the early winner");
    rusqlite::Connection::open(&cache_path)
        .unwrap()
        .execute(
            "DELETE FROM mc_rows WHERE source_id = ?1 AND generation = ?2 AND ordinal = ?3",
            rusqlite::params![loser_source, loser_generation, loser_ordinal],
        )
        .unwrap();
    let mut loser_seen = 0usize;
    let loser_result =
        crate::visit_local_usage_snapshot(&snapshot, UsageQuery::compact(), &|_| true, &mut |_| {
            loser_seen += 1
        });
    assert!(loser_result.is_err());
    assert_eq!(loser_seen, 0);
    // No mutable canonical writes during pure queries: the failed fold
    // leaves the durable index and canonical rows byte-identical.
    let probe = query(root.path());
    let winners_after: i64 = probe
        .db
        .query_row(
            "SELECT COUNT(*) FROM sq_decisions WHERE scope_generation=?1 AND lane IN ('claude','codex') AND oc_source IS NULL",
            [snapshot.generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(winners_after, winners);
}

#[test]
fn unchanged_report_republish_keeps_identical_winners() {
    // A collapsed unchanged re-publication finds the same scope generation
    // and its v2 marker: the second fold emits byte-identical winners, so
    // the build-once early return is observed through output, not marker
    // text. Deleting the marker still folds identically via full prepare.
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-lazy")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let clients = vec!["claude".to_string()];
    let transcript = home.join(".claude/projects/-home-lazy/session.jsonl");
    write_report_claude(&transcript, &[("turn-a", 10), ("turn-a", 11)]);
    let publisher = query(root.path());
    let first = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(report_inputs(&first), vec![11]);
    let publisher = query(root.path());
    let second = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(second.generation, first.generation);
    assert_eq!(report_inputs(&second), vec![11]);
    query(root.path())
        .db
        .execute(
            "DELETE FROM sq_indexes WHERE scope_generation=?1 AND version=2",
            [second.generation],
        )
        .unwrap();
    assert_eq!(report_inputs(&second), vec![11]);
}

#[test]
fn report_reuse_keeps_corrections_and_retention() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-lazy")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let clients = vec!["claude".to_string()];
    let transcript = home.join(".claude/projects/-home-lazy/session.jsonl");
    // A same-key rewrite corrects the turn: the later input wins.
    write_report_claude(&transcript, &[("same-turn", 10)]);
    let publisher = query(root.path());
    let first = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(report_inputs(&first), vec![10]);
    write_report_claude(&transcript, &[("same-turn", 99)]);
    let publisher = query(root.path());
    let corrected = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(report_inputs(&corrected), vec![99]);
    // A compacting rewrite drops the live turn; retention keeps it. The
    // retained turn keeps its corrected input (99), plus the new live turn.
    write_report_claude(&transcript, &[("other-turn", 5)]);
    let publisher = query(root.path());
    let second = report_snapshot(&publisher, root.path(), &context, &clients);
    let mut inputs = report_inputs(&second);
    inputs.sort_unstable();
    assert_eq!(inputs, vec![5, 99]);
    // Deleting the only source empties the scope: no live rows and no
    // carried cohorts under the bare test helper (production carries durable
    // identities via carry_absent_durable_sources, covered by the durable
    // lifecycle test in lib.rs). The fold must be empty, not stale.
    std::fs::remove_file(&transcript).unwrap();
    let publisher = query(root.path());
    let third = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(report_inputs(&third), Vec::<i64>::new());
    // The empty selected set still records the marker: completeness, not a
    // full rebuild, and a repeated fold stays empty.
    let probe = query(root.path());
    let ready: bool = probe
        .db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sq_indexes WHERE scope_generation=?1 AND version=2)",
            [third.generation],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ready);
    assert_eq!(report_inputs(&third), Vec::<i64>::new());
}

#[test]
fn report_index_rejects_stale_handles_and_corrupt_payloads() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-lazy")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let clients = vec!["claude".to_string()];
    let transcript = home.join(".claude/projects/-home-lazy/session.jsonl");
    write_report_claude(&transcript, &[("turn-a", 10)]);
    let publisher = query(root.path());
    let first = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(report_inputs(&first), vec![10]);
    // Account/scope difference: another client set is another scope, so a
    // repeated fold of the first scope still returns its own winners.
    let other_clients = vec!["codex".to_string()];
    let publisher = query(root.path());
    let other = report_snapshot(&publisher, root.path(), &context, &other_clients);
    assert!(report_inputs(&other).is_empty());
    assert_eq!(report_inputs(&first), vec![10]);
    // A changed source supersedes the first generation: pinning the old
    // handle rejects once a newer generation publishes. Retention keeps the
    // dropped turn alongside the new one.
    write_report_claude(&transcript, &[("turn-b", 20)]);
    let publisher = query(root.path());
    let second = report_snapshot(&publisher, root.path(), &context, &clients);
    let mut inputs = report_inputs(&second);
    inputs.sort_unstable();
    assert_eq!(inputs, vec![10, 20]);
    assert!(query(root.path()).pin(&first).is_err());
    // Corrupt a canonical payload: the next fold fails whole with no
    // partial output, and the durable index never masks the error.
    let db_path = root.path().join("source-message-cache.sqlite3");
    let victim: Option<(i64, i64, i64)> = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT source_id, generation, ordinal FROM mc_rows ORDER BY source_id DESC, ordinal DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap();
    if let Some((source_id, generation, ordinal)) = victim {
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE mc_rows SET message = zeroblob(4), digest = zeroblob(32) WHERE source_id = ?1 AND generation = ?2 AND ordinal = ?3",
                rusqlite::params![source_id, generation, ordinal],
            )
            .unwrap();
        let mut seen = 0usize;
        let result = crate::visit_local_usage_snapshot(
            &second,
            UsageQuery::compact(),
            &|_| true,
            &mut |_| seen += 1,
        );
        assert!(result.is_err());
        assert_eq!(seen, 0);
    }
}

#[test]
fn filtered_opencode_json_fallback_still_arbitrates_fresh_per_report() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-lazy")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let transcript = home.join(".claude/projects/-home-lazy/session.jsonl");
    write_report_claude(&transcript, &[("turn-a", 10)]);
    // Same shared id in both lanes: the sqlite row carries model
    // "plain-model", the JSON twin carries "filtered-model". Report mode
    // falls back to the sqlite row when the predicate rejects the JSON twin.
    let db_path = home.join("opencode.db");
    let source = rusqlite::Connection::open(&db_path).unwrap();
    source.execute_batch("CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT); INSERT INTO session VALUES('session','/fixture');").unwrap();
    let sqlite = serde_json::json!({"role":"assistant","id":"shared","sessionID":"session","modelID":"plain-model","providerID":"openai","time":{"created":1700000000000.0},"tokens":{"input":5,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.0});
    source
        .execute(
            "INSERT INTO message VALUES('row','session',?1)",
            rusqlite::params![sqlite.to_string()],
        )
        .unwrap();
    drop(source);
    let json_path = home.join("shared.json");
    std::fs::write(
        &json_path,
        serde_json::json!({"role":"assistant","id":"shared","sessionID":"session","modelID":"filtered-model","providerID":"openai","time":{"created":1700000000000.0},"tokens":{"input":7,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.75}).to_string(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["claude".to_string(), "opencode".to_string()];
    let mut snapshot = publisher
        .begin_scope(root.path(), &context, &clients, None, SelectionMode::Report)
        .unwrap();
    crate::usage_store::synchronize_opencode_sources(
        std::slice::from_ref(&db_path),
        std::slice::from_ref(&json_path),
        root.path(),
        &mut |db, path, sqlite, index, key| {
            publisher.register_opencode(db, path, sqlite, index, key)
        },
    )
    .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    // Same scope, different report predicates: the JSON fallback follows the
    // predicate each time instead of reusing a stale arbitration.
    let plain: Vec<String> = {
        let mut models = Vec::new();
        crate::visit_local_usage_snapshot(
            &snapshot,
            UsageQuery::compact(),
            &|message| message.model_id == "plain-model",
            &mut |message| models.push(message.model_id.clone()),
        )
        .unwrap();
        models
    };
    assert_eq!(plain, vec!["plain-model".to_string()]);
    let all: Vec<String> = {
        let mut models = Vec::new();
        crate::visit_local_usage_snapshot(
            &snapshot,
            UsageQuery::compact(),
            &|_| true,
            &mut |message| models.push(message.model_id.clone()),
        )
        .unwrap();
        models.sort();
        models
    };
    // With no predicate the JSON twin wins (provider-reported cost): the
    // sqlite fallback only surfaces when the predicate rejects the twin.
    assert!(all.contains(&"filtered-model".to_string()));
}

#[test]
fn synthetic_only_scope_survives_shared_dedup_key() {
    // Exact synthetic-only scopes may scan all clients: a reports-stage
    // ordinary row and a later gateway row share one Claude-lane dedup key
    // across two canonical sources. Snapshot eligibility precedes
    // arbitration, so the synthetic-only repeated fold keeps the gateway
    // turn, not the ordinary one.
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-ordinary")).unwrap();
    std::fs::create_dir_all(home.join(".claude/projects/-home-gateway")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let clients = vec!["synthetic".to_string()];
    let ordinary = home.join(".claude/projects/-home-ordinary/session.jsonl");
    let gateway = home.join(".claude/projects/-home-gateway/session.jsonl");
    let body = |model: &str, input: i64| {
        serde_json::json!({"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"shared-turn","message":{"id":"shared-turn","model":model,"usage":{"input_tokens":input,"output_tokens":2}}})
            .to_string()
            + "\n"
    };
    std::fs::write(&ordinary, body("claude-3-5-sonnet", 10)).unwrap();
    std::fs::write(&gateway, body("hf:org/synthetic-model", 20)).unwrap();
    let publisher = query(root.path());
    let snapshot = report_snapshot(&publisher, root.path(), &context, &clients);
    assert_eq!(report_inputs(&snapshot), vec![20]);
    let mut rejected = Vec::new();
    crate::visit_local_usage_snapshot(
        &snapshot,
        UsageQuery::compact(),
        &|_| false,
        &mut |message| rejected.push(message.tokens.input),
    )
    .unwrap();
    assert!(rejected.is_empty());
}
#[test]
fn metadata_report_index_matches_full_replay_and_rejects_loser_loss() {
    // Uncertain edge: the metadata-only cold replay must arbitrate exactly
    // like the full payload replay (same winner order/references), and a
    // lost loser row must fail the repeated fold whole before any callback —
    // proving losers still gate completeness. Winner corruption is covered
    // by repeated_mixed_report_queries_reuse_indexed_winners; the empty
    // marker case by report_reuse_keeps_corrections_and_retention.
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-home-early")).unwrap();
    std::fs::create_dir_all(home.join(".claude/projects/-home-late")).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let clients = vec!["claude".to_string(), "codex".to_string()];
    // Same dedup key across two claude sources: early source wins, late rows
    // lose. The codex lane contributes one uncontested winner, so the
    // metadata replay must arbitrate both lanes and both eligibility inputs.
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    write_report_codex(&home.join(".codex/sessions/rollout.jsonl"), "session-a", 30);
    write_report_claude(
        &home.join(".claude/projects/-home-early/session.jsonl"),
        &[("shared-turn", 12)],
    );
    write_report_claude(
        &home.join(".claude/projects/-home-late/session.jsonl"),
        &[("shared-turn", 10), ("shared-turn", 11)],
    );
    let publisher = query(root.path());
    let snapshot = report_snapshot(&publisher, root.path(), &context, &clients);
    let mut folded = report_inputs(&snapshot);
    folded.sort_unstable();
    assert_eq!(folded, vec![12, 30]);
    // Differential proof: rebuild the same generation's arbitration from the
    // full payload replay and require identical winner references in order.
    let indexed: Vec<(String, i64, i64, i64)> = query(root.path())
        .db
        .prepare(
            "SELECT lane, cache_source_id, cache_generation, cache_ordinal FROM sq_decisions WHERE scope_generation=?1 AND lane IN ('claude','codex') AND oc_source IS NULL ORDER BY rank",
        )
        .unwrap()
        .query_map([snapshot.generation], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(indexed.len(), 2);
    let verifier = query(root.path());
    verifier.pin(&snapshot).unwrap();
    // Full payload arbitration of the same generation, without touching the
    // durable index: replay all canonical rows, apply the same Report
    // eligibility, and arbitrate both lanes exactly as publication did.
    verifier.replay_snapshot(&snapshot).unwrap();
    verifier.set_client_eligibility(&snapshot.clients).unwrap();
    verifier
        .select_lane("claude", SelectionMode::Report, false)
        .unwrap();
    verifier
        .select_lane("codex", SelectionMode::Report, false)
        .unwrap();
    let replayed: Vec<(String, i64, i64, i64)> = verifier
        .db
        .prepare(
            "SELECT r.lane, r.cache_source_id, r.cache_generation, r.cache_ordinal FROM sq_rows r JOIN sq_selected s ON s.id=r.id WHERE r.lane IN ('claude','codex') AND r.cache_source_id IS NOT NULL ORDER BY s.rank",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    verifier.unpin().unwrap();
    assert_eq!(replayed, indexed);
    // Deleting one canonical loser row trips the completeness gate before
    // any callback, even though the repeated fold only copies winners.
    let cache_path = root.path().join("source-message-cache.sqlite3");
    let loser: Option<(i64, i64, i64)> = rusqlite::Connection::open(&cache_path)
        .unwrap()
        .query_row(
            "SELECT r.source_id, r.generation, r.ordinal FROM sq_scope_sources s JOIN mc_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') AND s.lane='claude' AND NOT EXISTS(SELECT 1 FROM sq_decisions d WHERE d.scope_generation=?1 AND d.cache_source_id=r.source_id AND d.cache_generation=r.generation AND d.cache_ordinal=r.ordinal) LIMIT 1",
            [snapshot.generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap();
    let (loser_source, loser_generation, loser_ordinal) =
        loser.expect("late source keeps canonical loser rows beside the early winner");
    rusqlite::Connection::open(&cache_path)
        .unwrap()
        .execute(
            "DELETE FROM mc_rows WHERE source_id = ?1 AND generation = ?2 AND ordinal = ?3",
            rusqlite::params![loser_source, loser_generation, loser_ordinal],
        )
        .unwrap();
    let mut seen = 0usize;
    let result =
        crate::visit_local_usage_snapshot(&snapshot, UsageQuery::compact(), &|_| true, &mut |_| {
            seen += 1
        });
    assert!(result.is_err());
    assert_eq!(seen, 0);
}

fn historical_payload(
    snapshot: &UsageSnapshot,
    options: &crate::ReportOptions,
    pricing: Option<&crate::pricing::PricingService>,
    kind: &str,
    incremental: bool,
) -> serde_json::Value {
    if incremental {
        historical_payload_inner(snapshot, options, pricing, kind, true)
    } else {
        // The complete oracle starts with a fresh chrono::Local thread cache.
        // A just-changed TZ must not make the independent oracle stale too.
        std::thread::scope(|scope| {
            scope
                .spawn(|| historical_payload_inner(snapshot, options, pricing, kind, false))
                .join()
                .unwrap()
        })
    }
}

fn historical_payload_inner(
    snapshot: &UsageSnapshot,
    options: &crate::ReportOptions,
    pricing: Option<&crate::pricing::PricingService>,
    kind: &str,
    incremental: bool,
) -> serde_json::Value {
    let start = std::time::Instant::now();
    let mut value = match kind {
        "models" => {
            let report = if incremental {
                crate::get_model_report_from_snapshot(snapshot, options.clone(), pricing).unwrap()
            } else {
                crate::model_report_with_visitor(options, start, |filter, sink| {
                    crate::drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
                })
                .unwrap()
            };
            serde_json::to_value(report).unwrap()
        }
        "agents" => {
            let report = if incremental {
                crate::get_agents_report_from_snapshot(snapshot, options.clone(), pricing).unwrap()
            } else {
                crate::agents_report_with_visitor(options, start, |filter, sink| {
                    crate::drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
                })
                .unwrap()
            };
            serde_json::to_value(report).unwrap()
        }
        "hourly" => {
            let report = if incremental {
                crate::get_hourly_report_from_snapshot(snapshot, options.clone(), pricing).unwrap()
            } else {
                crate::hourly_report_with_visitor(options, start, |filter, sink| {
                    crate::drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
                })
                .unwrap()
            };
            serde_json::to_value(report).unwrap()
        }
        "graph-best-effort" => {
            let report = if incremental {
                crate::lean_local_graph_from_snapshot(
                    snapshot,
                    options.clone(),
                    pricing,
                    crate::GraphPricingMode::BestEffort,
                )
                .unwrap()
            } else {
                crate::lean_graph_report_with_visitor(
                    options,
                    start,
                    crate::GraphPricingMode::BestEffort,
                    |filter, sink| {
                        crate::drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
                    },
                )
                .unwrap()
            };
            serde_json::to_value(report.into_graph()).unwrap()
        }
        _ => panic!("unknown report fixture"),
    };
    value.as_object_mut().unwrap().remove("processing_time_ms");
    if let Some(meta) = value
        .get_mut("meta")
        .and_then(serde_json::Value::as_object_mut)
    {
        meta.remove("processing_time_ms");
        meta.remove("generated_at");
    }
    value
}

fn publish_historical_fixture(
    root: &std::path::Path,
    context: &ResolvedLocalSourceContext,
    count: usize,
    correction: Option<usize>,
) -> UsageSnapshot {
    let publisher = query(root);
    let mut snapshot = publisher
        .begin_scope(root, context, &["amp".into()], None, SelectionMode::Report)
        .unwrap();
    let source = context.home_dir().join("historical.jsonl");
    std::fs::write(&source, format!("{count}-{correction:?}")).unwrap();
    let cache = SourceMessageCache::open_from_root(Some(root));
    cache
        .write_source(
            CacheIdentity::for_client(ClientId::Amp),
            &source,
            SourceFingerprint::from_path_samples_only(&source).unwrap(),
            None,
            None,
            None,
            &mut |_| {},
            &mut |db, metadata| {
                publisher.register_cache_on(db, &source, metadata, "amp", 0, 0, false, false)
            },
            |writer| {
                for index in 0..count {
                    let timestamp = if index == 7 {
                        0
                    } else {
                        1_735_689_600_000 + index as i64 * 3_600_000
                            - if correction == Some(index) {
                                86_400_000
                            } else {
                                0
                            }
                    };
                    let mut row = message(
                        "amp",
                        "history",
                        timestamp,
                        Some(&format!("row-{index}")),
                        if correction == Some(index) {
                            1_000
                        } else {
                            (index % 5 + 1) as i64
                        },
                    );
                    row.model_id = if index % 2 == 0 {
                        "model"
                    } else {
                        "other-model"
                    }
                    .into();
                    row.agent = Some(if index % 3 == 0 { "Builder" } else { "Main" }.into());
                    row.is_turn_start = index % 7 == 0;
                    row.duration_ms = Some((index % 17 + 1) as i64);
                    // Reassociation would lose the small terms after the large
                    // cost. Every checkpoint must preserve the sequential bits.
                    if index % 11 != 0 {
                        row.cost = if index == 1 { 1e16 } else { 1.0 };
                        row.cost_source = crate::sessions::CostSource::ProviderReported;
                    }
                    writer.push(&row, false, false)?;
                }
                Ok(())
            },
        )
        .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    snapshot
}

#[test]
fn historical_report_checkpoints_reuse_changed_generations_with_exact_payloads() {
    if std::env::var_os("SYRTIS_REPORT_CHECKPOINT_CHILD").is_none() {
        let home = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store_query::tests::historical_report_checkpoints_reuse_changed_generations_with_exact_payloads", "--nocapture"])
            .env_clear()
            .env("SYRTIS_REPORT_CHECKPOINT_CHILD", "1")
            .env("HOME", home.path())
            .env("SYRTIS_CONFIG_DIR", home.path().join("config"))
            .env("XDG_CONFIG_HOME", home.path().join("xdg-config"))
            .env("XDG_DATA_HOME", home.path().join("xdg-data"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .env("TZ", "UTC")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let options = crate::ReportOptions {
        clients: Some(vec!["amp".into()]),
        ..Default::default()
    };
    let price = |rate| {
        crate::pricing::PricingService::new(
            std::collections::HashMap::from([(
                "model".into(),
                crate::pricing::ModelPricing {
                    input_cost_per_token: Some(rate),
                    ..Default::default()
                },
            )]),
            std::collections::HashMap::new(),
        )
    };
    let pricing = price(0.013);
    let kinds = ["graph-best-effort", "models", "hourly", "agents"];
    let first = publish_historical_fixture(root.path(), &context, 2305, None);
    for kind in kinds {
        assert_eq!(
            historical_payload(&first, &options, Some(&pricing), kind, true),
            historical_payload(&first, &options, Some(&pricing), kind, false),
        );
    }
    let second = publish_historical_fixture(root.path(), &context, 2330, None);
    assert_ne!(first.generation, second.generation);
    assert!(query(root.path()).pin(&first).is_err());
    for kind in kinds {
        // All connections from the producing fold were dropped. Reopening the
        // disk state is sufficient; no process registry holds these folds.
        let cached =
            crate::HistoricalReportFold::new(&second, &options, Some(&pricing), kind).unwrap();
        assert_eq!(cached.checkpoint.resume_rows, 2305, "{kind}");
        drop(cached);
        REPORT_DECODED_ROWS.with(|count| count.set(0));
        let incremental = historical_payload(&second, &options, Some(&pricing), kind, true);
        assert_eq!(
            REPORT_DECODED_ROWS.with(|count| count.get()),
            25,
            "{kind} decoded history"
        );
        if kind == "models" || kind == "agents" {
            let key = if kind == "models" {
                ("model", "other-model")
            } else {
                ("agent", "Main")
            };
            let cost = incremental["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry[key.0] == key.1)
                .unwrap()["cost"]
                .as_f64()
                .unwrap();
            assert_eq!(
                cost.to_bits(),
                1e16_f64.to_bits(),
                "sequential f64 bits: {kind}"
            );
            assert_ne!(
                cost.to_bits(),
                (1e16_f64 + 12.0).to_bits(),
                "a merged suffix sum would change the exact result"
            );
        }
        assert_eq!(
            incremental,
            historical_payload(&second, &options, Some(&pricing), kind, false)
        );
        REPORT_DECODED_ROWS.with(|count| count.set(0));
        assert_eq!(
            incremental,
            historical_payload(&second, &options, Some(&pricing), kind, true)
        );
        assert_eq!(
            REPORT_DECODED_ROWS.with(|count| count.get()),
            0,
            "{kind} warm decode"
        );
    }
    let corrected = publish_historical_fixture(root.path(), &context, 2330, Some(1200));
    for kind in kinds {
        let cached =
            crate::HistoricalReportFold::new(&corrected, &options, Some(&pricing), kind).unwrap();
        assert_eq!(
            cached.checkpoint.resume_rows, 1024,
            "old-date correction: {kind}"
        );
        drop(cached);
        assert_eq!(
            historical_payload(&corrected, &options, Some(&pricing), kind, true),
            historical_payload(&corrected, &options, Some(&pricing), kind, false),
        );
        for year in ["2025", "1900"] {
            let options = crate::ReportOptions {
                year: Some(year.into()),
                ..options.clone()
            };
            let cached =
                crate::HistoricalReportFold::new(&corrected, &options, Some(&pricing), kind)
                    .unwrap();
            assert!(!cached.checkpoint.has_restored(), "year identity: {kind}");
            drop(cached);
            assert_eq!(
                historical_payload(&corrected, &options, Some(&pricing), kind, true),
                historical_payload(&corrected, &options, Some(&pricing), kind, false),
            );
        }
        let changed_price = price(0.031);
        let cached =
            crate::HistoricalReportFold::new(&corrected, &options, Some(&changed_price), kind)
                .unwrap();
        assert!(!cached.checkpoint.has_restored(), "price identity: {kind}");
        drop(cached);
        assert_eq!(
            historical_payload(&corrected, &options, Some(&changed_price), kind, true),
            historical_payload(&corrected, &options, Some(&changed_price), kind, false),
        );
    }
    crate::model_alias::set_model_aliases(&crate::model_alias::ModelAliasMap {
        entries: std::collections::BTreeMap::from([("other-model".into(), "model".into())]),
    });
    for kind in ["models", "hourly"] {
        assert!(
            !crate::HistoricalReportFold::new(&corrected, &options, Some(&pricing), kind)
                .unwrap()
                .checkpoint
                .has_restored()
        );
        assert_eq!(
            historical_payload(&corrected, &options, Some(&pricing), kind, true),
            historical_payload(&corrected, &options, Some(&pricing), kind, false),
        );
    }
    crate::model_alias::clear_model_aliases();
    unsafe {
        std::env::set_var("TZ", "Asia/Seoul");
    }
    for kind in kinds {
        assert!(
            !crate::HistoricalReportFold::new(&corrected, &options, Some(&pricing), kind)
                .unwrap()
                .checkpoint
                .has_restored(),
            "timezone identity: {kind}"
        );
        assert_eq!(
            historical_payload(&corrected, &options, Some(&pricing), kind, true),
            historical_payload(&corrected, &options, Some(&pricing), kind, false),
        );
    }
    unsafe {
        std::env::set_var("TZ", "UTC");
    }
    let retained = publish_historical_fixture(root.path(), &context, 0, None);
    for kind in kinds {
        assert_eq!(
            historical_payload(&retained, &options, Some(&pricing), kind, true),
            historical_payload(&retained, &options, Some(&pricing), kind, false),
        );
    }
    let other_home = root.path().join("other-home");
    std::fs::create_dir_all(&other_home).unwrap();
    let other_context = ResolvedLocalSourceContext::capture(
        Some(other_home),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let other_root_scope = publish_historical_fixture(root.path(), &other_context, 20, None);
    assert_ne!(other_root_scope.scope, retained.scope);
    for kind in kinds {
        assert!(
            !crate::HistoricalReportFold::new(&other_root_scope, &options, Some(&pricing), kind)
                .unwrap()
                .checkpoint
                .has_restored(),
            "root scope identity: {kind}"
        );
        assert_eq!(
            historical_payload(&other_root_scope, &options, Some(&pricing), kind, true),
            historical_payload(&other_root_scope, &options, Some(&pricing), kind, false),
        );
    }
    let publisher = query(root.path());
    let mut other_client_scope = publisher
        .begin_scope(
            root.path(),
            &context,
            &["claude".into()],
            None,
            SelectionMode::Report,
        )
        .unwrap();
    publisher.publish(&mut other_client_scope).unwrap();
    let other_options = crate::ReportOptions {
        clients: Some(vec!["claude".into()]),
        ..options.clone()
    };
    for kind in kinds {
        assert!(
            !crate::HistoricalReportFold::new(
                &other_client_scope,
                &other_options,
                Some(&pricing),
                kind,
            )
            .unwrap()
            .checkpoint
            .has_restored(),
            "client scope identity: {kind}"
        );
        assert_eq!(
            historical_payload(
                &other_client_scope,
                &other_options,
                Some(&pricing),
                kind,
                true
            ),
            historical_payload(
                &other_client_scope,
                &other_options,
                Some(&pricing),
                kind,
                false
            ),
        );
    }
    assert!(crate::get_model_report_from_snapshot(
        &other_client_scope,
        options.clone(),
        Some(&pricing)
    )
    .is_err());
    let database = query(root.path());
    let (entries, bytes): (i64, i64) = database.db.query_row(
        "SELECT COUNT(*),COALESCE(SUM(length(state)+length(query_key)+length(membership)+length(state_digest)+32),0)
         FROM report_checkpoints",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert!(entries <= 128);
    assert!(bytes <= 16 * 1024 * 1024);
    database
        .db
        .execute(
            "UPDATE report_checkpoints SET state=x'ff' WHERE row_count=20",
            [],
        )
        .unwrap();
    for kind in kinds {
        assert!(
            crate::HistoricalReportFold::new(&other_root_scope, &options, Some(&pricing), kind,)
                .is_err(),
            "corrupt checkpoint state: {kind}"
        );
    }
    database
        .db
        .execute("DELETE FROM report_checkpoints WHERE row_count=20", [])
        .unwrap();
    for kind in kinds {
        historical_payload(&other_root_scope, &options, Some(&pricing), kind, true);
    }
    let (source, generation, ordinal): (i64, i64, i64) = database
        .db
        .query_row(
            "SELECT v.source_id,v.row_generation,r.ordinal
         FROM sq_scope_sources s JOIN mc_generation_storage v
           ON v.source_id=s.cache_source_id AND v.generation=s.cache_generation
         JOIN mc_rows r ON r.source_id=v.source_id AND r.generation=v.row_generation AND r.ordinal<v.row_count
         WHERE s.scope_generation=?1 AND s.lane='amp' ORDER BY r.ordinal LIMIT 1",
            [other_root_scope.generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    // Alter the actual bytes underneath a previously saved checkpoint while
    // leaving the producer's digest untouched. Metadata-only admission would
    // return a successful stale aggregate; every ordinary report must fail.
    database
        .db
        .execute(
            "UPDATE mc_rows SET message=x'ff' WHERE source_id=?1 AND generation=?2 AND ordinal=?3",
            params![source, generation, ordinal],
        )
        .unwrap();
    let revision: i64 = database
        .db
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    assert!(crate::get_model_report_from_snapshot(
        &other_root_scope,
        options.clone(),
        Some(&pricing)
    )
    .is_err());
    assert!(crate::get_agents_report_from_snapshot(
        &other_root_scope,
        options.clone(),
        Some(&pricing)
    )
    .is_err());
    assert!(crate::get_hourly_report_from_snapshot(
        &other_root_scope,
        options.clone(),
        Some(&pricing)
    )
    .is_err());
    assert!(crate::lean_local_graph_from_snapshot(
        &other_root_scope,
        options,
        Some(&pricing),
        crate::GraphPricingMode::BestEffort,
    )
    .is_err());
    assert_eq!(
        revision,
        database
            .db
            .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        "failed checkpoint admission must not publish cache replacements"
    );
}

#[test]
fn checkpoint_membership_owns_opencode_aliases_after_scope_gc() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let clients = vec!["opencode".to_string()];
    let mut snapshot = publisher
        .begin_scope(root.path(), &context, &clients, None, SelectionMode::Report)
        .unwrap();
    let source = home.join("opencode.db");
    // Equal payloads within one canonical source are collapsed at ingestion.
    // Separate sources preserve the two disconnected alias cohorts until
    // report arbitration, which is the boundary this regression exercises.
    local_fixture(&source, &[("row-a", 1)]);
    let other_directory = home.join("other");
    std::fs::create_dir_all(&other_directory).unwrap();
    let other_source = other_directory.join("opencode.db");
    local_fixture(&other_source, &[("row-b", 1)]);
    crate::usage_store::synchronize_opencode_sources(
        &[source, other_source],
        &[],
        root.path(),
        &mut |db, path, sqlite, index, key| {
            publisher.register_opencode(db, path, sqlite, index, key)
        },
    )
    .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    let prepare = || {
        let reader = query(root.path());
        reader.pin(&snapshot).unwrap();
        reader
            .prepare_query(&snapshot, UsageQuery::compact())
            .unwrap();
        reader.unpin().unwrap();
        reader
    };
    let first = prepare();
    let second = prepare();
    let (first_id, second_id, payloads): (i64, i64, i64) = second
        .db
        .query_row(
            "SELECT MIN(id),MAX(id),COUNT(DISTINCT payload) FROM ocq_rows",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_ne!(first_id, second_id);
    assert_eq!(
        payloads, 1,
        "alias collision must affect exact-payload arbitration"
    );
    let messages_before: Vec<Vec<u8>> = second
        .db
        .prepare("SELECT message FROM ocq_rows ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    second
        .db
        .execute("DELETE FROM ocq_aliases WHERE id=?1", [second_id])
        .unwrap();
    second
        .db
        .execute(
            "INSERT INTO ocq_aliases SELECT ?1,key,timestamp,position FROM ocq_aliases WHERE id=?2",
            params![second_id, first_id],
        )
        .unwrap();
    let messages_after: Vec<Vec<u8>> = second
        .db
        .prepare("SELECT message FROM ocq_rows ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        messages_before, messages_after,
        "only owned arbitration aliases changed"
    );
    // Both readers have unpinned. Publish the same producer without sources,
    // retiring all old canonical descriptors before membership is examined.
    let mut replacement = publisher
        .begin_scope(root.path(), &context, &clients, None, SelectionMode::Report)
        .unwrap();
    publisher.publish(&mut replacement).unwrap();
    assert_eq!(
        publisher
            .db
            .query_row(
                "SELECT COUNT(*) FROM sq_scope_sources WHERE scope_generation=?1",
                [snapshot.generation],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    first.prepare_report_fold(&snapshot).unwrap();
    second.prepare_report_fold(&snapshot).unwrap();
    let fold_count = |reader: &QueryState| {
        let mut count = 0_u64;
        crate::fold_prepared_usage_snapshot(
            &snapshot,
            reader,
            None,
            UsageQuery::compact(),
            &|_| true,
            &mut |_| count += 1,
            false,
        )
        .unwrap();
        count
    };
    let first_count = fold_count(&first);
    let second_count = fold_count(&second);
    assert_eq!(first_count, 2);
    assert_eq!(second_count, 1);
    let saved = crate::usage_store::ReportCheckpoint::open(first.connection(), [66; 32]).unwrap();
    saved.save(first.connection(), 0, &first_count).unwrap();
    saved.finish(first.connection()).unwrap();
    let changed =
        crate::usage_store::ReportCheckpoint::open(second.connection(), [66; 32]).unwrap();
    assert!(
        !changed.has_restored(),
        "post-unpin GC cannot make differing alias cohorts share a checkpoint"
    );
}

#[test]
fn checkpoint_timezone_rules_drive_immediate_same_thread_calendar_changes() {
    if std::env::var_os("SYRTIS_CHECKPOINT_TIMEZONE_CHILD").is_none() {
        let root = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store_query::tests::checkpoint_timezone_rules_drive_immediate_same_thread_calendar_changes", "--nocapture"])
            .env_clear()
            .env("SYRTIS_CHECKPOINT_TIMEZONE_CHILD", "1")
            .env("HOME", root.path())
            .env("SYRTIS_CONFIG_DIR", root.path().join("config"))
            .env("XDG_CONFIG_HOME", root.path().join("xdg-config"))
            .env("XDG_DATA_HOME", root.path().join("xdg-data"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .env("TZ", "UTC")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use chrono::TimeZone;
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.clone()),
        false,
        crate::scanner::ScannerSettings::default(),
    )
    .unwrap();
    let publisher = query(root.path());
    let mut snapshot = publisher
        .begin_scope(
            root.path(),
            &context,
            &["amp".into()],
            None,
            SelectionMode::Report,
        )
        .unwrap();
    let source = home.join("timezone.jsonl");
    std::fs::write(&source, "timezone fixture").unwrap();
    let timestamp = 1_735_689_600_000 - 30 * 60 * 1000; // 2024-12-31 23:30 UTC.
    SourceMessageCache::open_from_root(Some(root.path()))
        .write_source(
            CacheIdentity::for_client(ClientId::Amp),
            &source,
            SourceFingerprint::from_path_samples_only(&source).unwrap(),
            None,
            None,
            None,
            &mut |_| {},
            &mut |db, metadata| {
                publisher.register_cache_on(db, &source, metadata, "amp", 0, 0, false, false)
            },
            |writer| {
                writer.push(
                    &message("amp", "timezone", timestamp, Some("one"), 10),
                    false,
                    false,
                )
            },
        )
        .unwrap();
    publisher.publish(&mut snapshot).unwrap();
    let options = crate::ReportOptions {
        clients: Some(vec!["amp".into()]),
        year: Some("2025".into()),
        since: Some("2025-01-01".into()),
        until: Some("2025-01-01".into()),
        ..Default::default()
    };
    let kinds = ["graph-best-effort", "models", "hourly", "agents"];
    for kind in kinds {
        historical_payload(&snapshot, &options, None, kind, true);
    }
    assert_eq!(
        chrono::Local
            .timestamp_millis_opt(timestamp)
            .single()
            .unwrap()
            .offset()
            .local_minus_utc(),
        0
    );
    unsafe {
        std::env::set_var("TZ", "Asia/Seoul");
    }
    // Prime and switch on the same thread, then query immediately: chrono's
    // old rules are demonstrably still cached, with no sleep-based workaround.
    assert_eq!(
        chrono::Local
            .timestamp_millis_opt(timestamp)
            .single()
            .unwrap()
            .offset()
            .local_minus_utc(),
        0,
        "fixture must expose the stale chrono thread cache"
    );
    for kind in kinds {
        let incremental = historical_payload(&snapshot, &options, None, kind, true);
        match kind {
            "graph-best-effort" => {
                assert_eq!(incremental["summary"]["total_tokens"], 10);
                assert_eq!(incremental["contributions"][0]["date"], "2025-01-01");
            }
            "models" => assert_eq!(incremental["total_input"], 10),
            "hourly" => assert_eq!(incremental["entries"][0]["hour"], "2025-01-01 08:00"),
            "agents" => assert_eq!(incremental["total_messages"], 1),
            _ => unreachable!(),
        }
        assert_eq!(
            incremental,
            historical_payload(&snapshot, &options, None, kind, false)
        );
        assert_eq!(
            incremental,
            historical_payload(&snapshot, &options, None, kind, true)
        );
    }
    let captured =
        crate::HistoricalReportFold::new(&snapshot, &options, None, "frozen-timezone-regression")
            .unwrap();
    assert!(captured.timezone.is_some());
    unsafe {
        std::env::set_var("TZ", "UTC");
    }
    let mut dated = std::collections::BTreeMap::<String, i64>::new();
    captured
        .fold(
            &mut dated,
            &|row| row.date == "2025-01-01",
            &mut |dated, row| {
                *dated.entry(row.date.clone()).or_default() += row.tokens.input;
            },
        )
        .unwrap();
    assert_eq!(
        dated,
        std::collections::BTreeMap::from([("2025-01-01".into(), 10)]),
        "key and conversions must keep the captured rules even after another TZ change"
    );
    unsafe {
        std::env::set_var("TZ", "Asia/Seoul");
    }
    let reused =
        crate::HistoricalReportFold::new(&snapshot, &options, None, "frozen-timezone-regression")
            .unwrap();
    assert!(reused.checkpoint.has_restored());
    for _ in 0..2 {
        let mut repeated = std::collections::BTreeMap::<String, i64>::new();
        reused
            .fold(
                &mut repeated,
                &|row| row.date == "2025-01-01",
                &mut |dated, row| {
                    *dated.entry(row.date.clone()).or_default() += row.tokens.input;
                },
            )
            .unwrap();
        assert_eq!(
            repeated, dated,
            "a consumed restore blob cannot leave an offset that skips facts"
        );
        assert!(
            !reused.checkpoint.has_restored(),
            "encoded restore bytes must be released after decoding"
        );
    }

    unsafe {
        std::env::set_var("TZ", "America/New_York");
    }
    let zone = crate::CapturedReportTimezone::capture().unwrap();
    for (utc, expected) in [
        ("2025-03-09T06:59:59Z", "2025-03-09 01:00"),
        ("2025-03-09T07:00:00Z", "2025-03-09 03:00"),
        ("2025-11-02T05:30:00Z", "2025-11-02 01:00"),
        ("2025-11-02T06:30:00Z", "2025-11-02 01:00"),
    ] {
        let timestamp = chrono::DateTime::parse_from_rfc3339(utc)
            .unwrap()
            .timestamp_millis();
        let mut row = message("amp", "dst", timestamp, None, 1);
        row.date = zone.date(timestamp);
        assert_eq!(zone.hour(&row), expected);
    }
    let mut undated = message("amp", "undated", 0, None, 1);
    undated.date = zone.date(0);
    assert_eq!(zone.hour(&undated), "1969-12-31 00:00");

    unsafe {
        std::env::set_var("TZ", "QST0");
    } // Valid custom POSIX rule without a named TZif file.
    let unfiltered = crate::ReportOptions {
        clients: Some(vec!["amp".into()]),
        ..Default::default()
    };
    let custom = crate::HistoricalReportFold::new(&snapshot, &unfiltered, None, "models").unwrap();
    assert!(custom.timezone.is_none());
    assert!(!custom.checkpoint.has_restored());
    let entries_before: i64 = publisher
        .db
        .query_row("SELECT COUNT(*) FROM report_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        crate::get_model_report_from_snapshot(&snapshot, unfiltered, None)
            .unwrap()
            .total_input,
        10
    );
    assert_eq!(
        publisher
            .db
            .query_row("SELECT COUNT(*) FROM report_checkpoints", [], |row| row
                .get::<_, i64>(0),)
            .unwrap(),
        entries_before,
        "unresolved/custom rules must not retain uncertain checkpoints"
    );
}
