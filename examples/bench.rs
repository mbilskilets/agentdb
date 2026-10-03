//! Times the common operations on an encrypted file with 10,000 documents.
//!
//! Run with `cargo run --release --example bench`.

use std::error::Error;
use std::time::Instant;

use agentdb::{AgentDb, FieldType, Op, Query, TableDef};
use serde_json::json;

const DOCS: u32 = 10_000;
const READS: u32 = 200;

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
        "{label:<44} {each:>9.3} ms each  ({runs} runs, {:.2} s)",
        total.as_secs_f64()
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("bench.db");
    let statuses = ["lead", "active", "churned"];

    let started = Instant::now();
    let db = AgentDb::open(&path, "bench key")?;
    println!(
        "{:<44} {:>9.3} ms",
        "open a new encrypted file",
        started.elapsed().as_secs_f64() * 1e3
    );
    db.define_table(
        &TableDef::new("clients")
            .required("name", FieldType::Text)
            .optional(
                "status",
                FieldType::Enum {
                    values: statuses.map(str::to_owned).to_vec(),
                },
            )
            .optional("revenue", FieldType::Number),
    )?;

    timed("insert one document", DOCS, |n| {
        let status = statuses
            .get(n as usize % statuses.len())
            .copied()
            .unwrap_or("lead");
        db.insert(
            "clients",
            json!({"name": format!("Client {n}"), "status": status, "revenue": n}),
        )
    })?;
    timed("get by id", READS, |n| db.get("clients", i64::from(n) + 1))?;
    timed("update one document", READS, |n| {
        db.update("clients", i64::from(n) + 1, json!({"revenue": 1}), None)
    })?;
    timed("find: status = lead (3,334 match, first 50)", READS, |_| {
        db.find(&Query::table("clients").filter("status", Op::Eq, "lead"))
    })?;
    timed("find: revenue > 9,900 (99 match)", READS, |_| {
        db.find(&Query::table("clients").filter("revenue", Op::Gt, 9900))
    })?;
    timed("find: name contains \"99\"", READS, |_| {
        db.find(&Query::table("clients").filter("name", Op::Contains, "99"))
    })?;
    timed("find: top 10 by revenue", READS, |_| {
        db.find(&Query::table("clients").sort("revenue", true).limit(10))
    })?;
    timed("rename a field across all documents", 1, |_| {
        db.rename_field("clients", "revenue", "yearly_revenue")
    })?;
    drop(db);

    let started = Instant::now();
    AgentDb::open(&path, "bench key")?;
    println!(
        "{:<44} {:>9.3} ms",
        "reopen the file",
        started.elapsed().as_secs_f64() * 1e3
    );
    println!(
        "file size: {} KB for {DOCS} documents",
        std::fs::metadata(&path)?.len() / 1024
    );
    Ok(())
}
