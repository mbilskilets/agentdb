//! The SQLite indexes that let a query find documents without reading the
//! whole table.
//!
//! Every table's documents live in the one `docs` table, so an indexed field
//! gets an index over that table's rows only (`WHERE tbl = '...'`). SQLite
//! uses such an index only when the query repeats that condition as text,
//! not as a bound value, and only reliably when told which index to use.
//! That is why this module writes table and field names into SQL. The names
//! always come from a stored [`TableDef`], which holds nothing but names
//! that passed `check_name`: lowercase letters, digits and underscores.

use rusqlite::Connection;

use crate::db::all_defs;
use crate::error::Result;
use crate::schema::{Field, TableDef};

pub(crate) const BY_ID: &str = "sqlite_autoindex_docs_1";
pub(crate) const BY_CREATED_AT: &str = "docs_created_at";
pub(crate) const BY_UPDATED_AT: &str = "docs_updated_at";
const FIELD_INDEXES: &str = "field.*";

/// The name of the index over one field of one table.
pub(crate) fn of_field(def: &TableDef, field: &Field) -> String {
    format!("field.{}.{}", def.name, field.name)
}

/// The SQL expression for a document's value of `field`.
pub(crate) fn value_of(field: &str) -> String {
    format!("json_extract(body, '$.{field}')")
}

/// The SQL condition that selects the documents of one table.
pub(crate) fn rows_of(def: &TableDef) -> String {
    format!("tbl = '{}'", def.name)
}

/// The `docs` table, read through the index called `index`.
pub(crate) fn docs_by(index: &str) -> String {
    format!("docs INDEXED BY \"{index}\"")
}

/// Counts the documents of `def` whose `field` holds the bound value.
pub(crate) fn count_holders_sql(def: &TableDef, field: &Field) -> String {
    format!(
        "SELECT count(*) FROM {} WHERE {} AND {} = ?1",
        docs_by(&of_field(def, field)),
        rows_of(def),
        value_of(&field.name)
    )
}

/// Finds a document of `def`, other than the one with the bound id, whose
/// `field` holds the bound value.
pub(crate) fn other_holder_sql(def: &TableDef, field: &Field) -> String {
    format!(
        "SELECT id FROM {} WHERE {} AND {} = ?1 AND id <> ?2 LIMIT 1",
        docs_by(&of_field(def, field)),
        rows_of(def),
        value_of(&field.name)
    )
}

