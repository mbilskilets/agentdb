#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use agentdb::{
        AgentDb, Change, ChangeKind, DbError, FieldType, Op, Query, SchemaChange, TableDef, Write,
    };
    use serde_json::json;
    use tokio::sync::broadcast::Receiver;

    const AGENTS: i64 = 4;
    const PER_AGENT: i64 = 50;

    fn setup(db: &AgentDb) {
        let changes: Vec<SchemaChange> = serde_json::from_value(json!([
            {"op": "define_table", "table": {"name": "employees", "fields": [
                {"name": "name", "type": "text", "required": true}
            ]}},
            {"op": "define_table", "table": {"name": "clients", "fields": [
                {"name": "name", "type": "text", "required": true},
                {"name": "status", "type": "enum", "values": ["lead", "active"], "required": true},
                {"name": "revenue", "type": "number", "required": false},
                {"name": "owner", "type": "ref", "table": "employees", "required": false}
            ]}}
        ]))
        .unwrap();
        db.migrate(&changes).unwrap();
    }

    fn insert_clients(db: &AgentDb, agent: i64, owner: i64) {
        for n in 0..PER_AGENT {
            let status = if n % 2 == 0 { "lead" } else { "active" };
            let name = format!("client {agent}-{n}");
            db.insert(
                "clients",
                json!({"name": name, "status": status, "revenue": n * 100, "owner": owner}),
            )
            .unwrap();
        }
    }

    /// Runs `work` once per agent, all at the same time.
    fn concurrently<T: Send + 'static>(
        db: &Arc<AgentDb>,
        work: impl Fn(&AgentDb, i64) -> T + Send + Copy + 'static,
    ) -> Vec<T> {
        let threads: Vec<_> = (0..AGENTS)
            .map(|agent| {
                let db = Arc::clone(db);
                thread::spawn(move || work(&db, agent))
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect()
    }

    fn check_inserts(db: &AgentDb) {
        let total = AGENTS * PER_AGENT;
        let all = db.find(&Query::table("clients").limit(500)).unwrap();
        assert_eq!(all.total, total);
        let ids: Vec<i64> = all.docs.iter().map(|doc| doc.id).collect();
        assert_eq!(ids, (1..=total).collect::<Vec<_>>());
        let leads = db
            .find(&Query::table("clients").filter("status", Op::Eq, "lead"))
            .unwrap();
        assert_eq!(leads.total, total / 2);
        assert_eq!(leads.docs.len(), 50);
        assert_eq!(leads.next_offset, Some(50));
    }

    fn check_events(events: &[Change]) {
        let count = |kind| events.iter().filter(|event| event.kind == kind).count();
        assert_eq!(count(ChangeKind::Insert), 201);
        assert_eq!(count(ChangeKind::Update), 1);
        assert_eq!(count(ChangeKind::Schema), 1);
        assert!(events.windows(2).all(|pair| pair[0].seq + 1 == pair[1].seq));
    }

    fn drain(live: &mut Receiver<Change>) -> Vec<Change> {
        std::iter::from_fn(|| live.try_recv().ok()).collect()
    }

    fn replay(db: &AgentDb) -> Vec<Change> {
        let mut replayed: Vec<Change> = Vec::new();
        loop {
            let from = replayed.last().map_or(0, |change| change.seq);
            let page = db.changes_since(from).unwrap();
            if page.is_empty() {
                return replayed;
            }
            replayed.extend(page);
        }
    }

    /// Two handles on one file stand in for two server processes.
    #[test]
    fn two_processes_write_one_file_without_losing_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        let first = AgentDb::open(&path, "tenant secret").unwrap();
        setup(&first);
        let owner = first
            .insert("employees", json!({"name": "Anna"}))
            .unwrap()
            .id;
        let second = AgentDb::open(&path, "tenant secret").unwrap();

        thread::scope(|scope| {
            for (agent, db) in [(0, &first), (1, &second)] {
                scope.spawn(move || insert_clients(db, agent, owner));
            }
        });
        let all = second.find(&Query::table("clients").limit(500)).unwrap();
        let ids: Vec<i64> = all.docs.iter().map(|doc| doc.id).collect();
        assert_eq!(ids, (1..=2 * PER_AGENT).collect::<Vec<_>>());

        let outcomes = thread::scope(|scope| {
            let racers = [&first, &second]
                .map(|db| scope.spawn(|| db.update("clients", 1, json!({"revenue": 1}), Some(1))));
            racers.map(|racer| racer.join().unwrap())
        });
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, Err(DbError::VersionConflict { actual: 2, .. })))
        );
        assert!(matches!(
            first.delete("clients", 1, Some(1)),
            Err(DbError::VersionConflict { actual: 2, .. })
        ));
        assert_eq!(second.get("clients", 1).unwrap().version, 2);
    }

    fn balances(db: &AgentDb) -> Vec<i64> {
        let tables = db.describe().unwrap();
        let [table] = tables.as_slice() else {
            panic!("expected exactly one table, got {tables:?}");
        };
        let field = table.fields[0].name.clone();
        let expected = if table.name == "accounts" {
            "balance"
        } else {
            "amount"
        };
        assert_eq!(field, expected);
        db.find(&Query::table(&table.name))
            .map(|page| {
                page.docs
                    .iter()
                    .map(|doc| doc.fields[&field].as_i64().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn move_money_and_reshape(db: &AgentDb, round: i64) {
        let (table, field, new_table, new_field) = if round % 2 == 0 {
            ("accounts", "balance", "ledgers", "amount")
        } else {
            ("ledgers", "amount", "accounts", "balance")
        };
        let set = |id, value: i64| Write::Update {
            table: table.to_owned(),
            id,
            patch: json!({field: value}),
            version: None,
        };
        db.batch(vec![set(1, 100 - round), set(2, round)]).unwrap();
        let changes: Vec<SchemaChange> = serde_json::from_value(json!([
            {"op": "rename_field", "table": table, "field": field, "new_name": new_field},
            {"op": "rename_table", "table": table, "new_name": new_table},
        ]))
        .unwrap();
        db.migrate(&changes).unwrap();
    }

    /// Reads until told to stop and returns how many reads it made.
    fn read_until_stopped(db: &AgentDb, writing: &AtomicBool) -> u32 {
        let mut reads = 0;
        while writing.load(Ordering::Relaxed) {
            let seen = balances(db);
            assert!(
                seen.is_empty() || seen.iter().sum::<i64>() == 100,
                "{seen:?}"
            );
            reads += 1;
        }
        reads
    }

    /// A reader runs while a writer moves money between two accounts in a
    /// batch and renames the table and its field in a migration. Whatever
    /// the reader sees, it is a state from before or after a whole write.
    #[test]
    fn readers_never_see_half_of_a_batch_or_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(dir.path().join("tenant.db"), "tenant secret").unwrap();
        db.define_table(&TableDef::new("accounts").required("balance", FieldType::Number))
            .unwrap();
        db.insert("accounts", json!({"balance": 100})).unwrap();
        db.insert("accounts", json!({"balance": 0})).unwrap();

        let writing = AtomicBool::new(true);
        let reads = thread::scope(|scope| {
            let reader = scope.spawn(|| read_until_stopped(&db, &writing));
            for round in 0..40 {
                move_money_and_reshape(&db, round);
            }
            writing.store(false, Ordering::Relaxed);
            reader.join().unwrap()
        });
        assert!(reads > 0);
        assert_eq!(balances(&db), [61, 39]);
    }

    /// Several agents share one tenant database: they write at the same
    /// time, race on one document, reshape a table, and the file is reopened.
    #[test]
    fn many_agents_share_one_encrypted_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        let db = Arc::new(AgentDb::open(&path, "tenant secret").unwrap());
        setup(&db);
        let mut live = db.subscribe();

        let owner = db.insert("employees", json!({"name": "Anna"})).unwrap().id;
        concurrently(&db, move |db, agent| insert_clients(db, agent, owner));
        check_inserts(&db);

        let outcomes = concurrently(&db, |db, agent| {
            db.update("clients", 1, json!({"revenue": agent}), Some(1))
        });
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        let conflicts = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(DbError::VersionConflict { actual: 2, .. })))
            .count();
        assert_eq!(conflicts, 3);

        db.rename_field("clients", "revenue", "yearly_revenue")
            .unwrap();
        let big = db
            .find(&Query::table("clients").filter("yearly_revenue", Op::Gte, 4900))
            .unwrap();
        assert_eq!(big.total, AGENTS);
        assert!(matches!(
            db.delete("employees", owner, None),
            Err(DbError::StillReferenced { .. })
        ));

        let events = drain(&mut live);
        check_events(&events);
        let last_seq = events.last().unwrap().seq;
        drop(live);
        drop(Arc::into_inner(db).unwrap());

        assert!(matches!(
            AgentDb::open(&path, "wrong"),
            Err(DbError::WrongKey)
        ));
        let reopened = AgentDb::open(&path, "tenant secret").unwrap();
        assert_eq!(
            reopened.find(&Query::table("clients")).unwrap().total,
            AGENTS * PER_AGENT
        );
        assert_eq!(reopened.get("clients", 1).unwrap().version, 2);
        let replayed = replay(&reopened);
        assert_eq!(replayed.last().unwrap().seq, last_seq);
        assert_eq!(replayed.len(), usize::try_from(last_seq).unwrap());
    }
}
