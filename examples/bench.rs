//! Times the common operations on an encrypted file with 10,000 documents.
//!
//! Run with `cargo run --release --example bench`.

use std::error::Error;
use std::time::Instant;

use agentdb::{AgentDb, Field, FieldType, Op, Query, TableDef, Write};
use serde_json::{Value, json};

const DOCS: u32 = 10_000;
const BATCH: u32 = 500;
const RUNS: u32 = 200;
const STATUSES: [&str; 3] = ["lead", "active", "churned"];

fn timed<T>(
    label: &str,
    runs: u32,
    mut work: impl FnMut(u32) -> agentdb::Result<T>,
) -> agentdb::Result<()> {
    let started = Instant::now();
    for run in 0..runs {
        work(run)?;
    }
    let total = started.elapsed();
    let each = total.as_secs_f64() * 1e3 / f64::from(runs);
    println!(
        "{label:<48} {each:>9.3} ms each  ({runs} runs, {:.2} s)",
        total.as_secs_f64()
    );
    Ok(())
}

fn client(n: u32) -> Value {
    let status = STATUSES
        .get(n as usize % STATUSES.len())
        .copied()
        .unwrap_or("lead");
    json!({"name": format!("Client {n}"), "status": status, "revenue": n})
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("bench.db");

    let started = Instant::now();
    let db = AgentDb::open(&path, "bench key")?;
    println!(
        "{:<48} {:>9.3} ms",
        "open a new encrypted file",
        started.elapsed().as_secs_f64() * 1e3
    );
    let status = FieldType::Enum {
        values: STATUSES.map(str::to_owned).to_vec(),
    };
    db.define_table(
        &TableDef::new("clients")
            .required("name", FieldType::Text)
            .with(Field::new("status", status, false).indexed())
            .with(Field::new("revenue", FieldType::Number, false).indexed()),
    )?;

    let started = Instant::now();
    for first in (0..DOCS).step_by(BATCH as usize) {
        let writes = (first..first + BATCH).map(|n| Write::Insert {
            table: "clients".to_owned(),
            doc: client(n),
        });
        db.batch(writes.collect())?;
    }
    let total = started.elapsed().as_secs_f64();
    println!(
        "{:<48} {:>9.3} ms each  ({DOCS} documents in batches of {BATCH}, {total:.2} s)",
        "insert in bulk",
        total * 1e3 / f64::from(DOCS)
    );
    timed("insert one document", RUNS, |n| {
        db.insert("clients", client(DOCS + n))
    })?;
    timed("get by id", RUNS, |n| db.get("clients", i64::from(n) + 1))?;
    timed("update one document", RUNS, |n| {
        db.update("clients", i64::from(n) + 1, json!({"revenue": 1}), None)
    })?;
    timed("find: status = lead (3,400 match, first 50)", RUNS, |_| {
        db.find(&Query::table("clients").filter("status", Op::Eq, "lead"))
    })?;
    timed("find: revenue > 9,900 (299 match)", RUNS, |_| {
        db.find(&Query::table("clients").filter("revenue", Op::Gt, 9900))
    })?;
    timed("find: status = lead, name contains \"99\"", RUNS, |_| {
        db.find(
            &Query::table("clients")
                .filter("status", Op::Eq, "lead")
                .filter("name", Op::Contains, "99"),
        )
    })?;
    timed("find: top 10 by revenue", RUNS, |_| {
        db.find(&Query::table("clients").sort("revenue", true).limit(10))
    })?;
    timed("rename a field across all documents", 1, |_| {
        db.rename_field("clients", "revenue", "yearly_revenue")
    })?;
    drop(db);

    let started = Instant::now();
    AgentDb::open(&path, "bench key")?;
    println!(
        "{:<48} {:>9.3} ms",
        "reopen the file",
        started.elapsed().as_secs_f64() * 1e3
    );
    println!(
        "file size: {} KB for {} documents",
        std::fs::metadata(&path)?.len() / 1024,
        DOCS + RUNS
    );
    Ok(())
}
