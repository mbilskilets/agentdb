#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use agentdb::{AgentDb, Answer, FieldType, Judge, Judgement, Op, Query, Question, TableDef};
    use serde_json::json;

    const NONE: &str = "none.";
    const TOO_LARGE: &str = "ask() could not fit this request and the choices the schema allows into one request to the model. Shorten the request if it is long; otherwise use find() with an explicit query.";

    type Questions = BTreeMap<String, Question>;

    /// Answers every request from the same canned answers, so `ask()` can be
    /// tested without the network. A question with no canned answer gets a
    /// sure "no" or a sure "none".
    struct Canned(BTreeMap<String, Answer>);

    impl Judge for Canned {
        fn judge(&self, _state: &str, questions: &Questions) -> agentdb::Result<Judgement> {
            let answers = questions
                .iter()
                .map(|(id, question)| {
                    let canned = self.0.get(id).cloned();
                    (id.clone(), canned.unwrap_or_else(|| sure_no(question)))
                })
                .collect();
            Ok(Judgement {
                answers,
                ..Judgement::default()
            })
        }
    }

    fn sure_no(question: &Question) -> Answer {
        match question {
            Question::Noul { .. } => Answer::Noul { noul: 0.0 },
            Question::Choice { criteria, .. } if criteria.contains_key(NONE) => sure(NONE, 1.0),
            Question::Choice { .. } => sure("none", 1.0),
        }
    }

    /// Returns only the given answers, like a model whose reply was cut
    /// short.
    struct CutShort(BTreeMap<String, Answer>);

    impl Judge for CutShort {
        fn judge(&self, _state: &str, _questions: &Questions) -> agentdb::Result<Judgement> {
            Ok(Judgement {
                answers: self.0.clone(),
                ..Judgement::default()
            })
        }
    }

    /// Keeps every request that reaches the model.
    struct Recording {
        judge: Canned,
        requests: RefCell<Vec<Questions>>,
    }

    impl Recording {
        fn new(judge: Canned) -> Self {
            Self {
                judge,
                requests: RefCell::new(Vec::new()),
            }
        }
    }

    impl Judge for Recording {
        fn judge(&self, state: &str, questions: &Questions) -> agentdb::Result<Judgement> {
            self.requests.borrow_mut().push(questions.clone());
            self.judge.judge(state, questions)
        }
    }

    fn pick(id: &str, choice: &str) -> (String, Answer) {
        (id.to_owned(), sure(choice, 0.95))
    }

    fn sure(choice: &str, confidence: f64) -> Answer {
        Answer::Choice {
            choice: choice.to_owned(),
            confidence,
        }
    }

    fn noul(id: &str, probability: f64) -> (String, Answer) {
        (id.to_owned(), Answer::Noul { noul: probability })
    }

    fn canned<const N: usize>(answers: [(String, Answer); N]) -> Canned {
        Canned(BTreeMap::from(answers))
    }

    fn values<const N: usize>(values: [&str; N]) -> FieldType {
        FieldType::Enum {
            values: values.map(str::to_owned).to_vec(),
        }
    }

    fn crm() -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("clients")
                .required("name", FieldType::Text)
                .optional("status", values(["lead", "active"]))
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

    fn names(asked: &agentdb::Asked) -> Vec<serde_json::Value> {
        let page = asked.page.as_ref().unwrap();
        page.docs
            .iter()
            .map(|doc| doc.fields["name"].clone())
            .collect()
    }

    #[test]
    fn picks_become_a_query_that_runs() {
        let judge = canned([
            pick("table", "clients"),
            pick("clients/field.status", "is lead"),
            pick("clients/number.0.role", "revenue"),
            pick("number.0.comparison", "more_than"),
            pick("period", "today"),
            pick("clients/period.field", "created_at"),
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
        assert_eq!(names(&asked), [json!("Acme")]);
    }

    #[test]
    fn excluded_values_sorting_and_limits_are_understood() {
        let judge = canned([
            pick("table", "clients"),
            pick("clients/field.status", "not lead"),
            pick("clients/number.0.role", "limit."),
            noul("sort", 0.97),
            pick("clients/sort.field", "revenue"),
            pick("sort.direction", "descending"),
            pick("clients/missing", "signed_at"),
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
    fn text_is_matched_by_what_it_contains() {
        let judge = canned([
            pick("table", "clients"),
            noul("clients/field.name", 0.9),
            pick("clients/field.name.match", "is"),
            pick("clients/field.name.value", "acme"),
        ]);
        let asked = crm().ask(&judge, "clients named acme").unwrap();
        assert_eq!(
            asked.query,
            Some(Query::table("clients").filter("name", Op::Contains, "acme"))
        );
        assert_eq!(names(&asked), [json!("Acme")]);
    }

    #[test]
    fn excluding_by_text_is_refused_because_no_query_can_say_it() {
        let judge = canned([
            pick("table", "clients"),
            noul("clients/field.name", 0.9),
            pick("clients/field.name.match", "is_not"),
            pick("clients/field.name.value", "Acme"),
        ]);
        let asked = crm().ask(&judge, "clients not named Acme").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.query, None);
        assert_eq!(
            asked.refusal.as_deref(),
            Some(
                "ask() matches text with `contains` and cannot exclude `clients` by the text of their `name`. Use find(): its `ne` operator excludes one exact value."
            )
        );
    }

    #[test]
    fn a_minus_sign_in_the_request_reaches_the_query() {
        let judge = canned([
            pick("table", "clients"),
            pick("clients/number.0.role", "revenue"),
            pick("number.0.comparison", "more_than"),
        ]);
        let asked = crm().ask(&judge, "clients with revenue above -20").unwrap();
        assert_eq!(
            asked.query,
            Some(Query::table("clients").filter("revenue", Op::Gt, -20))
        );
        assert_eq!(asked.page.unwrap().total, 3);
    }

    #[test]
    fn a_limit_that_is_not_a_whole_number_is_refused() {
        let judge = canned([
            pick("table", "clients"),
            pick("clients/number.0.role", "limit."),
        ]);
        let asked = crm().ask(&judge, "top 2.5 clients").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(
            asked.refusal.as_deref(),
            Some(
                "the request is about `clients`, but how many results \"2.5\" means could not be worked out from it. Use find() with an explicit query."
            )
        );
    }

    #[test]
    fn enum_values_that_look_like_other_options_are_told_apart() {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("tasks").optional("state", values(["started", "not_started", "none"])),
        )
        .unwrap();
        let state = |option: &str| {
            let judge = canned([pick("table", "tasks"), pick("tasks/field.state", option)]);
            db.ask(&judge, "tasks").unwrap().query.unwrap().filters
        };
        let tasks = |op: Op, value: &str| Query::table("tasks").filter("state", op, value).filters;

        assert_eq!(state("is started"), tasks(Op::Eq, "started"));
        assert_eq!(state("not started"), tasks(Op::Ne, "started"));
        assert_eq!(state("is not_started"), tasks(Op::Eq, "not_started"));
        assert_eq!(state("not not_started"), tasks(Op::Ne, "not_started"));
        assert_eq!(state("is none"), tasks(Op::Eq, "none"));
        assert_eq!(state(NONE), Vec::new());
    }

    #[test]
    fn number_fields_named_like_number_roles_are_read_as_fields() {
        let db = AgentDb::open_in_memory().unwrap();
        db.define_table(
            &TableDef::new("plans")
                .optional("limit", FieldType::Number)
                .optional("period", FieldType::Number),
        )
        .unwrap();
        let judge = canned([
            pick("table", "plans"),
            pick("plans/number.0.role", "limit."),
            pick("plans/number.1.role", "limit"),
            pick("number.1.comparison", "more_than"),
            pick("plans/number.2.role", "period"),
            pick("number.2.comparison", "is"),
        ]);
        let asked = db
            .ask(&judge, "first 2 plans with a limit over 5 and period 30")
            .unwrap();
        let expected = Query::table("plans")
            .filter("limit", Op::Gt, 5)
            .filter("period", Op::Eq, 30)
            .limit(2);
        assert_eq!(asked.query, Some(expected));
        assert_eq!(asked.refusal, None);
    }

    #[test]
    fn tables_and_fields_named_like_questions_keep_their_own_answers() {
        let db = AgentDb::open_in_memory().unwrap();
        for table in ["none", "sort", "period", "number", "field"] {
            db.define_table(
                &TableDef::new(table)
                    .optional("missing", FieldType::Bool)
                    .optional("present", FieldType::Text)
                    .optional("date_field", FieldType::Datetime)
                    .optional("sort", FieldType::Number)
                    .optional("field", FieldType::Bool)
                    .optional("none", FieldType::Bool),
            )
            .unwrap();
        }
        db.freeze_time(Some("2026-10-03T12:00:00Z")).unwrap();
        let judge = canned([
            pick("table", "none"),
            pick("none/field.missing", "yes"),
            pick("none/field.none", "no"),
            pick("none/missing", "present"),
            pick("period", "today"),
            pick("none/period.field", "date_field"),
            noul("sort", 0.97),
            pick("none/sort.field", "sort"),
            pick("sort.direction", "ascending"),
            pick("sort/field.field", "yes"),
            pick("field/sort.field", "field"),
            pick("period/period.field", "created_at"),
            pick("number/field.none", "yes"),
        ]);
        let asked = db.ask(&judge, "anything").unwrap();
        let expected = Query::table("none")
            .filter("missing", Op::Eq, true)
            .filter("none", Op::Eq, false)
            .filter("date_field", Op::Gte, "2026-10-03")
            .filter("date_field", Op::Lt, "2026-10-04")
            .filter("present", Op::Eq, json!(null))
            .sort("sort", false);
        assert_eq!(asked.query, Some(expected));
        assert_eq!(asked.refusal, None);
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
    fn a_request_that_might_change_data_is_not_run() {
        let judge = canned([pick("table", "clients"), noul("write", 0.45)]);
        let asked = crm().ask(&judge, "clear out the leads").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.query, Some(Query::table("clients")));
        assert!(
            asked
                .refusal
                .unwrap()
                .starts_with("not confident enough (0.55)")
        );
    }

    #[test]
    fn requests_about_no_table_are_refused() {
        let asked = crm()
            .ask(&canned([pick("table", NONE)]), "tell me a joke")
            .unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(
            asked.refusal.as_deref(),
            Some("the request does not match any table. Tables: clients.")
        );
    }

    #[test]
    fn an_unsure_answer_returns_the_guess_without_running_it() {
        let judge = canned([
            pick("table", "clients"),
            ("clients/field.status".to_owned(), sure("is lead", 0.4)),
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
    fn a_reply_that_is_cut_short_is_not_run() {
        let judge = CutShort(BTreeMap::from([
            pick("table", "clients"),
            noul("write", 0.0),
            noul("statistic", 0.0),
            noul("either_or", 0.0),
            noul("relational", 0.0),
        ]));
        let asked = crm().ask(&judge, "leads").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.query, Some(Query::table("clients")));
        assert!(asked.confidence.abs() < f64::EPSILON);
        assert!(
            asked
                .refusal
                .unwrap()
                .starts_with("not confident enough (0.00)")
        );
    }

    #[test]
    fn an_answer_that_cannot_be_used_is_not_run() {
        let unusable = [
            ("write", Answer::Other),
            ("clients/field.status", Answer::Other),
            ("clients/field.status", Answer::Noul { noul: 0.9 }),
            ("clients/field.status", sure("is churned", 0.95)),
            ("clients/field.status", sure("is lead", f64::NAN)),
            ("clients/field.status", sure("is lead", 1.5)),
            ("sort", sure("revenue", 0.95)),
            ("sort", Answer::Noul { noul: -0.5 }),
            ("leftover", Answer::Other),
        ];
        for (id, answer) in unusable {
            let judge = canned([pick("table", "clients"), (id.to_owned(), answer.clone())]);
            let asked = crm().ask(&judge, "leads").unwrap();
            assert_eq!(asked.page, None, "{id} answered {answer:?}");
            assert_eq!(asked.query, Some(Query::table("clients")));
            assert!(asked.confidence.abs() < f64::EPSILON);
        }
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

    #[test]
    fn a_query_that_ran_never_reports_less_than_the_confidence_it_needs() {
        let judge = canned([pick("table", "clients"), noul("leftover", 0.45)]);
        let asked = crm().ask(&judge, "all clients").unwrap();
        assert_eq!(asked.page.unwrap().total, 3);
        assert!((asked.confidence - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn a_small_schema_is_asked_about_in_one_request_and_then_checked() {
        let judge = Recording::new(canned([pick("table", "clients")]));
        let asked = crm().ask(&judge, "all clients").unwrap();
        assert_eq!(asked.page.unwrap().total, 3);
        assert_eq!(judge.requests.into_inner().len(), 2);
    }

    #[test]
    fn a_schema_too_large_for_one_request_is_asked_about_one_table_at_a_time() {
        let db = crm();
        for index in 0..200 {
            db.define_table(
                &TableDef::new(format!("archive_{index}"))
                    .optional("title", FieldType::Text)
                    .optional("notes", FieldType::Text),
            )
            .unwrap();
        }
        let judge = Recording::new(canned([
            pick("table", "clients"),
            pick("clients/field.status", "is lead"),
        ]));
        let asked = db.ask(&judge, "clients that are leads").unwrap();
        assert_eq!(asked.page.unwrap().total, 2);

        let requests = judge.requests.into_inner();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert!(serde_json::to_string(request).unwrap().len() < 60_000);
        }
        let about_other_tables: Vec<&String> = requests
            .iter()
            .flat_map(BTreeMap::keys)
            .filter(|id| id.starts_with("archive_"))
            .collect();
        assert_eq!(about_other_tables, Vec::<&String>::new());
    }

    #[test]
    fn a_request_too_long_for_the_model_is_refused_without_sending_it() {
        let judge = Recording::new(canned([pick("table", "clients")]));
        let text = "clients named Acme ".repeat(5_000);
        let asked = crm().ask(&judge, &text).unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.refusal.as_deref(), Some(TOO_LARGE));
        assert_eq!(judge.requests.into_inner(), Vec::new());
    }

    #[test]
    fn a_field_with_more_values_than_the_model_accepts_is_refused() {
        let db = AgentDb::open_in_memory().unwrap();
        let codes: Vec<String> = (0..200).map(|code| format!("code_{code}")).collect();
        db.define_table(
            &TableDef::new("parts").optional("code", FieldType::Enum { values: codes }),
        )
        .unwrap();
        let judge = Recording::new(canned([pick("table", "parts")]));
        let asked = db.ask(&judge, "parts with code 7").unwrap();
        assert_eq!(asked.page, None);
        assert_eq!(asked.query, None);
        assert_eq!(asked.refusal.as_deref(), Some(TOO_LARGE));
    }
}
