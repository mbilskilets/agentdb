#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use agentdb::{AgentDb, Change, ChangeKind, DbError, Op, Query, SchemaChange};
    use serde_json::json;

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

    /// Several agents share one tenant database: they write at the same
    /// time, race on one document, reshape a table, and the file is reopened.
    #[test]
    fn many_agents_share_one_encrypted_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        let db = Arc::new(AgentDb::open(&path, "tenant secret").unwrap());
        setup(&db);
        let live = db.subscribe().unwrap();

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
            db.delete("employees", owner),
            Err(DbError::StillReferenced { .. })
        ));

        let events: Vec<_> = live.try_iter().collect();
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
