#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use agentdb::{
        AgentDb, Change, ChangeKind, DbError, Doc, Field, FieldType, Op, Query, SchemaChange,
        TableDef, Write,
    };
    use serde_json::json;
    use tokio::sync::broadcast::Receiver;

    fn crm() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(&TableDef::new("companies").required("name", FieldType::Text))
            .unwrap();
        db.define_table(
            &TableDef::new("clients")
                .required("name", FieldType::Text)
                .optional("mail", FieldType::Text)
                .optional("vip", FieldType::Bool)
                .optional(
                    "status",
                    FieldType::Enum {
                        values: vec!["lead".to_owned()],
                    },
                )
                .optional(
                    "company",
                    FieldType::Ref {
                        table: "companies".to_owned(),
                    },
                ),
        )
        .unwrap();
        db.insert("companies", json!({"name": "Northwind"}))
            .unwrap();
        db.insert(
            "clients",
            json!({"name": "Acme", "mail": "a@acme.io", "vip": true, "company": 1}),
        )
        .unwrap();
        db.insert("clients", json!({"name": "Globex"})).unwrap();
        db
    }

    fn drain(live: &mut Receiver<Change>) -> Vec<Change> {
        std::iter::from_fn(|| live.try_recv().ok()).collect()
    }

    fn message<T: Debug>(result: agentdb::Result<T>) -> String {
        match result {
            Ok(value) => format!("unexpected success: {value:?}"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn renaming_a_field_moves_the_data_and_keeps_its_type() {
        let db = crm();
        db.rename_field("clients", "mail", "email").unwrap();
        db.rename_field("clients", "vip", "important").unwrap();
        let acme = db.get("clients", 1).unwrap();
        assert_eq!(acme.fields["email"], "a@acme.io");
        assert_eq!(acme.fields["important"], true);
        assert_eq!(acme.fields.get("mail"), None);
        assert_eq!(acme.version, 1);
        let found = db
            .find(&Query::table("clients").filter("email", Op::Contains, "acme"))
            .unwrap();
        assert_eq!(found.total, 1);
        assert_eq!(
            message(db.insert("clients", json!({"name": "New", "mail": "x@y.z"}))),
            "unknown field `mail` on table `clients`. Did you mean `email`? Valid fields: name, email, important, status, company."
        );
        assert!(matches!(
            db.rename_field("clients", "email", "name"),
            Err(DbError::FieldExists { .. })
        ));
        assert!(matches!(
            db.rename_field("clients", "email", "id"),
            Err(DbError::ReservedField { .. })
        ));
    }

    const STORED_AT: &str = "2026-10-01T08:00:00Z";
    const MIGRATED_AT: &str = "2026-10-02T08:00:00Z";

    /// A table with a field of every type and three documents stored at
    /// [`STORED_AT`]: one sets every field, one sets the values a careless
    /// rewrite would change (`2.5`, `false`, an empty text), one sets none.
    fn every_field_type() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(&TableDef::new("teams").required("name", FieldType::Text))
            .unwrap();
        db.define_table(
            &TableDef::new("tasks")
                .optional("title", FieldType::Text)
                .optional("points", FieldType::Number)
                .optional("done", FieldType::Bool)
                .optional("due", FieldType::Datetime)
                .optional(
                    "stage",
                    FieldType::Enum {
                        values: vec!["open".to_owned(), "won".to_owned()],
                    },
                )
                .optional(
                    "team",
                    FieldType::Ref {
                        table: "teams".to_owned(),
                    },
                ),
        )
        .unwrap();
        db.freeze_time(Some(STORED_AT)).unwrap();
        db.insert("teams", json!({"name": "Sales"})).unwrap();
        db.insert(
            "tasks",
            json!({"title": "Say \"hi\" to Łódź\n🙂", "points": 3, "done": true,
                   "due": "2026-10-03", "stage": "won", "team": 1}),
        )
        .unwrap();
        db.insert("tasks", json!({"title": "", "points": 2.5, "done": false}))
            .unwrap();
        db.insert("tasks", json!({})).unwrap();
        db.freeze_time(Some(MIGRATED_AT)).unwrap();
        db
    }

    fn tasks(db: &AgentDb) -> serde_json::Value {
        serde_json::to_value(db.find(&Query::table("tasks")).unwrap().docs).unwrap()
    }

    #[test]
    fn renaming_a_field_keeps_every_value_exactly_as_it_was_stored() {
        let db = every_field_type();
        for field in ["title", "points", "done", "due", "stage", "team"] {
            db.rename_field("tasks", field, &format!("new_{field}"))
                .unwrap();
        }
        assert_eq!(
            tasks(&db),
            json!([
                {"id": 1, "version": 1, "created_at": STORED_AT, "updated_at": STORED_AT,
                 "new_title": "Say \"hi\" to Łódź\n🙂", "new_points": 3, "new_done": true,
                 "new_due": "2026-10-03T00:00:00Z", "new_stage": "won", "new_team": 1},
                {"id": 2, "version": 1, "created_at": STORED_AT, "updated_at": STORED_AT,
                 "new_title": "", "new_points": 2.5, "new_done": false},
                {"id": 3, "version": 1, "created_at": STORED_AT, "updated_at": STORED_AT},
            ])
        );
    }

    #[test]
    fn removing_a_field_leaves_every_other_value_exactly_as_it_was_stored() {
        let db = every_field_type();
        db.remove_field("tasks", "title", true).unwrap();
        db.remove_field("tasks", "stage", true).unwrap();
        assert_eq!(
            tasks(&db),
            json!([
                {"id": 1, "version": 1, "created_at": STORED_AT, "updated_at": STORED_AT,
                 "points": 3, "done": true, "due": "2026-10-03T00:00:00Z", "team": 1},
                {"id": 2, "version": 1, "created_at": STORED_AT, "updated_at": STORED_AT,
                 "points": 2.5, "done": false},
                {"id": 3, "version": 1, "created_at": STORED_AT, "updated_at": STORED_AT},
            ])
        );
        for field in ["points", "done", "due", "team"] {
            db.remove_field("tasks", field, true).unwrap();
        }
        let left: Vec<_> = db.find(&Query::table("tasks")).unwrap().docs;
        assert!(left.iter().all(|task| task.fields.is_empty()));
    }

    #[test]
    fn renaming_a_table_keeps_documents_and_links() {
        let db = crm();
        db.rename_table("companies", "accounts").unwrap();
        assert_eq!(db.get("accounts", 1).unwrap().fields["name"], "Northwind");
        assert!(matches!(
            db.get("companies", 1),
            Err(DbError::UnknownTable { .. })
        ));
        assert_eq!(
            message(db.insert("clients", json!({"name": "New", "company": 9}))),
            "field `company` points to `accounts` id 9, which does not exist. Insert that `accounts` document first or use an existing id."
        );
        assert!(matches!(
            db.delete("accounts", 1, None),
            Err(DbError::StillReferenced { .. })
        ));
        assert!(matches!(
            db.rename_table("accounts", "clients"),
            Err(DbError::TableExists { .. })
        ));
    }

    #[test]
    fn removing_data_needs_force() {
        let db = crm();
        assert_eq!(
            message(db.remove_field("clients", "mail", false)),
            "this would permanently delete the `mail` value of `clients` documents, affecting 1 document(s). Call again with force = true if that is intended."
        );
        assert_eq!(db.get("clients", 1).unwrap().fields["mail"], "a@acme.io");
        db.remove_field("clients", "status", false).unwrap();
        db.remove_field("clients", "mail", true).unwrap();
        assert_eq!(db.get("clients", 1).unwrap().fields.get("mail"), None);
        assert!(matches!(
            db.remove_field("clients", "mail", true),
            Err(DbError::UnknownField { .. })
        ));
    }

    #[test]
    fn dropping_a_table_needs_force_and_no_links() {
        let db = crm();
        assert_eq!(
            message(db.drop_table("companies", true)),
            "cannot drop table `companies`: field `company` on table `clients` links to it. Remove that field first."
        );
        assert_eq!(
            message(db.drop_table("clients", false)),
            "this would permanently delete table `clients` and everything in it, affecting 2 document(s). Call again with force = true if that is intended."
        );
        db.drop_table("clients", true).unwrap();
        db.drop_table("companies", true).unwrap();
        assert_eq!(db.describe().unwrap(), []);
    }

    #[test]
    fn a_field_can_become_required_once_every_document_has_it() {
        let db = crm();
        assert_eq!(
            message(db.set_required("clients", "mail", true)),
            "cannot make `mail` required on `clients`: 1 document(s) have no value for it. Fill them in first; find them by filtering `mail` eq null."
        );
        db.update("clients", 2, json!({"mail": "g@globex.io"}), None)
            .unwrap();
        db.set_required("clients", "mail", true).unwrap();
        assert!(matches!(
            db.insert("clients", json!({"name": "New"})),
            Err(DbError::MissingRequired { .. })
        ));
        db.set_required("clients", "mail", false).unwrap();
        db.insert("clients", json!({"name": "New"})).unwrap();
    }

    #[test]
    fn enum_values_can_be_added() {
        let db = crm();
        db.insert("clients", json!({"name": "New", "status": "active"}))
            .unwrap_err();
        db.add_enum_value("clients", "status", "active").unwrap();
        db.insert("clients", json!({"name": "New", "status": "active"}))
            .unwrap();
        assert!(matches!(
            db.add_enum_value("clients", "status", "active"),
            Err(DbError::EnumValueExists { .. })
        ));
        assert!(matches!(
            db.add_enum_value("clients", "name", "x"),
            Err(DbError::NotAnEnum { .. })
        ));
    }

    #[test]
    fn enum_values_must_be_distinct_and_not_empty() {
        let db = crm();
        let stage = |values: &[&str]| {
            TableDef::new("deals").required(
                "stage",
                FieldType::Enum {
                    values: values.iter().map(|&value| value.to_owned()).collect(),
                },
            )
        };
        assert_eq!(
            message(db.define_table(&stage(&["open", "won", "open"]))),
            "enum field `stage` lists `open` more than once. List every allowed value exactly once."
        );
        assert_eq!(
            message(db.define_table(&stage(&["open", ""]))),
            "enum field `stage` cannot allow an empty value. Give every allowed value at least one character; leave the field unset to mean \"no value\"."
        );
        assert!(matches!(
            db.add_enum_value("clients", "status", ""),
            Err(DbError::BlankEnumValue { .. })
        ));
        assert!(matches!(
            db.change_field_type(
                "clients",
                "name",
                FieldType::Enum {
                    values: vec!["Acme".to_owned(), "Acme".to_owned()],
                },
            ),
            Err(DbError::RepeatedEnumValue { .. })
        ));
        db.define_table(&stage(&["open", "won"])).unwrap();
    }

    #[test]
    fn renaming_a_table_renames_it_in_the_change_log() {
        let db = crm();
        db.rename_table("clients", "customers").unwrap();
        let tables: Vec<String> = db
            .changes_since(0)
            .unwrap()
            .into_iter()
            .filter(|change| change.kind == ChangeKind::Insert)
            .map(|change| change.table)
            .collect();
        assert_eq!(tables, ["companies", "customers", "customers"]);
    }

    #[test]
    fn ids_continue_after_a_table_is_dropped_and_defined_again() {
        let db = crm();
        let notes = TableDef::new("notes").required("text", FieldType::Text);
        db.define_table(&notes).unwrap();
        for text in ["one", "two", "three"] {
            db.insert("notes", json!({"text": text})).unwrap();
        }
        db.drop_table("notes", true).unwrap();
        db.define_table(&notes).unwrap();
        assert_eq!(db.insert("notes", json!({"text": "four"})).unwrap().id, 4);

        db.rename_table("notes", "memos").unwrap();
        db.define_table(&notes).unwrap();
        assert_eq!(db.insert("notes", json!({"text": "five"})).unwrap().id, 5);
        assert_eq!(db.insert("memos", json!({"text": "six"})).unwrap().id, 5);

        db.drop_table("memos", true).unwrap();
        db.rename_table("notes", "memos").unwrap();
        assert_eq!(db.insert("memos", json!({"text": "seven"})).unwrap().id, 6);
    }

    fn people() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("people")
                .required("name", FieldType::Text)
                .optional("email", FieldType::Text)
                .optional("age", FieldType::Text),
        )
        .unwrap();
        for (name, email, age) in [
            ("Ann", Some("ann@acme.io"), "41"),
            ("Bob", Some("bob@acme.io"), "41.0"),
            ("Cy", None, "29"),
            ("Di", Some("bob@acme.io"), "30"),
            ("Ed", None, "31"),
        ] {
            db.insert("people", json!({"name": name, "email": email, "age": age}))
                .unwrap();
        }
        db
    }

    #[test]
    fn a_field_cannot_become_unique_while_documents_share_a_value() {
        let db = people();
        assert_eq!(
            message(db.set_unique("people", "email", true)),
            "cannot make `email` unique on `people`: 2 documents hold \"bob@acme.io\", for example ids 2 and 4. Change or delete all but one of them first; nothing was changed."
        );
        db.insert("people", json!({"name": "Flo", "email": "ann@acme.io"}))
            .unwrap();
        db.delete("people", 6, None).unwrap();

        db.update("people", 4, json!({"email": "di@acme.io"}), None)
            .unwrap();
        db.set_unique("people", "email", true).unwrap();
        assert!(matches!(
            db.insert("people", json!({"name": "Flo", "email": "ann@acme.io"})),
            Err(DbError::DuplicateValue { id: 1, .. })
        ));
        db.insert("people", json!({"name": "Flo"})).unwrap();

        db.set_unique("people", "email", false).unwrap();
        db.insert("people", json!({"name": "Gus", "email": "ann@acme.io"}))
            .unwrap();
    }

    #[test]
    fn a_type_change_cannot_make_a_unique_field_hold_duplicates() {
        let db = people();
        db.set_unique("people", "age", true).unwrap();
        assert_eq!(
            message(db.change_field_type("people", "age", FieldType::Number)),
            "cannot make `age` unique on `people`: 2 documents hold 41, for example ids 1 and 2. Change or delete all but one of them first; nothing was changed."
        );
        assert_eq!(db.get("people", 2).unwrap().fields["age"], "41.0");
        db.update("people", 2, json!({"age": "42"}), None).unwrap();
        db.change_field_type("people", "age", FieldType::Number)
            .unwrap();
        assert!(matches!(
            db.update("people", 3, json!({"age": 42.0}), None),
            Err(DbError::DuplicateValue { id: 2, .. })
        ));
    }

    #[test]
    fn unique_and_ref_fields_are_always_indexed() {
        let db = crm();
        db.set_unique("clients", "mail", true).unwrap();
        let tables = serde_json::to_value(db.describe().unwrap()).unwrap();
        let clients = tables
            .as_array()
            .unwrap()
            .iter()
            .find(|table| table["name"] == "clients")
            .unwrap();
        assert_eq!(
            clients["fields"],
            json!([
                {"name": "name", "type": "text", "required": true},
                {"name": "mail", "type": "text", "required": false, "indexed": true, "unique": true},
                {"name": "vip", "type": "bool", "required": false},
                {"name": "status", "type": "enum", "values": ["lead"], "required": false},
                {"name": "company", "type": "ref", "table": "companies", "required": false, "indexed": true},
            ])
        );
        assert_eq!(
            message(db.set_indexed("clients", "mail", false)),
            "`mail` on `clients` has to stay indexed because it is unique; turn that off first with the set_unique schema change."
        );
        assert_eq!(
            message(db.set_indexed("clients", "company", false)),
            "`company` on `clients` has to stay indexed because it is a ref, and deleting a document looks up what points to it."
        );
        db.set_unique("clients", "mail", false).unwrap();
        db.set_indexed("clients", "mail", false).unwrap();
        assert!(matches!(
            db.set_indexed("clients", "phone", true),
            Err(DbError::UnknownField { .. })
        ));
    }

    #[test]
    fn a_table_is_limited_to_10_indexed_fields() {
        let db = crm();
        let wide = (0..11).fold(TableDef::new("wide"), |def, n| {
            def.with(Field::new(format!("f{n}"), FieldType::Text, false).indexed())
        });
        assert_eq!(
            message(db.define_table(&wide)),
            "table `wide` cannot have more than 10 indexed fields, because every index slows down each write. It would have these: f0, f1, f2, f3, f4, f5, f6, f7, f8, f9, f10. Ref and unique fields are always indexed. Stop indexing a field that no query filters or sorts by, using the schema change {\"op\": \"set_indexed\", \"table\": \"wide\", \"field\": \"<field>\", \"indexed\": false}."
        );
        let mut fits = wide;
        fits.fields.truncate(10);
        db.define_table(&fits).unwrap();
        let company = FieldType::Ref {
            table: "companies".to_owned(),
        };
        assert!(matches!(
            db.add_field("wide", Field::new("company", company, false)),
            Err(DbError::TooManyIndexes { max: 10, .. })
        ));
        db.add_field("wide", Field::new("note", FieldType::Text, false))
            .unwrap();
        assert!(matches!(
            db.set_unique("wide", "note", true),
            Err(DbError::TooManyIndexes { .. })
        ));
        db.set_indexed("wide", "f0", false).unwrap();
        db.set_unique("wide", "note", true).unwrap();
    }

    /// Enough documents that every query has to go through an index.
    fn large() -> AgentDb {
        let db = crm();
        db.define_table(
            &TableDef::new("events")
                .with(Field::new("kind", FieldType::Text, true).indexed())
                .with(Field::new("score", FieldType::Text, true).indexed())
                .optional(
                    "company",
                    FieldType::Ref {
                        table: "companies".to_owned(),
                    },
                ),
        )
        .unwrap();
        let writes: Vec<Write> = (0..1001)
            .map(|n| {
                let kind = if n % 2 == 0 { "click" } else { "view" };
                let company = (n == 7).then_some(1);
                Write::Insert {
                    table: "events".to_owned(),
                    doc: json!({"kind": kind, "score": n.to_string(), "company": company}),
                }
            })
            .collect();
        for batch in writes.chunks(500) {
            db.batch(batch.to_vec()).unwrap();
        }
        db
    }

    #[test]
    fn indexes_keep_answering_queries_through_schema_changes() {
        let db = large();
        let total = |query: Query| db.find(&query).unwrap().total;
        let changes: Vec<SchemaChange> = serde_json::from_value(json!([
            {"op": "rename_field", "table": "events", "field": "kind", "new_name": "action"},
            {"op": "change_type", "table": "events", "field": "score", "to": {"type": "number"}},
            {"op": "rename_table", "table": "events", "new_name": "actions"},
            {"op": "rename_table", "table": "companies", "new_name": "accounts"},
        ]))
        .unwrap();
        db.migrate(&changes).unwrap();
        let actions = || Query::table("actions");
        assert_eq!(total(actions().filter("action", Op::Eq, "view")), 500);
        assert_eq!(total(actions().filter("score", Op::Gte, 990)), 11);
        assert_eq!(total(actions().filter("company", Op::Eq, 1)), 1);
        assert_eq!(
            message(db.delete("accounts", 1, None)),
            "cannot delete `accounts` id 1: 1 document(s) in `actions` point to it through `company`. Update or delete those first."
        );
        db.update("actions", 8, json!({"company": null}), None)
            .unwrap();
        assert_eq!(
            message(db.delete("accounts", 1, None)),
            "cannot delete `accounts` id 1: 1 document(s) in `clients` point to it through `company`. Update or delete those first."
        );
        db.update("clients", 1, json!({"company": null}), None)
            .unwrap();
        db.delete("accounts", 1, None).unwrap();

        db.set_indexed("actions", "action", false).unwrap();
        assert!(matches!(
            db.find(&actions().filter("action", Op::Eq, "view")),
            Err(DbError::QueryNeedsIndex { .. })
        ));
        db.drop_table("actions", true).unwrap();
        db.define_table(
            &TableDef::new("actions").with(Field::new("score", FieldType::Number, true).indexed()),
        )
        .unwrap();
        assert_eq!(total(actions().filter("score", Op::Gte, 0)), 0);
    }

    #[test]
    fn indexes_can_be_changed_as_json() {
        let db = crm();
        let changes: Vec<SchemaChange> = serde_json::from_value(json!([
            {"op": "set_indexed", "table": "clients", "field": "name", "indexed": true},
            {"op": "set_unique", "table": "clients", "field": "mail", "unique": true},
            {"op": "add_field", "table": "clients", "field":
                {"name": "code", "type": "text", "required": false, "unique": true}},
        ]))
        .unwrap();
        db.migrate(&changes).unwrap();
        let tables = db.describe().unwrap();
        let clients = tables.iter().find(|table| table.name == "clients").unwrap();
        let flags: Vec<_> = clients
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.indexed, field.unique))
            .collect();
        assert_eq!(
            flags,
            [
                ("name", true, false),
                ("mail", true, true),
                ("vip", false, false),
                ("status", false, false),
                ("company", true, false),
                ("code", true, true),
            ]
        );
    }

    #[test]
    fn schema_changes_reach_subscribers_and_failed_ones_do_not() {
        let db = crm();
        let mut live = db.subscribe();
        db.rename_field("clients", "mail", "email").unwrap();
        db.remove_field("clients", "email", false).unwrap_err();
        db.rename_table("clients", "customers").unwrap();
        let seen: Vec<_> = drain(&mut live)
            .into_iter()
            .map(|change| (change.kind, change.table, change.doc))
            .collect();
        assert_eq!(
            seen,
            [
                (ChangeKind::Schema, "clients".to_owned(), None),
                (ChangeKind::Schema, "customers".to_owned(), None),
            ]
        );
    }

    #[test]
    fn schema_changes_survive_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        {
            let db = AgentDb::open(&path, "key").unwrap();
            db.define_table(&TableDef::new("clients").required("name", FieldType::Text))
                .unwrap();
            db.insert("clients", json!({"name": "Acme"})).unwrap();
            db.rename_field("clients", "name", "title").unwrap();
        }
        let db = AgentDb::open(&path, "key").unwrap();
        assert_eq!(db.get("clients", 1).unwrap().fields["title"], "Acme");
    }

    #[test]
    fn changing_a_type_converts_every_value() {
        let db = crm();
        db.add_field("clients", Field::new("budget", FieldType::Text, false))
            .unwrap();
        db.update("clients", 1, json!({"budget": "1500"}), None)
            .unwrap();
        db.update("clients", 2, json!({"budget": " 20.5 "}), None)
            .unwrap();
        db.change_field_type("clients", "budget", FieldType::Number)
            .unwrap();
        assert_eq!(db.get("clients", 1).unwrap().fields["budget"], 1500);
        assert_eq!(db.get("clients", 2).unwrap().fields["budget"], 20.5);
        let rich = db
            .find(&Query::table("clients").filter("budget", Op::Gt, 100))
            .unwrap();
        assert_eq!(rich.total, 1);

        db.change_field_type("clients", "vip", FieldType::Text)
            .unwrap();
        assert_eq!(db.get("clients", 1).unwrap().fields["vip"], "true");
        db.change_field_type("clients", "vip", FieldType::Bool)
            .unwrap();
        assert_eq!(db.get("clients", 1).unwrap().fields["vip"], true);
    }

    #[test]
    fn a_type_change_that_does_not_fit_changes_nothing() {
        let db = crm();
        assert_eq!(
            message(db.change_field_type("clients", "name", FieldType::Number)),
            "cannot change `name` on `clients` to number: 2 document(s) hold a value that does not fit, for example id 1 with \"Acme\". Fix those values first; nothing was changed."
        );
        assert_eq!(db.get("clients", 1).unwrap().fields["name"], "Acme");
        db.insert("clients", json!({"name": "Still text"})).unwrap();

        let to_missing_company = FieldType::Ref {
            table: "companies".to_owned(),
        };
        db.add_field("clients", Field::new("parent", FieldType::Number, false))
            .unwrap();
        db.update("clients", 1, json!({"parent": 1}), None).unwrap();
        db.update("clients", 2, json!({"parent": 77}), None)
            .unwrap();
        assert!(matches!(
            db.change_field_type("clients", "parent", to_missing_company.clone()),
            Err(DbError::CannotConvert {
                count: 1,
                example_id: 2,
                ..
            })
        ));
        db.update("clients", 2, json!({"parent": null}), None)
            .unwrap();
        db.change_field_type("clients", "parent", to_missing_company)
            .unwrap();
        assert!(matches!(
            db.delete("companies", 1, None),
            Err(DbError::StillReferenced { .. })
        ));
    }

    /// More documents than a schema change rewrites at a time, so the change
    /// has to carry on from where the last chunk ended.
    fn readings() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(&TableDef::new("readings").optional("level", FieldType::Text))
            .unwrap();
        let writes: Vec<Write> = (1..=1201)
            .map(|id| {
                let level = match id {
                    7 => json!(null),
                    1100 => json!("n/a"),
                    1150 => json!("?"),
                    id => json!(id.to_string()),
                };
                Write::Insert {
                    table: "readings".to_owned(),
                    doc: json!({"level": level}),
                }
            })
            .collect();
        for batch in writes.chunks(500) {
            db.batch(batch.to_vec()).unwrap();
        }
        db
    }

    fn all_readings(db: &AgentDb) -> Vec<Doc> {
        (0..3)
            .flat_map(|page| {
                let query = Query::table("readings").limit(500).offset(page * 500);
                db.find(&query).unwrap().docs
            })
            .collect()
    }

    #[test]
    fn renaming_and_removing_a_field_reach_every_document_of_a_large_table() {
        let db = readings();
        db.rename_field("readings", "level", "depth").unwrap();
        let renamed = all_readings(&db);
        assert_eq!(renamed.len(), 1201);
        for doc in &renamed {
            let expected = match doc.id {
                7 => json!({}),
                1100 => json!({"depth": "n/a"}),
                1150 => json!({"depth": "?"}),
                id => json!({"depth": id.to_string()}),
            };
            assert_eq!(json!(doc.fields), expected);
        }

        db.remove_field("readings", "depth", true).unwrap();
        let emptied = all_readings(&db);
        assert_eq!(emptied.len(), 1201);
        assert!(emptied.iter().all(|doc| doc.fields.is_empty()));
    }

    #[test]
    fn a_type_change_converts_a_large_table_or_none_of_it() {
        let db = readings();
        db.freeze_time(Some(MIGRATED_AT)).unwrap();
        assert_eq!(
            message(db.change_field_type("readings", "level", FieldType::Number)),
            "cannot change `level` on `readings` to number: 2 document(s) hold a value that does not fit, for example id 1100 with \"n/a\". Fix those values first; nothing was changed."
        );
        for id in [1, 500, 501, 1201] {
            assert_eq!(
                db.get("readings", id).unwrap().fields["level"],
                id.to_string()
            );
        }

        db.update("readings", 1100, json!({"level": "1100"}), None)
            .unwrap();
        db.update("readings", 1150, json!({"level": "11.5"}), None)
            .unwrap();
        let before = db.get("readings", 501).unwrap();
        db.change_field_type("readings", "level", FieldType::Number)
            .unwrap();
        for id in [1, 500, 501, 1000, 1001, 1100, 1201] {
            assert_eq!(db.get("readings", id).unwrap().fields["level"], json!(id));
        }
        assert_eq!(db.get("readings", 1150).unwrap().fields["level"], 11.5);
        assert_eq!(db.get("readings", 7).unwrap().fields.get("level"), None);
        let after = db.get("readings", 501).unwrap();
        assert_eq!(
            (after.version, after.updated_at),
            (before.version, before.updated_at)
        );
    }

    #[test]
    fn enum_values_can_be_removed_once_unused() {
        let db = crm();
        db.add_enum_value("clients", "status", "active").unwrap();
        db.update("clients", 1, json!({"status": "lead"}), None)
            .unwrap();
        assert_eq!(
            message(db.remove_enum_value("clients", "status", "lead")),
            "cannot remove `lead` from `status` on `clients`: 1 document(s) still use it. Update them to another value first."
        );
        assert_eq!(
            message(db.remove_enum_value("clients", "status", "won")),
            "`won` is not an allowed value of `status` on `clients`. Allowed values: lead, active."
        );
        db.update("clients", 1, json!({"status": "active"}), None)
            .unwrap();
        db.remove_enum_value("clients", "status", "lead").unwrap();
        db.insert("clients", json!({"name": "New", "status": "lead"}))
            .unwrap_err();
        assert!(matches!(
            db.remove_enum_value("clients", "status", "active"),
            Err(DbError::EmptyEnum { .. })
        ));
    }

    #[test]
    fn descriptions_are_stored_and_shown() {
        let db = crm();
        db.set_description("clients", None, "Businesses we sell to")
            .unwrap();
        db.set_description("clients", Some("vip"), "Gets priority support")
            .unwrap();
        let tables = db.describe().unwrap();
        let clients = tables.iter().find(|table| table.name == "clients").unwrap();
        assert_eq!(
            clients.description.as_deref(),
            Some("Businesses we sell to")
        );
        let vip = clients
            .fields
            .iter()
            .find(|field| field.name == "vip")
            .unwrap();
        assert_eq!(vip.description.as_deref(), Some("Gets priority support"));
        db.set_description("clients", None, "").unwrap();
        let tables = db.describe().unwrap();
        assert_eq!(
            tables
                .iter()
                .find(|table| table.name == "clients")
                .unwrap()
                .description,
            None
        );

        let built = TableDef::new("deals")
            .described("Sales in progress")
            .with(Field::new("title", FieldType::Text, true).described("Short name of the deal"));
        db.define_table(&built).unwrap();
        let json = serde_json::to_value(db.describe().unwrap()).unwrap();
        let deals = json
            .as_array()
            .unwrap()
            .iter()
            .find(|table| table["name"] == "deals")
            .unwrap();
        assert_eq!(deals["description"], "Sales in progress");
        assert_eq!(deals["fields"][0]["description"], "Short name of the deal");
    }

    #[test]
    fn a_batch_applies_fully_or_not_at_all() {
        let db = crm();
        let mut live = db.subscribe();
        let rename = |field: &str, new_name: &str| SchemaChange::RenameField {
            table: "clients".to_owned(),
            field: field.to_owned(),
            new_name: new_name.to_owned(),
        };
        let failing = [
            rename("mail", "email"),
            SchemaChange::AddField {
                table: "clients".to_owned(),
                field: Field::new("phone", FieldType::Text, false),
            },
            SchemaChange::DropTable {
                table: "companies".to_owned(),
                force: true,
            },
        ];
        assert_eq!(
            message(db.migrate(&failing)),
            "step 3 of 3 failed, so none of the 3 changes were applied: cannot drop table `companies`: field `company` on table `clients` links to it. Remove that field first."
        );
        let acme = db.get("clients", 1).unwrap();
        assert_eq!(acme.fields["mail"], "a@acme.io");
        assert_eq!(acme.fields.get("email"), None);
        db.insert("clients", json!({"name": "New", "phone": "555"}))
            .unwrap_err();
        assert_eq!(drain(&mut live), []);

        let working = [
            rename("mail", "email"),
            SchemaChange::RemoveField {
                table: "clients".to_owned(),
                field: "company".to_owned(),
                force: true,
            },
            SchemaChange::DropTable {
                table: "companies".to_owned(),
                force: true,
            },
        ];
        db.migrate(&working).unwrap();
        assert_eq!(db.get("clients", 1).unwrap().fields["email"], "a@acme.io");
        assert!(matches!(
            db.get("companies", 1),
            Err(DbError::UnknownTable { .. })
        ));
        assert_eq!(
            drain(&mut live)
                .iter()
                .filter(|change| change.kind == ChangeKind::Schema)
                .count(),
            3
        );
    }

    #[test]
    fn a_batch_can_be_written_as_json() {
        let db = crm();
        let changes: Vec<SchemaChange> = serde_json::from_value(json!([
            {"op": "define_table", "table": {
                "name": "deals",
                "description": "Sales in progress",
                "fields": [
                    {"name": "title", "type": "text", "required": true},
                    {"name": "client", "type": "ref", "table": "clients", "required": true},
                    {"name": "stage", "type": "enum", "values": ["open", "won"], "required": false}
                ]
            }},
            {"op": "rename_field", "table": "clients", "field": "mail", "new_name": "email"},
            {"op": "change_type", "table": "clients", "field": "vip", "to": {"type": "text"}},
            {"op": "describe", "table": "clients", "field": "email", "description": "Main contact address"},
            {"op": "add_enum_value", "table": "deals", "field": "stage", "value": "lost"},
            {"op": "remove_field", "table": "clients", "field": "status"}
        ]))
        .unwrap();
        db.migrate(&changes).unwrap();
        let deal = db
            .insert(
                "deals",
                json!({"title": "Big one", "client": 1, "stage": "lost"}),
            )
            .unwrap();
        assert_eq!(deal.id, 1);
        assert_eq!(db.get("clients", 1).unwrap().fields["vip"], "true");
    }
}
