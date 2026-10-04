#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::path::Path;

    use agentdb::{
        AgentDb, Change, ChangeKind, DbError, Field, FieldType, Op, Query, TableDef, Write,
    };
    use serde_json::{Value, json};
    use tokio::sync::broadcast::Receiver;
    use tokio::sync::broadcast::error::TryRecvError;

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

    fn drain(live: &mut Receiver<Change>) -> Vec<Change> {
        std::iter::from_fn(|| live.try_recv().ok()).collect()
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
            message(db.delete("companies", company.id, None)),
            "cannot delete `companies` id 1: 1 document(s) in `clients` point to it through `company`. Update or delete those first."
        );
        db.delete("clients", client.id, None).unwrap();
        db.delete("companies", company.id, None).unwrap();
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
            "`clients` id 1 is at version 2, but this write expected version 1. Someone else changed it: read it again with get() and retry with version 2."
        );
    }

    #[test]
    fn delete_refuses_a_stale_version() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        db.update("clients", 1, json!({"revenue": 1}), None)
            .unwrap();
        assert_eq!(
            message(db.delete("clients", 1, Some(1))),
            "`clients` id 1 is at version 2, but this write expected version 1. Someone else changed it: read it again with get() and retry with version 2."
        );
        assert_eq!(db.get("clients", 1).unwrap().version, 2);
        db.delete("clients", 1, Some(2)).unwrap();
        assert!(matches!(
            db.get("clients", 1),
            Err(DbError::NotFound { .. })
        ));
    }

    #[test]
    fn a_batch_applies_every_write_in_order() {
        let db = crm();
        let mut live = db.subscribe();
        let writes: Vec<Write> = serde_json::from_value(json!([
            {"op": "insert", "table": "companies", "doc": {"name": "Acme Corp"}},
            {"op": "insert", "table": "clients", "doc": {"name": "Ann", "company": 1}},
            {"op": "update", "table": "clients", "id": 1, "patch": {"revenue": 5}, "version": 1},
            {"op": "insert", "table": "clients", "doc": {"name": "Bob"}},
            {"op": "delete", "table": "clients", "id": 2},
        ]))
        .unwrap();
        let docs = db.batch(writes).unwrap();
        let summary: Vec<_> = docs
            .iter()
            .map(|doc| (doc.id, doc.version, doc.fields["name"].clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (1, 1, json!("Acme Corp")),
                (1, 1, json!("Ann")),
                (1, 2, json!("Ann")),
                (2, 1, json!("Bob")),
                (2, 1, json!("Bob")),
            ]
        );
        assert_eq!(docs[2].fields["revenue"], 5);
        assert_eq!(db.get("clients", 1).unwrap(), docs[2]);
        assert!(matches!(
            db.get("clients", 2),
            Err(DbError::NotFound { .. })
        ));

        let published = drain(&mut live);
        let kinds: Vec<_> = published.iter().map(|change| change.kind).collect();
        assert_eq!(
            kinds,
            [
                ChangeKind::Insert,
                ChangeKind::Insert,
                ChangeKind::Update,
                ChangeKind::Insert,
                ChangeKind::Delete
            ]
        );
        let published_docs: Vec<_> = published.into_iter().filter_map(|c| c.doc).collect();
        assert_eq!(published_docs, docs);
    }

    #[test]
    fn a_batch_with_a_failing_write_changes_nothing() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        let before = db.latest_seq().unwrap();
        let mut live = db.subscribe();
        let insert = |doc| Write::Insert {
            table: "clients".to_owned(),
            doc,
        };
        let writes = vec![
            insert(json!({"name": "Globex"})),
            Write::Update {
                table: "clients".to_owned(),
                id: 1,
                patch: json!({"revenue": 9}),
                version: None,
            },
            insert(json!({"name": "Initech", "company": 77})),
        ];
        assert_eq!(
            message(db.batch(writes)),
            "step 3 of 3 failed, so none of the 3 changes were applied: field `company` points to `companies` id 77, which does not exist. Insert that `companies` document first or use an existing id."
        );
        assert_eq!(db.find(&Query::table("clients")).unwrap().total, 1);
        assert_eq!(db.get("clients", 1).unwrap().version, 1);
        assert_eq!(db.latest_seq().unwrap(), before);
        assert_eq!(drain(&mut live), []);
        assert_eq!(
            db.insert("clients", json!({"name": "Hooli"})).unwrap().id,
            2
        );
    }

    #[test]
    fn a_batch_is_limited_to_500_writes() {
        let db = crm();
        let writes = |count: usize| {
            vec![
                Write::Insert {
                    table: "clients".to_owned(),
                    doc: json!({"name": "Acme"}),
                };
                count
            ]
        };
        assert_eq!(
            message(db.batch(writes(501))),
            "a batch takes at most 500 writes, and this one has 501. Split it into batches of 500 or fewer and send them one after another."
        );
        assert_eq!(db.find(&Query::table("clients")).unwrap().total, 0);
        assert_eq!(db.batch(writes(500)).unwrap().len(), 500);
        assert_eq!(db.batch(Vec::new()).unwrap(), []);
    }

    #[test]
    fn ids_are_not_reused_after_delete() {
        let db = crm();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        db.delete("clients", 1, None).unwrap();
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
    fn a_descending_sort_breaks_ties_by_descending_id() {
        let db = crm();
        for (name, revenue) in [("Acme", 5), ("Globex", 9), ("Initech", 5), ("Hooli", 5)] {
            db.insert("clients", json!({"name": name, "revenue": revenue}))
                .unwrap();
        }
        assert_eq!(
            names(&db, &Query::table("clients").sort("revenue", false)),
            ["Acme", "Initech", "Hooli", "Globex"]
        );
        assert_eq!(
            names(&db, &Query::table("clients").sort("revenue", true)),
            ["Globex", "Hooli", "Initech", "Acme"]
        );
        assert_eq!(
            names(&db, &Query::table("clients").sort("id", true).limit(2)),
            ["Hooli", "Initech"]
        );
    }

    fn insert_many(db: &AgentDb, table: &str, docs: impl Iterator<Item = Value>) {
        let writes: Vec<Write> = docs
            .map(|doc| Write::Insert {
                table: table.to_owned(),
                doc,
            })
            .collect();
        for batch in writes.chunks(500) {
            db.batch(batch.to_vec()).unwrap();
        }
    }

    fn event(n: i64) -> Value {
        let kind = if n % 2 == 0 { "click" } else { "view" };
        json!({"kind": kind, "score": n, "note": format!("note {n}"), "weight": n % 7})
    }

    fn events(count: i64) -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("events")
                .with(Field::new("kind", FieldType::Text, true).indexed())
                .with(Field::new("score", FieldType::Number, true).indexed())
                .required("note", FieldType::Text)
                .required("weight", FieldType::Number),
        )
        .unwrap();
        insert_many(&db, "events", (0..count).map(event));
        db
    }

    fn total(db: &AgentDb, query: &Query) -> i64 {
        db.find(query).unwrap().total
    }

    #[test]
    fn a_table_of_up_to_1000_documents_answers_any_query() {
        let db = events(1000);
        let all = || Query::table("events");
        assert_eq!(total(&db, &all().filter("weight", Op::Eq, 3)), 143);
        assert_eq!(
            total(&db, &all().filter("note", Op::Contains, "NOTE 99")),
            11
        );
        assert_eq!(total(&db, &all().filter("kind", Op::Ne, "click")), 500);
        let lightest = db.find(&all().sort("weight", false).limit(2)).unwrap();
        let ids: Vec<i64> = lightest.docs.iter().map(|doc| doc.id).collect();
        assert_eq!(ids, [1, 8]);
    }

    const TOO_LARGE: &str = "cannot run this query: `events` holds 1001 documents, and a table with more than 1000 is only searched through an index.";
    const INDEXED: &str = "Indexed fields: id, created_at, updated_at, kind, score. Add a filter on one of them that leaves few documents to read (an `eq`, or a narrow `gt`, `gte`, `lt` or `lte` range)";
    const INDEX_WEIGHT: &str = r#", or index `weight` first with the schema change {"op": "set_indexed", "table": "events", "field": "weight", "indexed": true}."#;

    #[test]
    fn a_larger_table_refuses_a_query_no_index_can_answer() {
        let db = events(1001);
        let all = || Query::table("events");
        assert_eq!(
            message(db.find(&all().filter("weight", Op::Eq, 3))),
            format!(
                "{TOO_LARGE} The filter on `weight` has no index to use. {INDEXED}{INDEX_WEIGHT}"
            )
        );
        assert_eq!(
            message(db.find(&all().sort("weight", true))),
            format!(
                "{TOO_LARGE} The sort on `weight` has no index to use. {INDEXED}{INDEX_WEIGHT}"
            )
        );
        assert_eq!(
            message(db.find(&all().filter("note", Op::Contains, "note 99"))),
            format!(
                "{TOO_LARGE} The `contains` filter on `note` cannot use an index (`ne` and `contains` never can). {INDEXED}."
            )
        );
        assert_eq!(
            message(
                db.find(
                    &all()
                        .filter("kind", Op::Ne, "click")
                        .filter("weight", Op::Lt, 2)
                )
            ),
            format!(
                "{TOO_LARGE} The filter on `weight` has no index to use. {INDEXED}{INDEX_WEIGHT}"
            )
        );
        let unindexed_system_field = db.find(&all().filter("version", Op::Eq, 1));
        assert!(matches!(
            unindexed_system_field,
            Err(DbError::QueryNeedsIndex {
                could_index: None,
                ..
            })
        ));
        assert_eq!(
            unindexed_system_field.unwrap_err().code(),
            "query_needs_index"
        );

        db.set_indexed("events", "weight", true).unwrap();
        assert_eq!(total(&db, &all().filter("weight", Op::Eq, 3)), 143);
        let heaviest = db.find(&all().sort("weight", true).limit(2)).unwrap();
        let ids: Vec<i64> = heaviest.docs.iter().map(|doc| doc.id).collect();
        assert_eq!(ids, [1001, 994]);
    }

    #[test]
    fn a_larger_table_answers_queries_an_index_can_narrow() {
        let db = events(1001);
        let all = || Query::table("events");
        assert_eq!(total(&db, &all()), 1001);
        assert_eq!(db.find(&all()).unwrap().next_offset, Some(50));
        assert_eq!(total(&db, &all().filter("kind", Op::Eq, "click")), 501);
        assert_eq!(total(&db, &all().filter("score", Op::Gte, 990)), 11);
        assert_eq!(total(&db, &all().filter("id", Op::Gt, 1000)), 1);
        assert_eq!(
            total(&db, &all().filter("created_at", Op::Gte, "2020-01-01")),
            1001
        );
        assert_eq!(
            total(
                &db,
                &all()
                    .filter("kind", Op::Ne, "click")
                    .filter("score", Op::Lt, 10)
            ),
            5
        );
        let on_top_of_an_index = all()
            .filter("kind", Op::Eq, "view")
            .filter("note", Op::Contains, "note 99")
            .filter("weight", Op::Gte, 0)
            .sort("weight", true);
        let page = db.find(&on_top_of_an_index).unwrap();
        let scores: Vec<_> = page.docs.iter().map(|doc| &doc.fields["score"]).collect();
        assert_eq!(scores, [993, 999, 991, 997, 995, 99]);
        let top = db.find(&all().sort("score", true).limit(3)).unwrap();
        let scores: Vec<_> = top.docs.iter().map(|doc| &doc.fields["score"]).collect();
        assert_eq!(scores, [1000, 999, 998]);
    }

    fn people() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("people")
                .required("name", FieldType::Text)
                .with(Field::new("email", FieldType::Text, false).unique()),
        )
        .unwrap();
        db
    }

    #[test]
    fn a_unique_field_refuses_a_second_document_with_the_same_value() {
        let db = people();
        let ann = json!({"name": "Ann", "email": "ann@acme.io"});
        db.insert("people", ann.clone()).unwrap();
        db.insert("people", json!({"name": "Bob"})).unwrap();
        db.insert("people", json!({"name": "Cy"})).unwrap();
        let duplicate = "`email` must be unique on `people`, and `people` id 1 already holds \"ann@acme.io\". Update that document instead of adding another one, or use a different value.";
        assert_eq!(message(db.insert("people", ann.clone())), duplicate);
        assert_eq!(
            message(db.update("people", 2, json!({"email": "ann@acme.io"}), None)),
            duplicate
        );
        assert_eq!(
            db.insert("people", ann.clone()).unwrap_err().code(),
            "duplicate_value"
        );
        assert_eq!(db.find(&Query::table("people")).unwrap().total, 3);

        db.update(
            "people",
            1,
            json!({"name": "Ann B", "email": "ann@acme.io"}),
            None,
        )
        .unwrap();
        db.update("people", 2, json!({"email": "Ann@acme.io"}), None)
            .unwrap();
        db.delete("people", 1, None).unwrap();
        assert_eq!(db.insert("people", ann).unwrap().id, 4);
    }

    #[test]
    fn a_batch_cannot_slip_two_equal_values_into_a_unique_field() {
        let db = people();
        let insert = |name: &str| Write::Insert {
            table: "people".to_owned(),
            doc: json!({"name": name, "email": "twin@acme.io"}),
        };
        assert_eq!(
            message(db.batch(vec![insert("Ann"), insert("Bob")])),
            "step 2 of 2 failed, so none of the 2 changes were applied: `email` must be unique on `people`, and `people` id 1 already holds \"twin@acme.io\". Update that document instead of adding another one, or use a different value."
        );
        assert_eq!(db.find(&Query::table("people")).unwrap().total, 0);
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

        let mut live = db.subscribe();
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        db.update("clients", 1, json!({"revenue": 5}), None)
            .unwrap();
        db.delete("clients", 1, None).unwrap();
        db.insert("clients", json!({"nope": 1})).unwrap_err();

        let received = drain(&mut live);
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
    fn a_subscriber_that_falls_behind_is_told_and_catches_up_from_the_log() {
        let db = crm();
        let start = db.latest_seq().unwrap();
        let mut live = db.subscribe();
        for _ in 0..3 {
            let writes = vec![
                Write::Insert {
                    table: "clients".to_owned(),
                    doc: json!({"name": "Acme"}),
                };
                500
            ];
            db.batch(writes).unwrap();
        }
        assert!(matches!(live.try_recv(), Err(TryRecvError::Lagged(_))));

        let mut caught_up = Vec::new();
        loop {
            let from = caught_up.last().map_or(start, |change: &Change| change.seq);
            let page = db.changes_since(from).unwrap();
            if page.is_empty() {
                break;
            }
            caught_up.extend(page);
        }
        assert_eq!(caught_up.len(), 1500);
        assert_eq!(caught_up.last().unwrap().seq, db.latest_seq().unwrap());
    }

    #[test]
    fn a_negative_seq_replays_the_change_log_from_the_start() {
        let db = AgentDb::open_in_memory().unwrap();
        assert_eq!(db.changes_since(-1).unwrap(), []);
        define_crm(&db);
        db.insert("clients", json!({"name": "Acme"})).unwrap();
        let from_the_start = db.changes_since(0).unwrap();
        assert_eq!(from_the_start.len(), 3);
        assert_eq!(db.changes_since(-1).unwrap(), from_the_start);
        assert_eq!(db.changes_since(i64::MIN).unwrap(), from_the_start);
    }

    #[test]
    fn the_change_log_keeps_only_the_newest_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tenant.db");
        let db = AgentDb::open(&path, "key").unwrap();
        assert_eq!(db.latest_seq().unwrap(), 0);
        assert_eq!(db.changes_since(0).unwrap(), []);
        db.define_table(&TableDef::new("notes").required("text", FieldType::Text))
            .unwrap();
        for _ in 0..21 {
            let writes = vec![
                Write::Insert {
                    table: "notes".to_owned(),
                    doc: json!({"text": "hello"}),
                };
                500
            ];
            db.batch(writes).unwrap();
        }
        assert_eq!(db.latest_seq().unwrap(), 10_501);
        assert_eq!(
            message(db.changes_since(0)),
            "cannot replay changes after seq 0: the log keeps only the newest 10000 changes, and the oldest one left is seq 502. Read the current state with find() instead, then continue from seq 10501."
        );
        assert!(matches!(
            db.changes_since(500),
            Err(DbError::ChangesTrimmed { .. })
        ));
        let oldest_kept = db.changes_since(501).unwrap();
        assert_eq!(oldest_kept.len(), 500);
        assert_eq!(oldest_kept[0].seq, 502);
        assert_eq!(db.changes_since(10_501).unwrap(), []);

        drop(db);
        let reopened = AgentDb::open(&path, "key").unwrap();
        assert_eq!(reopened.latest_seq().unwrap(), 10_501);
        assert!(matches!(
            reopened.changes_since(0),
            Err(DbError::ChangesTrimmed { .. })
        ));
        assert_eq!(reopened.changes_since(10_500).unwrap().len(), 1);
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

    /// Whether any file in `dir` holds `needle` as plain bytes.
    fn leaked(dir: &Path, needle: &[u8]) -> bool {
        std::fs::read_dir(dir).unwrap().any(|entry| {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            bytes.windows(needle.len()).any(|window| window == needle)
        })
    }

    #[test]
    fn nothing_on_disk_is_readable_while_the_database_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = AgentDb::open(dir.path().join("tenant.db"), "correct horse").unwrap();
        define_crm(&db);
        db.insert("clients", json!({"name": "Acme Secret Client"}))
            .unwrap();
        assert!(std::fs::read_dir(dir.path()).unwrap().count() > 1);
        assert!(!leaked(dir.path(), b"Acme Secret Client"));
        assert!(!leaked(dir.path(), b"clients"));
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
        assert!(!leaked(dir.path(), b"Acme Secret Client"));
        assert!(!leaked(dir.path(), b"SQLite format"));

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
