//! Reproducible local query-work microbenchmark.
//!
//! Run an optimized build for meaningful timings:
//! `cargo run --release --example query_benchmark -- 500 5000 20000`.

use foremerge::model::{Scope, WorkQuery};
use foremerge::{Foremerge, Store};
use rusqlite::{Connection, params};
use serde_json::json;
use std::hint::black_box;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn main() -> anyhow::Result<()> {
    let sizes = std::env::args()
        .skip(1)
        .map(|value| value.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?;
    let sizes = if sizes.is_empty() {
        vec![500, 5_000, 20_000]
    } else {
        sizes
    };
    for size in sizes {
        println!("{}", serde_json::to_string(&run(size)?)?);
    }
    Ok(())
}

fn run(size: usize) -> anyhow::Result<serde_json::Value> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let database = std::env::temp_dir().join(format!(
        "foremerge-query-benchmark-{}-{nonce}.sqlite3",
        std::process::id()
    ));
    let initialized = Store::open(&database)?;
    drop(initialized);
    let started = Instant::now();
    seed(&database, size)?;
    let seed_ms = started.elapsed().as_millis();

    let service = Foremerge::new(Store::open(&database)?);
    let unfiltered = median_micros(|| {
        service.query_work(WorkQuery {
            limit: 50,
            ..WorkQuery::default()
        })
    })?;
    let filtered = median_micros(|| {
        service.query_work(WorkQuery {
            agent_id: Some("agt_benchmark".into()),
            status: Some("IN_PROGRESS".into()),
            limit: 50,
            ..WorkQuery::default()
        })
    })?;
    let scoped_hit = median_micros(|| {
        service.query_work(WorkQuery {
            scope: Some(Scope::new("symbol", "BenchmarkTarget")),
            limit: 50,
            ..WorkQuery::default()
        })
    })?;
    let scoped_miss = median_micros(|| {
        service.query_work(WorkQuery {
            scope: Some(Scope::new("symbol", "MissingTarget")),
            limit: 50,
            ..WorkQuery::default()
        })
    })?;
    drop(service);
    remove_database(&database);

    Ok(json!({
        "schema_version": 1,
        "intents": size,
        "seed_ms": seed_ms,
        "iterations": 9,
        "median_microseconds": {
            "unfiltered_limit_50": unfiltered,
            "agent_and_status_limit_50": filtered,
            "semantic_scope_hit_limit_50": scoped_hit,
            "semantic_scope_miss_limit_50": scoped_miss,
        }
    }))
}

fn seed(database: &Path, size: usize) -> anyhow::Result<()> {
    let mut conn = Connection::open(database)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO agents(id, name, model, capabilities_json, worktree, git_branch,
         git_head, status, registered_at, last_seen_at)
         VALUES('agt_benchmark', 'benchmark', 'scripted', '[]', NULL, NULL, NULL,
         'ACTIVE', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        [],
    )?;
    {
        let mut tasks = tx.prepare_cached(
            "INSERT INTO tasks(id, task_key, title, created_by_agent_id, created_at)
             VALUES(?1, ?2, ?3, 'agt_benchmark', ?4)",
        )?;
        let mut intents = tx.prepare_cached(
            "INSERT INTO intents(id, agent_id, task_id, summary, rationale, scopes_json,
             depends_on_json, metadata_json, status, created_at, updated_at)
             VALUES(?1, 'agt_benchmark', ?2, ?3, NULL, ?4, '[]', '{}', ?5, ?6, ?6)",
        )?;
        let mut scopes = tx.prepare_cached(
            "INSERT INTO intent_scopes(intent_id, scope_kind, scope_key, canonical_scope)
             VALUES(?1, 'symbol', ?2, ?3)",
        )?;
        for index in 0..size {
            let task_id = format!("tsk_{index:08}");
            let intent_id = format!("int_{index:08}");
            let created_at = format!(
                "2026-01-01T00:{:02}:{:02}.{:06}Z",
                index / 60 % 60,
                index % 60,
                index
            );
            let scope_key = if index == 0 {
                "BenchmarkTarget".to_string()
            } else {
                format!("Unrelated{index}")
            };
            let scopes_json = serde_json::to_string(&[Scope::new("symbol", &scope_key)])?;
            tasks.execute(params![
                task_id,
                format!("task-key-{index}"),
                format!("Task {index}"),
                created_at,
            ])?;
            intents.execute(params![
                intent_id,
                task_id,
                format!("Intent {index}"),
                scopes_json,
                if index % 2 == 0 {
                    "IN_PROGRESS"
                } else {
                    "INTENT"
                },
                created_at,
            ])?;
            scopes.execute(params![
                intent_id,
                scope_key,
                format!("symbol:{}", scope_key.to_lowercase()),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn median_micros<T>(mut operation: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<u128> {
    let mut samples = Vec::with_capacity(9);
    for _ in 0..9 {
        let started = Instant::now();
        black_box(operation()?);
        samples.push(started.elapsed().as_micros());
    }
    samples.sort_unstable();
    Ok(samples[samples.len() / 2])
}

fn remove_database(database: &Path) {
    for path in [
        database.to_path_buf(),
        std::path::PathBuf::from(format!("{}-wal", database.display())),
        std::path::PathBuf::from(format!("{}-shm", database.display())),
    ] {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("warning: could not remove {}: {error}", path.display());
            }
        }
    }
}
