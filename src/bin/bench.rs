use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::{Direction, GraphHandle};
use mcpmem::types::{EntityInput as Entity, RelationInput};

/// Standalone knowledge-graph benchmark.
///
/// Seeds a fresh SQLite database with a chain graph (each entity links to the
/// next) and times reads, searches, traversals, and mutations against it.
/// Every measured call asserts its expected result, so a regression that makes
/// an operation fail panics the benchmark instead of printing a fast lie.
#[derive(Parser)]
#[command(
    name = "bench",
    version,
    about = "Knowledge-graph benchmark for mcpmem (in-process GraphHandle, SQLite-backed)"
)]
struct Args {
    /// Number of entities to seed; relations = entities - 1 (chain graph).
    #[arg(long, default_value_t = 1000)]
    entities: usize,

    /// Observations per entity; must be at least 1.
    #[arg(long = "obs-per-entity", default_value_t = 5)]
    obs_per_entity: usize,

    /// SQLite database path. The file (plus -wal/-shm) is removed before and
    /// after the run.
    #[arg(long, default_value = "/tmp/mcp_memory_bench.db")]
    db: PathBuf,
}

fn main() {
    let args = Args::parse();
    if args.entities < 2 {
        eprintln!("error: --entities must be at least 2 (the graph is a chain)");
        std::process::exit(2);
    }
    if args.obs_per_entity == 0 {
        eprintln!(
            "error: --obs-per-entity must be at least 1 (the benchmark has an observation search)"
        );
        std::process::exit(2);
    }
    bench(&args);
}

