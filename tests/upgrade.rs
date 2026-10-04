#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdb::{AgentDb, ChangeKind, DbError, Op, Query};
    use rusqlite::Connection;
    use serde_json::json;

    const KEY: &str = "tenant secret";

    /// The storage tables as format version 1 created them.
    const VERSION_1_TABLES: &str = "
        CREATE TABLE _tables (
            name TEXT PRIMARY KEY,
            schema TEXT NOT NULL,
            next_id INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE docs (
            tbl TEXT NOT NULL,
            id INTEGER NOT NULL,
            version INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            body TEXT NOT NULL,
            PRIMARY KEY (tbl, id)
        );
        CREATE TABLE changes (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            tbl TEXT NOT NULL,
            kind TEXT NOT NULL,
            at TEXT NOT NULL,
            doc TEXT NOT NULL
        );
        PRAGMA user_version = 1;
    ";

    /// What version 1 stored for two companies and three clients, one of
    /// which was deleted again.
    const VERSION_1_CONTENT: &str = r#"
        INSERT INTO _tables VALUES ('companies',
            '{"name":"companies","fields":[{"name":"name","type":"text","required":true}]}', 3);
        INSERT INTO _tables VALUES ('clients',
            '{"name":"clients","fields":[{"name":"name","type":"text","required":true},{"name":"company","type":"ref","table":"companies","required":false}]}', 4);
        INSERT INTO _tables VALUES ('notes',
            '{"name":"notes","fields":[{"name":"text","type":"text","required":true}]}', 1);
        INSERT INTO docs VALUES ('companies', 1, 1, '2026-10-01T08:00:00Z', '2026-10-01T08:00:00Z', '{"name":"Northwind"}');
        INSERT INTO docs VALUES ('companies', 2, 1, '2026-10-01T08:00:00Z', '2026-10-01T08:00:00Z', '{"name":"Initech"}');
        INSERT INTO docs VALUES ('clients', 1, 2, '2026-10-01T09:00:00Z', '2026-10-02T09:00:00Z', '{"name":"Acme","company":1}');
        INSERT INTO docs VALUES ('clients', 3, 1, '2026-10-03T09:00:00Z', '2026-10-03T09:00:00Z', '{"name":"Globex"}');
        INSERT INTO changes (tbl, kind, at, doc) VALUES
            ('companies', 'schema', '2026-10-01T08:00:00Z', 'null'),
            ('clients', 'insert', '2026-10-03T09:00:00Z',
             '{"id":3,"version":1,"created_at":"2026-10-03T09:00:00Z","updated_at":"2026-10-03T09:00:00Z","name":"Globex"}');
    "#;

    fn raw(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "key", KEY).unwrap();
        conn
    }

    fn format_version(path: &Path) -> i64 {
        raw(path)
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_version_1_file_is_upgraded_when_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        raw(&path)
            .execute_batch(&format!("{VERSION_1_TABLES}{VERSION_1_CONTENT}"))
            .unwrap();

        let db = AgentDb::open(&path, KEY).unwrap();
        let tables = db.describe().unwrap();
        let counts: Vec<_> = tables
            .iter()
            .map(|table| (table.name.as_str(), table.count))
            .collect();
        assert_eq!(counts, [("clients", 2), ("companies", 2), ("notes", 0)]);
        let company = &tables[0].fields[1];
        assert!(company.indexed);

        assert_eq!(db.get("clients", 1).unwrap().version, 2);
        let of_northwind = Query::table("clients").filter("company", Op::Eq, 1);
        assert_eq!(db.find(&of_northwind).unwrap().total, 1);
        assert!(matches!(
            db.delete("companies", 1, None),
            Err(DbError::StillReferenced { count: 1, .. })
        ));
        assert_eq!(
            db.insert("clients", json!({"name": "Hooli"})).unwrap().id,
            4
        );
        assert_eq!(db.insert("notes", json!({"text": "hi"})).unwrap().id, 1);

        let kinds: Vec<_> = db
            .changes_since(0)
            .unwrap()
            .iter()
            .map(|change| change.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ChangeKind::Schema,
                ChangeKind::Insert,
                ChangeKind::Insert,
                ChangeKind::Insert
            ]
        );
        assert_eq!(db.latest_seq().unwrap(), 4);

        drop(db);
        assert_eq!(format_version(&path), 2);
        let reopened = AgentDb::open(&path, KEY).unwrap();
        assert_eq!(reopened.find(&Query::table("clients")).unwrap().total, 3);
    }

    #[test]
    fn an_upgrade_that_fails_leaves_the_file_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        raw(&path)
            .execute_batch(&format!(
                "{VERSION_1_TABLES}{VERSION_1_CONTENT}
                 INSERT INTO _tables VALUES ('broken', 'not a schema', 1);"
            ))
            .unwrap();

        assert!(matches!(
            AgentDb::open(&path, KEY),
            Err(DbError::Internal(_))
        ));
        assert_eq!(format_version(&path), 1);
        let next_id: i64 = raw(&path)
            .query_row(
                "SELECT next_id FROM _tables WHERE name = 'clients'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(next_id, 4);
    }

    #[test]
    fn a_file_from_a_newer_agentdb_is_refused_untouched() {
        for journal_mode in ["DELETE", "WAL"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tenant.db");
            let newer = raw(&path);
            newer
                .pragma_update(None, "journal_mode", journal_mode)
                .unwrap();
            newer
                .execute_batch(&format!("{VERSION_1_TABLES} PRAGMA user_version = 3;"))
                .unwrap();
            drop(newer);
            let written = std::fs::read(&path).unwrap();

            assert_eq!(
                AgentDb::open(&path, KEY).unwrap_err().to_string(),
                "this database file uses storage format 3, but this version of agentdb only understands up to 2. Upgrade agentdb."
            );
            assert_eq!(std::fs::read(&path).unwrap(), written);
            let files: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(files, ["tenant.db"]);
        }
    }
}
