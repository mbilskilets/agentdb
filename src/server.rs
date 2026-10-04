//! The HTTP server: one process serving many tenants, each with its own
//! encrypted database file.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query as UrlQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, mpsc};
use tokio::task::spawn_blocking;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::{AgentDb, Change, DbError, Jev, Query, SchemaChange};

const MAX_TENANT_LEN: usize = 64;
const SUBSCRIBER_BUFFER: usize = 256;

#[derive(Debug, Clone)]
pub struct Config {
    /// Callers must send this as `Authorization: Bearer <secret>`.
    pub secret: String,
    /// Each tenant's encryption key is derived from this. Losing it makes
    /// every tenant file unreadable.
    pub master_key: String,
    /// Where tenant files live, one `<tenant>.db` each.
    pub data_dir: PathBuf,
    /// Needed only for `ask`.
    pub jev: Option<Jev>,
}

impl Config {
    /// Reads `AGENTDB_SECRET`, `AGENTDB_MASTER_KEY`, `AGENTDB_DATA_DIR`
    /// (default `./data`) and `TYPESAFE_API_KEY` (optional).
    ///
    /// # Errors
    /// Names the variable that is missing.
    pub fn from_env() -> Result<Self, String> {
        let required = |name: &str| match std::env::var(name) {
            Ok(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("the {name} environment variable must be set")),
        };
        Ok(Self {
            secret: required("AGENTDB_SECRET")?,
            master_key: required("AGENTDB_MASTER_KEY")?,
            data_dir: std::env::var("AGENTDB_DATA_DIR")
                .unwrap_or_else(|_| "data".to_owned())
                .into(),
            jev: Jev::from_env().ok(),
        })
    }
}

struct App {
    config: Config,
    /// Tenant databases stay open: opening one costs far more than a query.
    tenants: Mutex<HashMap<String, Arc<AgentDb>>>,
}

impl App {
    fn tenant(&self, id: &str) -> Result<Arc<AgentDb>, ApiError> {
        let valid = !id.is_empty()
            && id.len() <= MAX_TENANT_LEN
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_tenant",
                format!(
                    "invalid tenant id `{id}`. Use 1 to {MAX_TENANT_LEN} letters, digits, `_` or `-`."
                ),
            ));
        }
        let mut tenants = self.tenants.lock().map_err(ApiError::internal)?;
        if let Some(db) = tenants.get(id) {
            return Ok(Arc::clone(db));
        }
        std::fs::create_dir_all(&self.config.data_dir).map_err(ApiError::internal)?;
        let path = self.config.data_dir.join(format!("{id}.db"));
        let db = Arc::new(AgentDb::open(path, &self.tenant_key(id)?)?);
        tenants.insert(id.to_owned(), Arc::clone(&db));
        Ok(db)
    }

    /// A raw 256-bit key, unique per tenant. Raw keys skip the slow
    /// passphrase stretching, which a random key does not need.
    fn tenant_key(&self, id: &str) -> Result<String, ApiError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.config.master_key.as_bytes())
            .map_err(ApiError::internal)?;
        mac.update(b"agentdb-tenant:");
        mac.update(id.as_bytes());
        let mut hex = String::new();
        for byte in mac.finalize().into_bytes() {
            write!(hex, "{byte:02x}").map_err(ApiError::internal)?;
        }
        Ok(format!("x'{hex}'"))
    }
}

/// An error as the caller sees it: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "used as a map_err callback, which hands over the error by value"
    )]
    fn internal(error: impl ToString) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            error.to_string(),
        )
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        let code = error.code();
        let status = match code {
            "not_found" | "unknown_table" => StatusCode::NOT_FOUND,
            "version_conflict" | "still_referenced" | "table_exists" | "field_exists"
            | "table_referenced" | "enum_value_in_use" | "would_destroy" => StatusCode::CONFLICT,
            "missing_api_key" => StatusCode::SERVICE_UNAVAILABLE,
            "model_unavailable" => StatusCode::BAD_GATEWAY,
            "storage" | "internal" | "newer_format" | "wrong_key" | "empty_key" => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            _ => StatusCode::BAD_REQUEST,
        };
        Self::new(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

type Api<T> = Result<Json<T>, ApiError>;

/// Runs database work for one tenant off the async threads.
async fn with_tenant<T, F>(app: Arc<App>, tenant: String, work: F) -> Api<T>
where
    T: Send + 'static,
    F: FnOnce(&AgentDb, &App) -> Result<T, DbError> + Send + 'static,
{
    spawn_blocking(move || {
        let db = app.tenant(&tenant)?;
        Ok(Json(work(&db, &app)?))
    })
    .await
    .map_err(ApiError::internal)?
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("the request body is not valid: {error}."),
        )
    })
}

/// Builds the server's routes.
pub fn router(config: Config) -> Router {
    let app = Arc::new(App {
        config,
        tenants: Mutex::new(HashMap::new()),
    });
    let tenant_routes = Router::new()
        .route("/describe", get(describe))
        .route("/migrate", post(migrate))
        .route("/find", post(find))
        .route("/ask", post(ask))
        .route("/changes", get(changes))
        .route("/subscribe", get(subscribe))
        .route("/tables/{table}/docs", post(insert))
        .route(
            "/tables/{table}/docs/{id}",
            get(get_doc).patch(update).delete(delete),
        )
        .route_layer(middleware::from_fn_with_state(Arc::clone(&app), authorize));
    Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .nest("/v1/tenants/{tenant}", tenant_routes)
        .with_state(app)
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let sent = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let matches: bool = sent.as_bytes().ct_eq(app.config.secret.as_bytes()).into();
    if matches {
        next.run(request).await
    } else {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "send the server secret as `Authorization: Bearer <secret>`.",
        )
        .into_response()
    }
}