fn bench(args: &Args) {
    let path = args.db.as_path();
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }

    let kg = GraphHandle::new(
        path,
        Durability::Async,
        SqliteTuning::default(),
        NonZeroUsize::new(10000).unwrap(),
        4,
    )
    .expect("create KG");

    let n = args.entities;
    let obs = args.obs_per_entity;
    let last = n - 1;
    let first = "entity_0";
    let last_name = format!("entity_{last}");
    let mid_name = format!("entity_{}", n / 2);
    let third_name = format!("entity_{}", 2.min(last)); // find_all_paths target (distance 2 on the chain)
    let search_name = format!("entity_{}", 5.min(last));
    let obs_q = format!("observation_{}_{}", 3.min(last), (obs - 1).min(2));

    // ── Seed ──────────────────────────────────────────────────────────
    let entities: Vec<Entity> = (0..n)
        .map(|i| Entity {
            name: format!("entity_{i}"),
            entity_type: if i % 2 == 0 {
                "person".into()
            } else {
                "place".into()
            },
            observations: (0..obs)
                .map(|j| format!("observation_{i}_{j}"))
                .map(Into::into)
                .collect(),
            attributes: None,
        })
        .collect();

    let relations: Vec<RelationInput> = (0..last)
        .map(|i| RelationInput {
            from: format!("entity_{i}"),
            to: format!("entity_{}", i + 1),
            relation_type: "edge".into(),
            observations: vec![],
            attributes: None,
        })
        .collect();

    // ── Measure ───────────────────────────────────────────────────────
    // The body must evaluate to `bool`: true = the operation returned an
    // expected result. A regression that returns an error or an empty result
    // panics here, naming the row.
    macro_rules! measure {
        ($name:expr, $n:expr, $body:expr) => {{
            let mut total = Duration::ZERO;
            for _ in 0..$n {
                let start = Instant::now();
                assert!($body, "{}: unexpected result", $name);
                total += start.elapsed();
            }
            let avg = total / $n as u32;
            println!(
                "  {:30} {:>8} runs  avg {:>10?}  total {:>10?}",
                $name, $n, avg, total
            );
        }};
    }

    println!(
        "Benchmark: {} entities, {} obs/entity, {} relations",
        n, obs, last
    );
    println!();

    // Warmup
    assert!(kg.get_entity_count().is_ok());
    assert!(kg.get_relation_count().is_ok());

    measure!("create_entities", 1, {
        kg.create_entities(&entities).is_ok()
    });
    assert!(
        kg.get_entity(first).expect("read seed back").is_some(),
        "seed entity did not come back"
    );

    measure!("get_entity (warm row)", n, {
        kg.get_entity(first).expect("get").is_some()
    });

    // Flush entity seq
    let _ = kg.get_entity_count();

    measure!("create_relations", 1, {
        kg.create_relations(&relations).is_ok()
    });
    assert_eq!(
        kg.get_relation_count().expect("relation count"),
        last,
        "relation count after create"
    );

    measure!("get_entity_count", 1000, {
        kg.get_entity_count().expect("count") == n
    });
    measure!("get_relation_count", 1000, {
        kg.get_relation_count().expect("count") == last
    });

    measure!("degree (outgoing)", 1000, {
        kg.degree(first, Direction::Outgoing).expect("degree") == 1
    });
    measure!("degree (both)", 1000, {
        kg.degree(&mid_name, Direction::Both).expect("degree") >= 1
    });

    // A name that does not exist: the row-absent lookup path, asserted to
    // stay empty. (There is no process-level entity cache; all reads are
    // SQLite page-cache reads.)
    measure!("get_entity (missing)", n, {
        kg.get_entity("entity_does_not_exist")
            .expect("get")
            .is_none()
    });

    measure!("search_nodes (name match)", 200, {
        !kg.search_nodes_filtered(&search_name, None, 0, 10)
            .is_empty()
    });

    measure!("search_nodes (obs match)", 200, {
        !kg.search_nodes_filtered(&obs_q, None, 0, 10).is_empty()
    });

    measure!("search_nodes (filtered)", 200, {
        !kg.search_nodes_filtered("entity", Some("person"), 0, 10)
            .is_empty()
    });

    measure!("read_graph (all)", 5, {
        !kg.read_graph_filtered(None, 0, usize::MAX)
            .expect("read_graph")
            .is_empty()
    });

    measure!("read_graph (filtered)", 5, {
        !kg.read_graph_filtered(Some("person"), 0, usize::MAX)
            .expect("read_graph")
            .is_empty()
    });

    measure!("open_nodes (single)", 100, {
        !kg.open_nodes(&[first.into()]).is_empty()
    });

    measure!("open_nodes (5 names)", 100, {
        let names: Vec<String> = [0usize, 10, 20, 30, 40]
            .iter()
            .copied()
            .map(|i| format!("entity_{}", i.min(last)))
            .collect();
        !kg.open_nodes(&names).is_empty()
    });

    measure!("find_path (first → last)", 200, {
        matches!(
            kg.find_path(first, &last_name).expect("find_path"),
            Some(p) if !p.is_empty()
        )
    });

    measure!("entities_exist (10 names)", 200, {
        let names: Vec<String> = [
            first.into(),
            "missing".into(),
            mid_name.clone(),
            last_name.clone(),
            "also_missing".into(),
            format!("entity_{}", 25.min(last)),
            format!("entity_{}", 75.min(last)),
            "nope".into(),
            format!("entity_{}", 1.min(last)),
            format!("entity_{}", 100.min(last)),
        ]
        .into_iter()
        .collect();
        let v = kg.entities_exist(&names).expect("entities_exist");
        v.len() == 10 && v.iter().any(|b| *b) && v.iter().any(|b| !*b)
    });

    measure!("describe_entity", 200, {
        kg.describe_entity(&mid_name).is_ok()
    });

    measure!("entity_type_counts", 1000, {
        kg.entity_type_counts().len() == 2
    });
    measure!("relation_type_counts", 1000, {
        kg.relation_type_counts().len() == 1
    });

    measure!("batch_get_entities (10)", 100, {
        let names: Vec<String> = [
            first.into(),
            format!("entity_{}", 1.min(last)),
            format!("entity_{}", 2.min(last)),
            "missing".into(),
            format!("entity_{}", 3.min(last)),
            format!("entity_{}", 4.min(last)),
            format!("entity_{}", 5.min(last)),
            "nonexistent".into(),
            format!("entity_{}", 6.min(last)),
            format!("entity_{}", 7.min(last)),
        ]
        .into_iter()
        .collect();
        let v = kg.batch_get_entities(&names);
        v.len() == 10 && v.iter().any(|e| e.is_some())
    });

    measure!("neighbors (depth 1)", 200, {
        matches!(
            kg.neighbors(&mid_name, Direction::Both, None, 1),
            Ok(s) if !s.is_empty()
        )
    });

    measure!("neighbors (depth 2)", 100, {
        matches!(
            kg.neighbors(&mid_name, Direction::Both, None, 2),
            Ok(s) if !s.is_empty()
        )
    });

    measure!("export (json)", 10, {
        kg.export("json", i64::MAX)
            .expect("export")
            .starts_with("{\"entities\":")
    });

    measure!("find_all_paths (first → third, depth 5)", 200, {
        !kg.find_all_paths(first, &third_name, 5, 10)
            .expect("find_all_paths")
            .is_empty()
    });

    // ── Mutating ops: one run each — every mutation changes the state the
    // next run would measure, so a loop would time a no-op.
    measure!("add_observations (2 obs)", 1, {
        kg.add_observations(first, &["new_obs_a".into(), "new_obs_b".into()])
            .expect("add")
            .len()
            == 2
    });

    let _ = kg.add_observations("entity_1", &["to_delete".into()]);
    measure!("delete_observations (1 obs)", 1, {
        kg.delete_observations("entity_1", &["to_delete".into()])
            .is_ok()
    });

    measure!("upsert_entities (type change + obs)", 1, {
        kg.upsert_entities(&[Entity {
            name: first.into(),
            entity_type: "person".into(),
            observations: vec!["existing".into(), "upserted_obs".into()],
            attributes: None,
        }])
        .expect("upsert")
        .len()
            == 1
    });

    measure!("search_relations (from)", 200, {
        !kg.search_relations(Some(first), None, None, None, None)
            .map(|r| r.is_empty())
            .unwrap_or(true)
    });

    measure!("search_relations (from+type)", 200, {
        !kg.search_relations(Some(first), None, Some("edge"), None, None)
            .map(|r| r.is_empty())
            .unwrap_or(true)
    });

    // Cleanup
    let _ = std::fs::remove_file(path);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}
