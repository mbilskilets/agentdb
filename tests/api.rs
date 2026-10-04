#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use agentdb::{AgentDb, ChangeKind, DbError, Field, FieldType, Op, Query, TableDef};
    use serde_json::json;

    fn crm() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        define_crm(&db);
        db
    }

    fn define_crm(db: &AgentDb) {
        db.define_table(&TableDef::new("companies").required("name", FieldType::Text))
            .unwrap();
        db.define_table(
            &TableDef::new("clients")
                .required("name", FieldType::Text)
                .optional("email", FieldType::Text)
                .optional(
                    "status",
                    FieldType::Enum {
                        values: vec!["lead".to_owned(), "active".to_owned()],
                    },
                )
                .optional("revenue", FieldType::Number)
                .optional("vip", FieldType::Bool)
                .optional("signed_at", FieldType::Datetime)
                .optional(
                    "company",
                    FieldType::Ref {
                        table: "companies".to_owned(),
                    },
                ),
        )
        .unwrap();
    }

    fn message<T: Debug>(result: agentdb::Result<T>) -> String {
        match result {
            Ok(value) => format!("unexpected success: {value:?}"),
            Err(error) => error.to_string(),
        }
    }

    fn seed(db: &AgentDb) {
        db.insert(
            "clients",
            json!({"name": "Acme", "status": "active", "revenue": 900, "signed_at": "2026-10-01"}),
        )
        .unwrap();
        db.insert(
            "clients",
            json!({"name": "Globex", "status": "lead", "revenue": 150.5, "vip": true}),
        )
        .unwrap();
        db.insert(
            "clients",
            json!({"name": "Acme Labs", "status": "active", "revenue": 4000, "signed_at": "2026-10-03T09:00:00+02:00"}),
        ).unwrap();
    }

    fn names(db: &AgentDb, query: &Query) -> Vec<String> {
        db.find(query)
            .unwrap()
            .docs
            .iter()
            .filter_map(|doc| doc.fields.get("name")?.as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn insert_then_get_returns_the_same_document() {
        let db = crm();
        let inserted = db
            .insert("clients", json!({"name": "Acme", "email": "a@acme.io"}))
            .unwrap();
        assert_eq!(inserted.id, 1);
        assert_eq!(inserted.version, 1);
        assert_eq!(db.get("clients", 1).unwrap(), inserted);
    }

    #[test]
    fn serialized_document_is_flat() {
        let db = crm();
        let doc = db.insert("clients", json!({"name": "Acme"})).unwrap();
        let value = serde_json::to_value(&doc).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["name"], "Acme");
    }

    #[test]
    fn typo_in_field_name_suggests_the_real_field() {
        let db = crm();
        let text = message(db.insert("clients", json!({"name": "Acme", "emial": "a@acme.io"})));
        assert_eq!(
            text,
            "unknown field `emial` on table `clients`. Did you mean `email`? Valid fields: name, email, status, revenue, vip, signed_at, company."
        );
    }

    #[test]
    fn typo_in_table_name_suggests_the_real_table() {
        let db = crm();
        let text = message(db.get("client", 1));
        assert_eq!(
            text,
            "unknown table `client`. Did you mean `clients`? Existing tables: clients, companies."
        );
    }

    #[test]
    fn schema_violations_explain_the_fix() {
        let db = crm();
        assert_eq!(
            message(db.insert("clients", json!({"name": "Acme", "revenue": "900"}))),
            "field `revenue` on table `clients` expects number, got \"900\"."
        );
        assert_eq!(
            message(db.insert("clients", json!({"name": "Acme", "status": "won"}))),
            "field `status` on table `clients` expects one of: lead, active, got \"won\"."
        );
        assert_eq!(
            message(db.insert("clients", json!({"email": "a@acme.io"}))),
            "table `clients` requires these fields and they are missing: name."
        );
        assert!(matches!(
            db.insert("clients", json!({"name": "Acme", "id": 7})),
            Err(DbError::ReservedField { .. })
        ));
        assert!(matches!(
            db.insert("clients", json!(["Acme"])),
            Err(DbError::NotAnObject { .. })
        ));
    }

    #[test]
    fn references_are_checked_both_ways() {
        let db = crm();
        assert!(matches!(
            db.insert("clients", json!({"name": "Acme", "company": 9})),
            Err(DbError::BrokenReference { id: 9, .. })
        ));
        let company = db
            .insert("companies", json!({"name": "Acme Corp"}))
            .unwrap();
        let client = db
            .insert("clients", json!({"name": "Ann", "company": company.id}))
            .unwrap();
        assert_eq!(
            message(db.delete("companies", company.id)),
            "cannot delete `companies` id 1: 1 document(s) in `clients` point to it through `company`. Update or delete those first."
        );
        db.delete("clients", client.id).unwrap();
        db.delete("companies", company.id).unwrap();
    }

    #[test]
    fn update_patches_fields_and_null_removes_them() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme", "email": "a@acme.io"}))
            .unwrap();
        let updated = db
            .update("clients", 1, json!({"revenue": 10, "email": null}), None)
            .unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(
            updated.fields,
            json!({"name": "Acme", "revenue": 10})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
        assert!(matches!(
            db.update("clients", 1, json!({"name": null}), None),
            Err(DbError::MissingRequired { .. })
        ));
    }

    #[test]
    fn stale_version_is_refused() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        db.update("clients", 1, json!({"revenue": 1}), Some(1))
            .unwrap();
        assert_eq!(
            message(db.update("clients", 1, json!({"revenue": 2}), Some(1))),
            "`clients` id 1 is at version 2, but the update expected version 1. Someone else changed it: read it again with get() and retry with version 2."
        );
    }

    #[test]
    fn ids_are_not_reused_after_delete() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        db.delete("clients", 1).unwrap();
        assert_eq!(
            db.insert("clients", json!({"name": "Globex"})).unwrap().id,
            2
        );
        assert!(matches!(
            db.get("clients", 1),
            Err(DbError::NotFound { .. })
        ));
    }

    #[test]
    fn find_filters_by_each_field_type() {
        let db = crm();
        seed(&db);
        let table = || Query::table("clients");
        assert_eq!(
            names(&db, &table().filter("status", Op::Eq, "active")),
            ["Acme", "Acme Labs"]
        );
        assert_eq!(
            names(&db, &table().filter("revenue", Op::Gt, 500)),
            ["Acme", "Acme Labs"]
        );
        assert_eq!(
            names(&db, &table().filter("revenue", Op::Lt, 200)),
            ["Globex"]
        );
        assert_eq!(
            names(&db, &table().filter("name", Op::Contains, "acme l")),
            ["Acme Labs"]
        );
        assert_eq!(names(&db, &table().filter("vip", Op::Eq, true)), ["Globex"]);
        assert_eq!(
            names(&db, &table().filter("signed_at", Op::Gte, "2026-10-02")),
            ["Acme Labs"]
        );
        assert_eq!(
            names(&db, &table().filter("signed_at", Op::Eq, json!(null))),
            ["Globex"]
        );
        assert_eq!(
            names(&db, &table().filter("id", Op::Gte, 2)),
            ["Globex", "Acme Labs"]
        );
        assert_eq!(
            names(
                &db,
                &table()
                    .filter("status", Op::Eq, "active")
                    .filter("revenue", Op::Lt, 1000)
            ),
            ["Acme"]
        );
    }

    #[test]
    fn ne_also_matches_documents_where_the_field_is_unset() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme", "email": "a@acme.io"}))
            .unwrap();
        db.insert("clients", json!({"name": "Globex", "email": "g@globex.io"}))
            .unwrap();
        db.insert("clients", json!({"name": "Initech"})).unwrap();
        assert_eq!(
            names(
                &db,
                &Query::table("clients").filter("email", Op::Ne, "a@acme.io")
            ),
            ["Globex", "Initech"]
        );
        assert_eq!(
            names(
                &db,
                &Query::table("clients").filter("email", Op::Ne, json!(null))
            ),
            ["Acme", "Globex"]
        );
    }

    #[test]
    fn contains_ignores_case_in_any_language() {
        let db = crm();
        for name in ["Łódź Fabryczna", "ŻABKA Polska", "Zabka"] {
            db.insert("clients", json!({"name": name})).unwrap();
        }
        let containing = |text: &str| {
            names(
                &db,
                &Query::table("clients").filter("name", Op::Contains, text),
            )
        };
        assert_eq!(containing("łódź"), ["Łódź Fabryczna"]);
        assert_eq!(containing("ŁÓDŹ"), ["Łódź Fabryczna"]);
        assert_eq!(containing("żabka"), ["ŻABKA Polska"]);
        assert_eq!(containing("abka"), ["ŻABKA Polska", "Zabka"]);
    }

    #[test]
    fn datetimes_are_stored_in_utc() {
        let db = crm();
        seed(&db);
        assert_eq!(
            db.get("clients", 3).unwrap().fields["signed_at"],
            "2026-10-03T07:00:00Z"
        );
        assert_eq!(
            db.get("clients", 1).unwrap().fields["signed_at"],
            "2026-10-01T00:00:00Z"
        );
    }

    #[test]
    fn find_sorts_and_pages() {
        let db = crm();
        seed(&db);
        let query = Query::table("clients").sort("revenue", true).limit(2);
        let first = db.find(&query).unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(first.next_offset, Some(2));
        assert_eq!(names(&db, &query), ["Acme Labs", "Acme"]);
        let last = db.find(&query.offset(2)).unwrap();
        assert_eq!(last.docs.len(), 1);
        assert_eq!(last.next_offset, None);
    }

    #[test]
    fn bad_queries_explain_the_fix() {
        let db = crm();
        assert_eq!(
            message(db.find(&Query::table("clients").filter("revenue", Op::Contains, "9"))),
            "operator `contains` does not work on field `revenue` (number). Operators for this field: eq, ne, gt, gte, lt, lte."
        );
        assert_eq!(
            message(db.find(&Query::table("clients").filter("created", Op::Gt, "2026-01-01"))),
            "unknown field `created` on table `clients`. Did you mean `created_at`? Valid fields: name, email, status, revenue, vip, signed_at, company, id, version, created_at, updated_at."
        );
        assert!(matches!(
            db.find(&Query::table("clients").filter("signed_at", Op::Gt, "yesterday")),
            Err(DbError::WrongType { .. })
        ));
    }

    #[test]
    fn query_can_be_built_from_json() {
        let db = crm();
        seed(&db);
        let query: Query = serde_json::from_value(json!({
            "table": "clients",
            "where": [{"field": "status", "op": "eq", "value": "lead"}],
            "sort": {"field": "name"},
        }))
        .unwrap();
        assert_eq!(names(&db, &query), ["Globex"]);
    }

    #[test]
    fn subscribers_and_the_change_log_see_every_write_in_order() {
        let db = crm();
        let setup = db.changes_since(0).unwrap();
        let schema_kinds: Vec<_> = setup.iter().map(|change| change.kind).collect();
        assert_eq!(schema_kinds, [ChangeKind::Schema, ChangeKind::Schema]);
        let start = setup.last().map_or(0, |change| change.seq);

        let live = db.subscribe().unwrap();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        db.update("clients", 1, json!({"revenue": 5}), None)
            .unwrap();
        db.delete("clients", 1).unwrap();
        db.insert("clients", json!({"nope": 1})).unwrap_err();

        let received: Vec<_> = live.try_iter().collect();
        let kinds: Vec<_> = received.iter().map(|change| change.kind).collect();
        assert_eq!(
            kinds,
            [ChangeKind::Insert, ChangeKind::Update, ChangeKind::Delete]
        );
        assert_eq!(received, db.changes_since(start).unwrap());
        let seqs: Vec<_> = received.iter().map(|change| change.seq - start).collect();
        assert_eq!(seqs, [1, 2, 3]);
        let last = received.last().and_then(|change| change.doc.as_ref());
        assert_eq!(
            last.map(|doc| doc.fields["revenue"].clone()),
            Some(json!(5))
        );
    }

    #[test]
    fn schema_can_grow() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        let field = |required| Field::new("phone", FieldType::Text, required);
        assert!(matches!(
            db.add_field("clients", field(true)),
            Err(DbError::RequiredFieldOnExistingDocs { count: 1, .. })
        ));
        db.add_field("clients", field(false)).unwrap();
        db.update("clients", 1, json!({"phone": "555"}), None)
            .unwrap();
        assert!(matches!(
            db.add_field("clients", field(false)),
            Err(DbError::FieldExists { .. })
        ));
        assert!(matches!(
            db.define_table(&TableDef::new("clients")),
            Err(DbError::TableExists { .. })
        ));
        assert!(matches!(
            db.define_table(&TableDef::new("Bad Name")),
            Err(DbError::InvalidName { .. })
        ));
        let described = db.describe().unwrap();
        let clients = described.iter().find(|table| table.name == "clients");
        assert_eq!(
            clients.map(|table| (table.count, table.fields.len())),
            Some((1, 8))
        );
    }

    #[test]
    fn file_is_encrypted_and_needs_the_right_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        {
            let db = AgentDb::open(&path, "correct horse").unwrap();
            define_crm(&db);
            db.insert("clients", json!({"name": "Acme Secret Client"}))
                .unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let leaked = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
        assert!(!leaked(b"Acme Secret Client"));
        assert!(!leaked(b"SQLite format"));

        assert!(matches!(
            AgentDb::open(&path, "wrong key"),
            Err(DbError::WrongKey)
        ));
        assert!(matches!(AgentDb::open(&path, ""), Err(DbError::EmptyKey)));
        let reopened = AgentDb::open(&path, "correct horse").unwrap();
        assert_eq!(
            reopened.get("clients", 1).unwrap().fields["name"],
            "Acme Secret Client"
        );
    }
}