async fn describe(State(app): State<Arc<App>>, Path(tenant): Path<String>) -> Api<Value> {
    with_tenant(app, tenant, |db, _| Ok(json!({"tables": db.describe()?}))).await
}

#[derive(Deserialize)]
struct MigrateBody {
    changes: Vec<SchemaChange>,
}

async fn migrate(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    body: Bytes,
) -> Api<Value> {
    let body: MigrateBody = parse(&body)?;
    with_tenant(app, tenant, move |db, _| {
        db.migrate(&body.changes)?;
        Ok(json!({"tables": db.describe()?}))
    })
    .await
}

async fn find(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    body: Bytes,
) -> Api<crate::Page> {
    let query: Query = parse(&body)?;
    with_tenant(app, tenant, move |db, _| db.find(&query)).await
}

#[derive(Deserialize)]
struct AskBody {
    text: String,
}

async fn ask(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    body: Bytes,
) -> Api<crate::Asked> {
    let body: AskBody = parse(&body)?;
    with_tenant(app, tenant, move |db, app| {
        let jev = app.config.jev.as_ref().ok_or(DbError::MissingApiKey)?;
        db.ask(jev, &body.text)
    })
    .await
}

#[derive(Deserialize)]
struct Since {
    since: Option<i64>,
}

#[derive(Serialize)]
struct Changes {
    changes: Vec<Change>,
}

async fn changes(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    UrlQuery(since): UrlQuery<Since>,
) -> Api<Changes> {
    with_tenant(app, tenant, move |db, _| {
        Ok(Changes {
            changes: db.changes_since(since.since.unwrap_or(0))?,
        })
    })
    .await
}

async fn insert(
    State(app): State<Arc<App>>,
    Path((tenant, table)): Path<(String, String)>,
    body: Bytes,
) -> Api<crate::Doc> {
    let doc: Value = parse(&body)?;
    with_tenant(app, tenant, move |db, _| db.insert(&table, doc)).await
}

async fn get_doc(
    State(app): State<Arc<App>>,
    Path((tenant, table, id)): Path<(String, String, i64)>,
) -> Api<crate::Doc> {
    with_tenant(app, tenant, move |db, _| db.get(&table, id)).await
}

#[derive(Deserialize)]
struct UpdateBody {
    patch: Value,
    #[serde(default)]
    version: Option<i64>,
}

async fn update(
    State(app): State<Arc<App>>,
    Path((tenant, table, id)): Path<(String, String, i64)>,
    body: Bytes,
) -> Api<crate::Doc> {
    let body: UpdateBody = parse(&body)?;
    with_tenant(app, tenant, move |db, _| {
        db.update(&table, id, body.patch, body.version)
    })
    .await
}

async fn delete(
    State(app): State<Arc<App>>,
    Path((tenant, table, id)): Path<(String, String, i64)>,
) -> Api<Value> {
    with_tenant(app, tenant, move |db, _| {
        db.delete(&table, id, None)?;
        Ok(json!({"deleted": true}))
    })
    .await
}

/// Streams changes as server-sent events. With `?since=N` it first replays
/// everything after change `N`, then continues live, so a client that
/// reconnects with the last `seq` it saw misses nothing.
async fn subscribe(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    UrlQuery(since): UrlQuery<Since>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (sender, receiver) = mpsc::channel::<Change>(SUBSCRIBER_BUFFER);
    let (live, replay) = spawn_blocking(move || {
        let db = app.tenant(&tenant)?;
        // Subscribe before reading the backlog so nothing falls in between.
        let live = db.subscribe();
        let replay = match since.since {
            Some(seq) => backlog(&db, seq)?,
            None => Vec::new(),
        };
        Ok::<_, ApiError>((live, replay))
    })
    .await
    .map_err(ApiError::internal)??;
    tokio::spawn(forward(replay, live, sender));
    let events = ReceiverStream::new(receiver).map(|change| {
        let data = serde_json::to_string(&change).unwrap_or_default();
        Ok(Event::default().id(change.seq.to_string()).data(data))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

fn backlog(db: &AgentDb, since: i64) -> Result<Vec<Change>, DbError> {
    let mut all: Vec<Change> = Vec::new();
    loop {
        let from = all.last().map_or(since, |change| change.seq);
        let page = db.changes_since(from)?;
        if page.is_empty() {
            return Ok(all);
        }
        all.extend(page);
    }
}

/// Sends the backlog, then live changes, until the client disconnects or
/// falls too far behind to follow.
async fn forward(
    replay: Vec<Change>,
    mut live: broadcast::Receiver<Change>,
    sender: mpsc::Sender<Change>,
) {
    let mut last = 0;
    for change in replay {
        last = change.seq;
        if sender.send(change).await.is_err() {
            return;
        }
    }
    loop {
        let received = tokio::select! {
            () = sender.closed() => return,
            received = live.recv() => received,
        };
        match received {
            Ok(change) if change.seq > last => {
                if sender.send(change).await.is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}
