#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use agentdb::server::{Config, serve};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::spawn_blocking;
    use tokio::time::timeout;

    const SECRET: &str = "test secret";
    const SERVER: &str = env!("CARGO_BIN_EXE_agentdb-server");
    const PATIENCE: Duration = Duration::from_secs(30);

    fn config(dir: &Path) -> Config {
        Config {
            secret: SECRET.to_owned(),
            master_key: "master".to_owned(),
            data_dir: dir.to_owned(),
            max_open_tenants: 100,
            jev: None,
        }
    }

    async fn start_with(config: Config) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { serve(listener, config, pending()).await.unwrap() });
        format!("http://{address}")
    }

    async fn start() -> (String, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let base = start_with(config(dir.path())).await;
        (base, dir)
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into()
    }

    /// Sends one request and returns the status with the JSON body.
    async fn call(
        method: &'static str,
        url: String,
        secret: &'static str,
        body: Value,
    ) -> (u16, Value) {
        spawn_blocking(move || {
            let agent = agent();
            let auth = format!("Bearer {secret}");
            let mut response = match method {
                "GET" => agent.get(&url).header("Authorization", &auth).call(),
                "DELETE" => agent.delete(&url).header("Authorization", &auth).call(),
                "PATCH" => agent
                    .patch(&url)
                    .header("Authorization", &auth)
                    .send_json(&body),
                _ => agent
                    .post(&url)
                    .header("Authorization", &auth)
                    .send_json(&body),
            }
            .unwrap();
            let status = response.status().as_u16();
            (status, response.body_mut().read_json().unwrap())
        })
        .await
        .unwrap()
    }

    /// The URL of a route of one tenant, such as `acme/find`.
    fn of(base: &str, route: &str) -> String {
        format!("{base}/v1/tenants/{route}")
    }

    fn code(body: &Value) -> &str {
        body["error"]["code"].as_str().unwrap()
    }

    fn message(body: &Value) -> &str {
        body["error"]["message"].as_str().unwrap()
    }

    async fn migrate(base: &str, tenant: &str, changes: Value) -> (u16, Value) {
        let body = json!({"changes": changes});
        call("POST", of(base, &format!("{tenant}/migrate")), SECRET, body).await
    }

    async fn define_clients(base: &str, tenant: &str) {
        let (status, body) = migrate(
            base,
            tenant,
            json!([{"op": "define_table", "table": {"name": "clients", "fields": [
                {"name": "name", "type": "text", "required": true},
                {"name": "email", "type": "text", "required": false},
                {"name": "revenue", "type": "number", "required": false}
            ]}}]),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["tables"][0]["name"], "clients");
    }

    /// Inserts `count` clients through batches of 500.
    async fn insert_clients(base: &str, tenant: &str, count: usize) {
        let writes: Vec<Value> = (0..count)
            .map(|n| json!({"op": "insert", "table": "clients", "doc": {"name": format!("Client {n}"), "revenue": n}}))
            .collect();
        for batch in writes.chunks(500) {
            let (status, body) = call(
                "POST",
                of(base, &format!("{tenant}/batch")),
                SECRET,
                json!({"writes": batch}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_need_the_secret() {
        let (base, _dir) = start().await;
        let (status, body) = call("GET", format!("{base}/health"), "", Value::Null).await;
        assert_eq!((status, &body), (200, &json!({"ok": true})));
        for secret in ["", "wrong"] {
            let (status, body) = call("GET", of(&base, "acme/describe"), secret, Value::Null).await;
            assert_eq!((status, code(&body)), (401, "unauthorized"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn documents_round_trip_over_http() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let docs = of(&base, "acme/tables/clients/docs");

        let (status, created) = call(
            "POST",
            docs.clone(),
            SECRET,
            json!({"name": "Acme", "revenue": 900}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            (created["id"].clone(), created["version"].clone()),
            (json!(1), json!(1))
        );

        let patch = json!({"patch": {"email": "a@acme.io"}, "version": 1});
        let (status, updated) = call("PATCH", format!("{docs}/1"), SECRET, patch.clone()).await;
        assert_eq!((status, updated["version"].clone()), (200, json!(2)));
        let (status, conflict) = call("PATCH", format!("{docs}/1"), SECRET, patch).await;
        assert_eq!((status, code(&conflict)), (409, "version_conflict"));

        let (_, fetched) = call("GET", format!("{docs}/1"), SECRET, Value::Null).await;
        assert_eq!(fetched, updated);
        let query =
            json!({"table": "clients", "where": [{"field": "revenue", "op": "gt", "value": 100}]});
        let (_, page) = call("POST", of(&base, "acme/find"), SECRET, query).await;
        assert_eq!(page["total"], 1);
        assert_eq!(page["docs"][0]["email"], "a@acme.io");

        let (status, stale) =
            call("DELETE", format!("{docs}/1?version=1"), SECRET, Value::Null).await;
        assert_eq!((status, code(&stale)), (409, "version_conflict"));
        let (status, deleted) =
            call("DELETE", format!("{docs}/1?version=2"), SECRET, Value::Null).await;
        assert_eq!((status, deleted), (200, json!({"deleted": true})));
        let (status, missing) = call("GET", format!("{docs}/1"), SECRET, Value::Null).await;
        assert_eq!((status, code(&missing)), (404, "not_found"));

        call("POST", docs.clone(), SECRET, json!({"name": "Globex"})).await;
        let (status, deleted) = call("DELETE", format!("{docs}/2"), SECRET, Value::Null).await;
        assert_eq!((status, deleted), (200, json!({"deleted": true})));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_carry_a_code_and_the_teaching_message() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let docs = of(&base, "acme/tables/clients/docs");

        let (status, body) = call(
            "POST",
            docs.clone(),
            SECRET,
            json!({"name": "Acme", "emial": "x"}),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(
            body["error"],
            json!({"code": "unknown_field", "message": "unknown field `emial` on table `clients`. Did you mean `email`? Valid fields: name, email, revenue."})
        );
        let (status, body) = call(
            "GET",
            of(&base, "acme/tables/client/docs/1"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, code(&body)), (404, "unknown_table"));
        let (status, body) =
            call("POST", of(&base, "acme/find"), SECRET, json!({"where": []})).await;
        assert_eq!((status, code(&body)), (400, "invalid_request"));
        let drop = json!([
            {"op": "add_field", "table": "clients", "field": {"name": "phone", "type": "text", "required": false}},
            {"op": "drop_table", "table": "clients"}
        ]);
        call("POST", docs, SECRET, json!({"name": "Acme"})).await;
        let (status, body) = migrate(&base, "acme", drop).await;
        assert_eq!((status, code(&body)), (409, "would_destroy"));
        let (status, body) = call(
            "POST",
            of(&base, "acme/ask"),
            SECRET,
            json!({"text": "all clients"}),
        )
        .await;
        assert_eq!((status, code(&body)), (503, "missing_api_key"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_url_and_an_unknown_route_get_the_same_error_shape() {
        let (base, _dir) = start().await;
        let (status, body) = call(
            "GET",
            of(&base, "acme/tables/clients/docs/first"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, code(&body)), (400, "invalid_request"));
        assert!(message(&body).contains("`first`"), "{body}");
        let (status, body) = call(
            "GET",
            of(&base, "acme/changes?since=start"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, code(&body)), (400, "invalid_request"));
        assert!(message(&body).contains("since"), "{body}");

        let (status, body) = call("GET", of(&base, "acme/tables"), SECRET, Value::Null).await;
        assert_eq!((status, code(&body)), (404, "unknown_route"));
        assert_eq!(
            message(&body),
            "agentdb has no route `GET /v1/tenants/acme/tables`. Tenant routes start with /v1/tenants/{tenant}/ and end in describe, migrate, find, ask, batch, changes, subscribe, tables/{table}/docs or tables/{table}/docs/{id}."
        );
        let (status, body) = call("DELETE", of(&base, "acme/describe"), SECRET, Value::Null).await;
        assert_eq!((status, code(&body)), (405, "unknown_route"));
        assert!(
            message(&body).starts_with("agentdb has no route `DELETE /v1/tenants/acme/describe`."),
            "{body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tenant_ids_are_lowercase_so_no_two_share_a_file() {
        let (base, dir) = start().await;
        for id in ["bad.name", "Acme", "ACME"] {
            let (status, body) = call(
                "GET",
                of(&base, &format!("{id}/describe")),
                SECRET,
                Value::Null,
            )
            .await;
            assert_eq!((status, code(&body)), (400, "invalid_tenant"));
            assert_eq!(
                message(&body),
                format!(
                    "invalid tenant id `{id}`. Use 1 to 64 lowercase letters, digits, `_` or `-`."
                )
            );
        }
        let (status, body) = migrate(&base, "Acme", json!([])).await;
        assert_eq!((status, code(&body)), (400, "invalid_tenant"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tenants_are_isolated_and_encrypted_with_their_own_keys() {
        let (base, dir) = start().await;
        define_clients(&base, "acme").await;
        call(
            "POST",
            of(&base, "acme/tables/clients/docs"),
            SECRET,
            json!({"name": "Top Secret Client"}),
        )
        .await;

        let (status, other) = call("GET", of(&base, "globex/describe"), SECRET, Value::Null).await;
        assert_eq!((status, other), (200, json!({"tables": []})));

        let bytes = std::fs::read(dir.path().join("acme.db")).unwrap();
        let leaked = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
        assert!(!leaked(b"Top Secret Client"));
        assert!(!leaked(b"SQLite format"));

        let mut other_key = config(dir.path());
        other_key.master_key = "a different master key".to_owned();
        let other_key = start_with(other_key).await;
        let (status, body) =
            call("GET", of(&other_key, "acme/describe"), SECRET, Value::Null).await;
        assert_eq!((status, code(&body)), (500, "wrong_key"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_schema_change_creates_a_tenants_file() {
        let (base, dir) = start().await;
        let (status, body) = call("GET", of(&base, "ghost/describe"), SECRET, Value::Null).await;
        assert_eq!((status, body), (200, json!({"tables": []})));
        let (status, body) = call("GET", of(&base, "ghost/changes"), SECRET, Value::Null).await;
        assert_eq!(
            (status, body),
            (200, json!({"changes": [], "latest_seq": 0}))
        );
        let unknown_table = "unknown table `clients`. Existing tables: (none).";
        let (status, body) = call(
            "POST",
            of(&base, "ghost/find"),
            SECRET,
            json!({"table": "clients"}),
        )
        .await;
        assert_eq!((status, message(&body)), (404, unknown_table));
        let (status, body) = call(
            "GET",
            of(&base, "ghost/tables/clients/docs/1"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, message(&body)), (404, unknown_table));
        let (status, body) = call(
            "POST",
            of(&base, "ghost/tables/clients/docs"),
            SECRET,
            json!({"name": "Acme"}),
        )
        .await;
        assert_eq!((status, message(&body)), (404, unknown_table));
        let insert =
            json!({"writes": [{"op": "insert", "table": "clients", "doc": {"name": "Acme"}}]});
        let (status, body) = call("POST", of(&base, "ghost/batch"), SECRET, insert).await;
        assert_eq!((status, message(&body)), (404, unknown_table));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        define_clients(&base, "ghost").await;
        assert!(dir.path().join("ghost.db").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_applies_every_write_or_none() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let batch = of(&base, "acme/batch");

        let writes = json!({"writes": [
            {"op": "insert", "table": "clients", "doc": {"name": "Acme"}},
            {"op": "insert", "table": "clients", "doc": {"name": "Globex"}},
            {"op": "update", "table": "clients", "id": 1, "patch": {"revenue": 5}, "version": 1},
            {"op": "delete", "table": "clients", "id": 2, "version": 1}
        ]});
        let (status, body) = call("POST", batch.clone(), SECRET, writes).await;
        assert_eq!(status, 200, "{body}");
        let docs = body["docs"].as_array().unwrap();
        assert_eq!(docs.len(), 4);
        assert_eq!(
            (&docs[2]["revenue"], &docs[2]["version"]),
            (&json!(5), &json!(2))
        );
        assert_eq!(docs[3]["name"], "Globex");

        let writes = json!({"writes": [
            {"op": "insert", "table": "clients", "doc": {"name": "Initech"}},
            {"op": "insert", "table": "clients", "doc": {"name": "Hooli", "emial": "x"}}
        ]});
        let (status, body) = call("POST", batch.clone(), SECRET, writes).await;
        assert_eq!((status, code(&body)), (400, "unknown_field"));
        assert!(message(&body).starts_with(
            "step 2 of 2 failed, so none of the 2 changes were applied: unknown field `emial`"
        ));
        let (_, page) = call(
            "POST",
            of(&base, "acme/find"),
            SECRET,
            json!({"table": "clients"}),
        )
        .await;
        assert_eq!(page["total"], 1);

        let too_many = vec![json!({"op": "insert", "table": "clients", "doc": {"name": "x"}}); 501];
        let (status, body) = call("POST", batch, SECRET, json!({"writes": too_many})).await;
        assert_eq!((status, code(&body)), (413, "batch_too_large"));
        assert!(
            message(&body).starts_with("a batch takes at most 500 writes"),
            "{body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_carries_a_full_batch_but_not_more_than_eight_mebibytes() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let batch = of(&base, "acme/batch");

        let ordinary = json!({"op": "insert", "table": "clients", "doc": {"name": "n".repeat(4_000), "email": "hello@acme.io", "revenue": 1}});
        let (status, body) = call(
            "POST",
            batch.clone(),
            SECRET,
            json!({"writes": vec![ordinary; 500]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["docs"].as_array().unwrap().len(), 500);

        let huge = json!({"op": "insert", "table": "clients", "doc": {"name": "n".repeat(8 * 1024 * 1024)}});
        let (status, body) = call("POST", batch, SECRET, json!({"writes": [huge]})).await;
        assert_eq!((status, code(&body)), (413, "request_too_large"));
        assert_eq!(
            message(&body),
            "the request body is larger than the 8 MiB one request may carry. Send fewer writes per batch, or smaller documents."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unique_and_indexed_fields_report_conflicts() {
        let (base, _dir) = start().await;
        let (status, body) = migrate(
            &base,
            "acme",
            json!([{"op": "define_table", "table": {"name": "clients", "fields": [
                {"name": "name", "type": "text", "required": true},
                {"name": "email", "type": "text", "required": false, "unique": true},
                {"name": "status", "type": "text", "required": false, "indexed": true}
            ]}}]),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let fields = &body["tables"][0]["fields"];
        assert_eq!(
            fields[0],
            json!({"name": "name", "type": "text", "required": true})
        );
        assert_eq!(
            fields[1],
            json!({"name": "email", "type": "text", "required": false, "indexed": true, "unique": true})
        );

        let docs = of(&base, "acme/tables/clients/docs");
        let acme = json!({"name": "Acme", "email": "hello@acme.io"});
        call("POST", docs.clone(), SECRET, acme.clone()).await;
        let (status, body) = call("POST", docs.clone(), SECRET, acme).await;
        assert_eq!((status, code(&body)), (409, "duplicate_value"));

        call("POST", docs, SECRET, json!({"name": "Acme"})).await;
        let unique_name =
            json!([{"op": "set_unique", "table": "clients", "field": "name", "unique": true}]);
        let (status, body) = migrate(&base, "acme", unique_name).await;
        assert_eq!((status, code(&body)), (409, "duplicates_exist"));

        let unindex =
            json!([{"op": "set_indexed", "table": "clients", "field": "email", "indexed": false}]);
        let (status, body) = migrate(&base, "acme", unindex).await;
        assert_eq!((status, code(&body)), (409, "index_required"));

        let fields: Vec<Value> = (0..11)
            .map(|n| json!({"name": format!("field_{n}"), "type": "text", "required": false, "indexed": true}))
            .collect();
        let wide = json!([{"op": "define_table", "table": {"name": "wide", "fields": fields}}]);
        let (status, body) = migrate(&base, "acme", wide).await;
        assert_eq!((status, code(&body)), (400, "too_many_indexes"));

        let statuses = json!({"name": "status", "type": "enum", "values": ["lead", "lead"], "required": false});
        let repeated = json!([{"op": "add_field", "table": "clients", "field": statuses}]);
        let (status, body) = migrate(&base, "acme", repeated).await;
        assert_eq!((status, code(&body)), (400, "repeated_enum_value"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_table_over_a_thousand_documents_is_only_searched_through_an_index() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        insert_clients(&base, "acme", 1_001).await;
        let find = of(&base, "acme/find");
        let by_revenue = json!({"table": "clients", "where": [{"field": "revenue", "op": "gte", "value": 1_000}]});

        let (status, body) = call("POST", find.clone(), SECRET, by_revenue.clone()).await;
        assert_eq!((status, code(&body)), (400, "query_needs_index"));
        assert!(
            message(&body).ends_with("index `revenue` first with the schema change {\"op\": \"set_indexed\", \"table\": \"clients\", \"field\": \"revenue\", \"indexed\": true}."),
            "{body}"
        );

        let index =
            json!([{"op": "set_indexed", "table": "clients", "field": "revenue", "indexed": true}]);
        let (status, body) = migrate(&base, "acme", index).await;
        assert_eq!(status, 200, "{body}");
        let (status, page) = call("POST", find, SECRET, by_revenue).await;
        assert_eq!((status, &page["total"]), (200, &json!(1)));
        assert_eq!(page["docs"][0]["name"], "Client 1000");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changes_come_with_the_latest_seq_and_old_ones_are_gone() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        insert_clients(&base, "acme", 2).await;
        let (status, body) = call(
            "GET",
            of(&base, "acme/changes?since=1"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!(status, 200);
        let seqs: Vec<_> = body["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| change["seq"].clone())
            .collect();
        assert_eq!(
            (seqs, &body["latest_seq"]),
            (vec![json!(2), json!(3)], &json!(3))
        );

        insert_clients(&base, "acme", 10_000).await;
        let (status, body) = call("GET", of(&base, "acme/changes"), SECRET, Value::Null).await;
        assert_eq!(
            (status, body),
            (200, json!({"changes": [], "latest_seq": 10_003}))
        );
        let (status, body) = call(
            "GET",
            of(&base, "acme/changes?since=0"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, code(&body)), (410, "changes_trimmed"));
        assert!(
            message(&body).ends_with("continue from seq 10003."),
            "{body}"
        );
        let (status, body) = call(
            "GET",
            of(&base, "acme/subscribe?since=2"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, code(&body)), (410, "changes_trimmed"));
        let (status, body) = call(
            "GET",
            of(&base, "acme/changes?since=10002"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!(
            (
                status,
                body["changes"].as_array().unwrap().len(),
                &body["latest_seq"]
            ),
            (200, 1, &json!(10_003))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_faults_are_logged_not_told() {
        let (base, dir) = start().await;
        std::fs::create_dir(dir.path().join("broken.db")).unwrap();
        let (status, body) = migrate(&base, "broken", json!([])).await;
        assert_eq!(status, 500);
        assert_eq!(
            body["error"],
            json!({"code": "storage", "message": "agentdb failed while handling this request. The fault is on the server, not in the call: retry it, and if it keeps failing the operator will find the cause in the server log."})
        );
    }

    type Feed = Box<dyn Iterator<Item = (String, Value)> + Send>;

    /// Turns the lines of a server-sent event stream into each event's name
    /// and data. A change has the name `change`.
    fn events(lines: impl Iterator<Item = String> + Send + 'static) -> Feed {
        let mut name = "change".to_owned();
        Box::new(lines.filter_map(move |line| match line.split_once(": ") {
            Some(("event", event)) => {
                name = event.to_owned();
                None
            }
            Some(("data", data)) => {
                let name = std::mem::replace(&mut name, "change".to_owned());
                Some((name, serde_json::from_str(data).unwrap()))
            }
            _ => None,
        }))
    }

    async fn subscribe(url: String) -> Feed {
        spawn_blocking(move || {
            let response = agent()
                .get(&url)
                .header("Authorization", &format!("Bearer {SECRET}"))
                .call()
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let lines = BufReader::new(response.into_body().into_reader()).lines();
            events(lines.map_while(Result::ok))
        })
        .await
        .unwrap()
    }

    /// Reads the next `count` changes of a feed and returns their `seq`s.
    async fn next_seqs(feed: Feed, count: usize) -> (Vec<i64>, Feed) {
        let (changes, feed) = next_events(feed, count).await;
        let seq = |(name, change): &(String, Value)| {
            assert_eq!(name, "change");
            change["seq"].as_i64().unwrap()
        };
        (changes.iter().map(seq).collect(), feed)
    }

    /// Reads the next `count` events of a feed, fewer if it ends first.
    async fn next_events(mut feed: Feed, count: usize) -> (Vec<(String, Value)>, Feed) {
        let reading = spawn_blocking(move || {
            let events: Vec<_> = feed.by_ref().take(count).collect();
            (events, feed)
        });
        timeout(PATIENCE, reading).await.unwrap().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribing_replays_the_log_then_streams_live() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        insert_clients(&base, "acme", 1_200).await;

        let feed = subscribe(of(&base, "acme/subscribe?since=0")).await;
        let (replayed, feed) = next_events(feed, 1_201).await;
        assert_eq!(
            (&replayed[0].1["kind"], &replayed[0].1["doc"]),
            (&json!("schema"), &Value::Null)
        );
        assert_eq!(replayed[1].1["kind"], "insert");
        let seqs: Vec<_> = replayed
            .iter()
            .map(|(_, change)| change["seq"].as_i64().unwrap())
            .collect();
        assert_eq!(seqs, (1..=1_201).collect::<Vec<_>>());

        let patch = json!({"patch": {"revenue": 7}});
        call(
            "PATCH",
            of(&base, "acme/tables/clients/docs/1"),
            SECRET,
            patch,
        )
        .await;
        insert_clients(&base, "acme", 1).await;
        let (live, _feed) = next_events(feed, 2).await;
        assert_eq!(
            (&live[0].1["seq"], &live[0].1["kind"]),
            (&json!(1_202), &json!("update"))
        );
        assert_eq!(live[0].1["doc"]["revenue"], 7);
        assert_eq!(
            (&live[1].1["seq"], &live[1].1["kind"]),
            (&json!(1_203), &json!("insert"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribing_without_since_starts_with_the_next_write() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        insert_clients(&base, "acme", 1).await;
        let feed = subscribe(of(&base, "acme/subscribe")).await;
        insert_clients(&base, "acme", 2).await;
        let (seqs, _feed) = next_seqs(feed, 2).await;
        assert_eq!(seqs, [3, 4]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscriber_can_wait_for_a_tenant_that_does_not_exist_yet() {
        let (base, dir) = start().await;
        let feed = subscribe(of(&base, "acme/subscribe")).await;
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        define_clients(&base, "acme").await;
        insert_clients(&base, "acme", 1).await;
        let (events, _feed) = next_events(feed, 2).await;
        let kinds: Vec<_> = events.iter().map(|(_, change)| &change["kind"]).collect();
        assert_eq!(kinds, ["schema", "insert"]);
    }

    /// Repeats a read until the server answers it with 200.
    async fn until_ok(url: String) {
        let waited = Instant::now();
        while call("GET", url.clone(), SECRET, Value::Null).await.0 != 200 {
            assert!(waited.elapsed() < PATIENCE, "{url} never answered 200");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn open_databases(dir: &Path) -> usize {
        let is_log = |name: std::ffi::OsString| name.to_string_lossy().ends_with(".db-wal");
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|entry| is_log(entry.as_ref().unwrap().file_name()))
            .count()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_tenants_are_closed_to_keep_the_open_ones_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut two_at_most = config(dir.path());
        two_at_most.max_open_tenants = 2;
        let base = start_with(two_at_most).await;

        for tenant in ["one", "two", "three", "four", "five"] {
            define_clients(&base, tenant).await;
            insert_clients(&base, tenant, 1).await;
            assert!(open_databases(dir.path()) <= 2);
        }
        for tenant in ["one", "two", "three", "four", "five"] {
            let (status, page) = call(
                "POST",
                of(&base, &format!("{tenant}/find")),
                SECRET,
                json!({"table": "clients"}),
            )
            .await;
            assert_eq!((status, &page["total"]), (200, &json!(1)));
        }
        assert_eq!(open_databases(dir.path()), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_with_a_subscriber_is_never_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut two_at_most = config(dir.path());
        two_at_most.max_open_tenants = 2;
        let base = start_with(two_at_most).await;
        for tenant in ["one", "two", "three"] {
            define_clients(&base, tenant).await;
        }

        let one = subscribe(of(&base, "one/subscribe")).await;
        for tenant in ["two", "three", "two", "three"] {
            insert_clients(&base, tenant, 1).await;
        }
        insert_clients(&base, "one", 1).await;
        let (seqs, one) = next_seqs(one, 1).await;
        assert_eq!(seqs, [2]);

        let two = subscribe(of(&base, "two/subscribe")).await;
        let (status, body) = call("GET", of(&base, "three/describe"), SECRET, Value::Null).await;
        assert_eq!((status, code(&body)), (503, "too_many_open_tenants"));
        assert_eq!(
            message(&body),
            "the server has 2 tenant databases open and every one is in use by a request or a subscriber, so it cannot open another right now. Retry in a moment. If this keeps happening, the operator should raise AGENTDB_MAX_OPEN_TENANTS."
        );

        drop(two);
        insert_clients(&base, "two", 1).await;
        until_ok(of(&base, "three/describe")).await;
        drop(one);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutting_down_ends_the_feeds_and_stops_serving() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel::<()>();
        let told_to_stop = async move { stopped.await.unwrap() };
        let server = tokio::spawn(serve(listener, config(dir.path()), told_to_stop));
        define_clients(&base, "acme").await;
        let feed = subscribe(of(&base, "acme/subscribe")).await;
        let waiting = subscribe(of(&base, "nobody/subscribe")).await;

        stop.send(()).unwrap();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(next_events(feed, 1).await.0, []);
        assert_eq!(next_events(waiting, 1).await.0, []);
        assert_eq!(open_databases(dir.path()), 0);
        let refused = spawn_blocking(move || agent().get(format!("{base}/health")).call().is_err());
        assert!(refused.await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutting_down_does_not_wait_forever_for_a_stuck_request() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let told_to_stop = async move { stopped.await.unwrap() };
        let server = tokio::spawn(serve(listener, config(dir.path()), told_to_stop));
        let mut stuck = TcpStream::connect(address).unwrap();
        stuck
            .write_all(
                b"POST /v1/tenants/acme/migrate HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n",
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        stop.send(()).unwrap();
        timeout(Duration::from_secs(8), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(stuck);
    }

    fn free_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    }

    fn run_server(dir: &Path, port: u16, secret: &str, master_key: &str) -> Child {
        Command::new(SERVER)
            .env_clear()
            .env("AGENTDB_SECRET", secret)
            .env("AGENTDB_MASTER_KEY", master_key)
            .env("AGENTDB_DATA_DIR", dir)
            .env("AGENTDB_PORT", port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn stderr_of(mut server: Child) -> (bool, String) {
        let mut stderr = String::new();
        server
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        (server.wait().unwrap().success(), stderr)
    }

    #[test]
    fn the_server_refuses_to_start_with_weak_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let long = "0123456789abcdef0123456789abcdef";

        let (started, stderr) = stderr_of(run_server(dir.path(), free_port(), "hunter2", long));
        assert!(!started);
        assert!(
            stderr.contains(
                "the AGENTDB_SECRET environment variable must be set to at least 32 characters"
            ),
            "{stderr}"
        );
        assert!(!stderr.contains("hunter2"));

        let (started, stderr) = stderr_of(run_server(dir.path(), free_port(), long, ""));
        assert!(!started);
        assert!(
            stderr.contains(
                "the AGENTDB_MASTER_KEY environment variable must be set to at least 32 characters"
            ),
            "{stderr}"
        );

        let (started, stderr) = stderr_of(run_server(dir.path(), free_port(), long, long));
        assert!(!started);
        assert!(
            stderr.contains("AGENTDB_SECRET and AGENTDB_MASTER_KEY must be two different values"),
            "{stderr}"
        );
        assert!(!stderr.contains(long));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sigterm_stops_the_server_and_closes_its_databases() {
        const LONG_SECRET: &str = "a secret that is long enough to pass";
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let mut server = run_server(
            dir.path(),
            port,
            LONG_SECRET,
            "a master key that is long enough",
        );
        let base = format!("http://127.0.0.1:{port}");
        let health = format!("{base}/health");
        let waited = Instant::now();
        while spawn_blocking({
            let health = health.clone();
            move || agent().get(&health).call().is_err()
        })
        .await
        .unwrap()
        {
            assert!(waited.elapsed() < PATIENCE, "the server did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let table =
            json!({"changes": [{"op": "define_table", "table": {"name": "notes", "fields": []}}]});
        let (status, _) = call("POST", of(&base, "acme/migrate"), LONG_SECRET, table).await;
        assert_eq!(status, 200);
        assert_eq!(open_databases(dir.path()), 1);

        let killed = Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let exited = spawn_blocking(move || server.wait().unwrap());
        assert!(timeout(PATIENCE, exited).await.unwrap().unwrap().success());
        assert_eq!(open_databases(dir.path()), 0);
        assert!(dir.path().join("acme.db").exists());
    }
}