/// Makes the SQLite indexes match the schema: one for every indexed field
/// and none left over from a field or table that changed.
pub(crate) fn sync(conn: &Connection) -> Result<()> {
    let mut wanted = Vec::new();
    for def in all_defs(conn)? {
        for field in def.fields.iter().filter(|field| field.indexed) {
            let name = of_field(&def, field);
            conn.execute(
                &format!(
                    "CREATE INDEX IF NOT EXISTS \"{name}\" ON docs ({}, id) WHERE {}",
                    value_of(&field.name),
                    rows_of(&def)
                ),
                [],
            )?;
            wanted.push(name);
        }
    }
    let existing: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name GLOB ?1")?
        .query_map([FIELD_INDEXES], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for name in existing.iter().filter(|name| !wanted.contains(name)) {
        conn.execute(&format!("DROP INDEX \"{name}\""), [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::types::Value as SqlValue;
    use rusqlite::{Connection, params_from_iter};
    use serde_json::Value;

    use super::{count_holders_sql, other_holder_sql};
    use crate::db::load_def;
    use crate::{AgentDb, Field, FieldType, Op, Query, TableDef};

    fn crm() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(&TableDef::new("companies").required("name", FieldType::Text))
            .unwrap();
        db.define_table(
            &TableDef::new("clients")
                .with(Field::new("email", FieldType::Text, false).unique())
                .with(Field::new("status", FieldType::Text, false).indexed())
                .with(Field::new("revenue", FieldType::Number, false).indexed())
                .optional("notes", FieldType::Text)
                .optional(
                    "company",
                    FieldType::Ref {
                        table: "companies".to_owned(),
                    },
                ),
        )
        .unwrap();
        db
    }

    /// The steps SQLite takes to run `sql`.
    fn plan(conn: &Connection, sql: &str, values: &[SqlValue]) -> Vec<String> {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params_from_iter(values), |row| row.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The steps SQLite takes to count and to fetch a page of `query` when
    /// the table is too large to scan.
    fn plans(db: &AgentDb, query: &Query) -> (Vec<String>, Vec<String>) {
        db.read(|conn| {
            let def = load_def(conn, &query.table)?;
            let compiled = query.compile(&def, i64::MAX)?;
            let count = plan(conn, &compiled.count_sql(), &compiled.params);
            let mut values = compiled.params.clone();
            values.extend([SqlValue::Integer(50), SqlValue::Integer(0)]);
            Ok((count, plan(conn, &compiled.page_sql(), &values)))
        })
        .unwrap()
    }

    fn clients() -> Query {
        Query::table("clients")
    }

    #[test]
    fn an_equality_filter_searches_the_fields_index() {
        let (count, page) = plans(&crm(), &clients().filter("status", Op::Eq, "lead"));
        assert_eq!(
            count,
            ["SEARCH docs USING INDEX field.clients.status (<expr>=?)"]
        );
        assert_eq!(
            page,
            ["SEARCH docs USING INDEX field.clients.status (<expr>=?)"]
        );
    }

    #[test]
    fn a_range_filter_searches_the_fields_index() {
        let query = clients()
            .filter("revenue", Op::Gt, 100)
            .filter("notes", Op::Contains, "vip")
            .sort("revenue", true);
        let (count, page) = plans(&crm(), &query);
        assert_eq!(
            count,
            ["SEARCH docs USING INDEX field.clients.revenue (<expr>>?)"]
        );
        assert_eq!(
            page,
            ["SEARCH docs USING INDEX field.clients.revenue (<expr>>?)"]
        );
    }

    #[test]
    fn a_sort_walks_the_fields_index_without_sorting() {
        for descending in [false, true] {
            let (_, page) = plans(&crm(), &clients().sort("revenue", descending));
            assert_eq!(page, ["SCAN docs USING INDEX field.clients.revenue"]);
        }
    }

    #[test]
    fn system_fields_use_their_own_indexes() {
        let db = crm();
        let (_, by_id) = plans(&db, &clients().filter("id", Op::Gte, 7));
        assert_eq!(
            by_id,
            ["SEARCH docs USING INDEX sqlite_autoindex_docs_1 (tbl=? AND id>?)"]
        );
        let (_, unsorted) = plans(&db, &clients());
        assert_eq!(
            unsorted,
            ["SEARCH docs USING INDEX sqlite_autoindex_docs_1 (tbl=?)"]
        );
        let (_, newest) = plans(&db, &clients().sort("created_at", true));
        assert_eq!(newest, ["SEARCH docs USING INDEX docs_created_at (tbl=?)"]);
        let (_, changed) = plans(&db, &clients().filter("updated_at", Op::Lt, "2026-10-04"));
        assert_eq!(
            changed[0],
            "SEARCH docs USING INDEX docs_updated_at (tbl=? AND updated_at<?)"
        );
    }

    #[test]
    fn the_filter_that_narrows_most_picks_the_index() {
        let query = clients()
            .filter("revenue", Op::Gt, 100)
            .filter("status", Op::Eq, "lead")
            .filter("email", Op::Eq, "a@acme.io");
        let (_, page) = plans(&crm(), &query);
        assert_eq!(
            page,
            ["SEARCH docs USING INDEX field.clients.email (<expr>=?)"]
        );

        let without_email =
            clients()
                .filter("email", Op::Eq, Value::Null)
                .filter("status", Op::Eq, "lead");
        let (_, page) = plans(&crm(), &without_email);
        assert_eq!(
            page,
            ["SEARCH docs USING INDEX field.clients.status (<expr>=?)"]
        );
    }

    #[test]
    fn the_reference_and_unique_checks_search_an_index() {
        let db = crm();
        db.read(|conn| {
            let def = load_def(conn, "clients")?;
            let field = |name| def.field(name).unwrap();
            assert_eq!(
                plan(
                    conn,
                    &count_holders_sql(&def, field("company")),
                    &[SqlValue::Integer(1)]
                ),
                ["SEARCH docs USING INDEX field.clients.company (<expr>=?)"]
            );
            assert_eq!(
                plan(
                    conn,
                    &other_holder_sql(&def, field("email")),
                    &[SqlValue::Text("a@acme.io".to_owned()), SqlValue::Integer(1)]
                ),
                ["SEARCH docs USING INDEX field.clients.email (<expr>=?)"]
            );
            Ok(())
        })
        .unwrap();
    }

    fn field_indexes(db: &AgentDb) -> Vec<String> {
        db.read(|conn| {
            Ok(conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")?
                .query_map([], |row| row.get(0))?
                .map(Result::unwrap)
                .filter(|name: &String| name.starts_with("field."))
                .collect())
        })
        .unwrap()
    }

    #[test]
    fn indexes_follow_every_schema_change() {
        let db = crm();
        assert_eq!(
            field_indexes(&db),
            [
                "field.clients.company",
                "field.clients.email",
                "field.clients.revenue",
                "field.clients.status"
            ]
        );
        db.rename_field("clients", "status", "stage").unwrap();
        db.set_indexed("clients", "revenue", false).unwrap();
        db.set_indexed("clients", "notes", true).unwrap();
        db.remove_field("clients", "email", false).unwrap();
        db.rename_table("clients", "customers").unwrap();
        assert_eq!(
            field_indexes(&db),
            [
                "field.customers.company",
                "field.customers.notes",
                "field.customers.stage"
            ]
        );
        db.change_field_type("customers", "company", FieldType::Number)
            .unwrap();
        db.set_indexed("customers", "company", false).unwrap();
        db.change_field_type(
            "customers",
            "revenue",
            FieldType::Ref {
                table: "companies".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            field_indexes(&db),
            [
                "field.customers.notes",
                "field.customers.revenue",
                "field.customers.stage"
            ]
        );
        db.drop_table("customers", false).unwrap();
        assert_eq!(field_indexes(&db), Vec::<String>::new());
    }
}
