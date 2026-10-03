#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::path::Path;

    use agentdb::server::{Config, router};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::task::spawn_blocking;

    const SECRET: &str = "test secret";

    async fn start_in(dir: &Path, master_key: &str) -> String {
        let config = Config {
            secret: SECRET.to_owned(),
            master_key: master_key.to_owned(),
            data_dir: dir.to_owned(),
            jev: None,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(config)).await.unwrap() });
        format!("http://{address}")
    }

    async fn start() -> (String, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let base = start_in(dir.path(), "master").await;
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

    async fn define_clients(base: &str, tenant: &str) {
        let changes = json!({"changes": [{"op": "define_table", "table": {"name": "clients", "fields": [
            {"name": "name", "type": "text", "required": true},
            {"name": "email", "type": "text", "required": false},
            {"name": "revenue", "type": "number", "required": false}
        ]}}]});
        let (status, body) = call(
            "POST",
            format!("{base}/v1/tenants/{tenant}/migrate"),
            SECRET,
            changes,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["tables"][0]["name"], "clients");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_need_the_secret() {
        let (base, _dir) = start().await;
        let (status, body) = call("GET", format!("{base}/health"), "", Value::Null).await;
        assert_eq!((status, &body), (200, &json!({"ok": true})));
        for secret in ["", "wrong"] {
            let (status, body) = call(
                "GET",
                format!("{base}/v1/tenants/acme/describe"),
                secret,
                Value::Null,
            )
            .await;
            assert_eq!(status, 401);
            assert_eq!(body["error"]["code"], "unauthorized");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn documents_round_trip_over_http() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let docs = format!("{base}/v1/tenants/acme/tables/clients/docs");

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
        assert_eq!(status, 409);
        assert_eq!(conflict["error"]["code"], "version_conflict");

        let (_, fetched) = call("GET", format!("{docs}/1"), SECRET, Value::Null).await;
        assert_eq!(fetched, updated);
        let query =
            json!({"table": "clients", "where": [{"field": "revenue", "op": "gt", "value": 100}]});
        let (_, page) = call(
            "POST",
            format!("{base}/v1/tenants/acme/find"),
            SECRET,
            query,
        )
        .await;
        assert_eq!(page["total"], 1);
        assert_eq!(page["docs"][0]["email"], "a@acme.io");

        let (status, deleted) = call("DELETE", format!("{docs}/1"), SECRET, Value::Null).await;
        assert_eq!((status, deleted), (200, json!({"deleted": true})));
        let (status, missing) = call("GET", format!("{docs}/1"), SECRET, Value::Null).await;
        assert_eq!(status, 404);
        assert_eq!(missing["error"]["code"], "not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_carry_a_code_and_the_teaching_message() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let docs = format!("{base}/v1/tenants/acme/tables/clients/docs");

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
            format!("{base}/v1/tenants/acme/tables/client/docs/1"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].clone()),
            (404, json!("unknown_table"))
        );
        let (status, body) = call(
            "POST",
            format!("{base}/v1/tenants/acme/find"),
            SECRET,
            json!({"where": []}),
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].clone()),
            (400, json!("invalid_request"))
        );
        let (status, body) = call(
            "GET",
            format!("{base}/v1/tenants/bad.name/describe"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].clone()),
            (400, json!("invalid_tenant"))
        );
        let drop = json!({"changes": [
            {"op": "add_field", "table": "clients", "field": {"name": "phone", "type": "text", "required": false}},
            {"op": "drop_table", "table": "clients"}
        ]});
        call("POST", docs, SECRET, json!({"name": "Acme"})).await;
        let (status, body) = call(
            "POST",
            format!("{base}/v1/tenants/acme/migrate"),
            SECRET,
            drop,
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].clone()),
            (409, json!("would_destroy"))
        );
        let (status, body) = call(
            "POST",
            format!("{base}/v1/tenants/acme/ask"),
            SECRET,
            json!({"text": "all clients"}),
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].clone()),
            (503, json!("missing_api_key"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tenants_are_isolated_and_encrypted_with_their_own_keys() {
        let (base, dir) = start().await;
        define_clients(&base, "acme").await;
        call(
            "POST",
            format!("{base}/v1/tenants/acme/tables/clients/docs"),
            SECRET,
            json!({"name": "Top Secret Client"}),
        )
        .await;

        let (status, other) = call(
            "GET",
            format!("{base}/v1/tenants/globex/describe"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!((status, other), (200, json!({"tables": []})));

        let bytes = std::fs::read(dir.path().join("acme.db")).unwrap();
        let leaked = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
        assert!(!leaked(b"Top Secret Client"));
        assert!(!leaked(b"SQLite format"));
        assert!(dir.path().join("globex.db").exists());

        let other_key = start_in(dir.path(), "a different master key").await;
        let (status, body) = call(
            "GET",
            format!("{other_key}/v1/tenants/acme/describe"),
            SECRET,
            Value::Null,
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].clone()),
            (500, json!("wrong_key"))
        );
    }

    /// Reads server-sent events until `count` have arrived.
    fn read_events(url: &str, count: usize) -> Vec<Value> {
        let mut response = agent()
            .get(url)
            .header("Authorization", &format!("Bearer {SECRET}"))
            .call()
            .unwrap();
        BufReader::new(response.body_mut().as_reader())
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(line.strip_prefix("data: ")?).ok())
            .take(count)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribing_replays_the_backlog_then_streams_live() {
        let (base, _dir) = start().await;
        define_clients(&base, "acme").await;
        let docs = format!("{base}/v1/tenants/acme/tables/clients/docs");
        call("POST", docs.clone(), SECRET, json!({"name": "Acme"})).await;

        let url = format!("{base}/v1/tenants/acme/subscribe?since=0");
        let reader = spawn_blocking(move || read_events(&url, 3));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        call(
            "PATCH",
            format!("{docs}/1"),
            SECRET,
            json!({"patch": {"revenue": 7}}),
        )
        .await;

        let events = reader.await.unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event["kind"].clone()).collect();
        assert_eq!(kinds, [json!("schema"), json!("insert"), json!("update")]);
        let seqs: Vec<_> = events.iter().map(|event| event["seq"].clone()).collect();
        assert_eq!(seqs, [json!(1), json!(2), json!(3)]);
        assert_eq!(events[2]["doc"]["revenue"], 7);
        assert_eq!(events[0]["doc"], Value::Null);
    }
}
