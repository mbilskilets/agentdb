#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agentdb::{AgentDb, Answer, FieldType, Judge, Judgement, Op, Query, Question, TableDef};
    use serde_json::json;

    /// Answers every request with the same canned answers, so `ask()` can be
    /// tested without the network. Questions with no canned answer count as
    /// "no" or "none".
    struct Canned(BTreeMap<String, Answer>);

    impl Judge for Canned {
        fn judge(
            &self,
            _state: &str,
            _questions: &BTreeMap<String, Question>,
        ) -> agentdb::Result<Judgement> {
            Ok(Judgement {
                answers: self.0.clone(),
                ..Judgement::default()
            })
        }
    }

    fn pick(id: &str, choice: &str) -> (String, Answer) {
        let answer = Answer::Choice {
            choice: choice.to_owned(),
            confidence: 0.95,
        };
        (id.to_owned(), answer)
    }

    fn noul(id: &str, probability: f64) -> (String, Answer) {
        (id.to_owned(), Answer::Noul { noul: probability })
    }

    fn canned<const N: usize>(answers: [(String, Answer); N]) -> Canned {
        Canned(BTreeMap::from(answers))
    }

    fn crm() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("clients")
                .required("name", FieldType::Text)
                .optional(
                    "status",
                    FieldType::Enum {
                        values: vec!["lead".to_owned(), "active".to_owned()],
                    },
                )
                .optional("revenue", FieldType::Number)
                .optional("signed_at", FieldType::Datetime),
        )
        .unwrap();
        db.freeze_time(Some("2026-10-02T10:00:00Z")).unwrap();
        db.insert(
            "clients",
            json!({"name": "Old", "status": "lead", "revenue": 10}),
        )
        .unwrap();
        db.freeze_time(Some("2026-10-03T12:00:00Z")).unwrap();
        db.insert(
            "clients",
            json!({"name": "Acme", "status": "lead", "revenue": 900}),
        )
        .unwrap();
        db.insert(
            "clients",
            json!({"name": "Globex", "status": "active", "revenue": 5000}),
        )
        .unwrap();
        db
    }

    #[test]
    fn picks_become_a_query_that_runs() {
        let judge = canned([
            pick("table", "clients"),
            pick("clients.status", "lead"),
            pick("clients.num.0", "revenue"),
            pick("num.0.op", "more_than"),
            pick("period", "today"),
            pick("clients.date_field", "created_at"),
        ]);
        let asked = crm()
            .ask(&judge, "leads created today with revenue over 100")
            .unwrap();
        let expected = Query::table("clients")
            .filter("status", Op::Eq, "lead")
            .filter("revenue", Op::Gt, 100)
            .filter("created_at", Op::Gte, "2026-10-03")
            .filter("created_at", Op::Lt, "2026-10-04");
        assert_eq!(asked.query, Some(expected));
        assert_eq!(asked.refusal, None);
        let names: Vec<_> = asked
            .page
            .unwrap()
            .docs
            .iter()
            .map(|doc| doc.fields["name"].clone())
            .collect();
        assert_eq!(names, [json!("Acme")]);
    }

    #[test]
    fn excluded_values_sorting_and_limits_are_understood() {
        let judge = canned([
            pick("table", "clients"),
            pick("clients.status", "not_lead"),
            pick("clients.num.0", "limit"),
            noul("sort", 0.97),
            pick("clients.sort.field", "revenue"),
            pick("sort.dir", "descending"),
            pick("clients.missing", "signed_at"),
        ]);
        let asked = crm()
            .ask(&judge, "top 2 unsigned clients that are not leads")
            .unwrap();
        let expected = Query::table("clients")
            .filter("status", Op::Ne, "lead")
            .filter("signed_at", Op::Eq, json!(null))
            .sort("revenue", true)
            .limit(2);
        assert_eq!(asked.query, Some(expected));
        assert_eq!(asked.page.unwrap().total, 1);
    }

    #[test]
    fn requests_to_change_data_are_refused() {
        let judge = canned([pick("table", "clients"), noul("write", 0.98)]);
        let asked = crm().ask(&judge, "delete all leads").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.query, None);
        assert!(asked.refusal.unwrap().starts_with("ask() only reads."));
    }

    #[test]
    fn requests_about_no_table_are_refused() {
        let asked = crm()
            .ask(&canned([pick("table", "none")]), "tell me a joke")
            .unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(
            asked.refusal.as_deref(),
            Some("the request does not match any table. Tables: clients.")
        );
    }

    #[test]
    fn an_unsure_answer_returns_the_guess_without_running_it() {
        let unsure = Answer::Choice {
            choice: "lead".to_owned(),
            confidence: 0.4,
        };
        let judge = canned([
            pick("table", "clients"),
            ("clients.status".to_owned(), unsure),
        ]);
        let asked = crm().ask(&judge, "leads").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(
            asked.query,
            Some(Query::table("clients").filter("status", Op::Eq, "lead"))
        );
        assert!(
            asked
                .refusal
                .unwrap()
                .starts_with("not confident enough (0.40)")
        );
    }

    #[test]
    fn a_dropped_condition_is_caught_by_the_second_check() {
        let judge = canned([pick("table", "clients"), noul("leftover", 0.9)]);
        let asked = crm().ask(&judge, "clients in Poland").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.query, Some(Query::table("clients")));
        assert_eq!(
            asked.refusal.as_deref(),
            Some(
                "the request seems to ask for something the query leaves out. It was understood as: all clients. Check the attached query, and use find() if a condition is missing."
            )
        );
    }
}
