#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use agentdb::{
        AgentDb, ChangeKind, DbError, Field, FieldType, Op, Query, SchemaChange, TableDef,
    };
    use serde_json::json;

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
            db.delete("accounts", 1),
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
    fn schema_changes_reach_subscribers_and_failed_ones_do_not() {
        let db = crm();
        let live = db.subscribe().unwrap();
        db.rename_field("clients", "mail", "email").unwrap();
        db.remove_field("clients", "email", false).unwrap_err();
        db.rename_table("clients", "customers").unwrap();
        let seen: Vec<_> = live
            .try_iter()
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
            db.delete("companies", 1),
            Err(DbError::StillReferenced { .. })
        ));
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
        let live = db.subscribe().unwrap();
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
        assert_eq!(live.try_iter().count(), 0);

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
            live.try_iter()
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
