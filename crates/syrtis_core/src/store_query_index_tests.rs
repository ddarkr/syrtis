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
