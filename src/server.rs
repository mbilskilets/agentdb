//! The HTTP server: one process serving many tenants, each with its own
//! encrypted database file.

mod error;
mod subscribe;
mod tenants;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query as UrlQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::spawn_blocking;

use self::error::ApiError;
use self::subscribe::subscribe;
use self::tenants::{Missing, Tenants};
use crate::{AgentDb, Asked, Change, DbError, Doc, Jev, Page, Query, SchemaChange, Write};

/// A shorter secret or master key is refused at startup: it could be guessed.
const MIN_SECRET_LEN: usize = 32;
const DEFAULT_MAX_OPEN_TENANTS: usize = 100;
const MAX_BODY_MIB: usize = 8;
/// Room for a batch of 500 writes of 16 KiB each.
const MAX_BODY_BYTES: usize = MAX_BODY_MIB * 1024 * 1024;
/// How long requests in flight get to finish once the server is told to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Config {
    /// Callers must send this as `Authorization: Bearer <secret>`.
    pub secret: String,
    /// Each tenant's encryption key is derived from this. Losing it makes
    /// every tenant file unreadable.
    pub master_key: String,
    /// Where tenant files live, one `<tenant>.db` each.
    pub data_dir: PathBuf,
    /// How many tenant databases may be open at once. Each one holds five
    /// file descriptors, so keep this well under the process's limit.
    pub max_open_tenants: usize,
    /// Needed only for `ask`.
    pub jev: Option<Jev>,
}

impl Config {
    /// Reads `AGENTDB_SECRET`, `AGENTDB_MASTER_KEY`, `AGENTDB_DATA_DIR`
    /// (default `./data`), `AGENTDB_MAX_OPEN_TENANTS` (default 100) and
    /// `TYPESAFE_API_KEY` (optional).
    ///
    /// # Errors
    /// Names the variable that is missing or unusable, never its value.
    pub fn from_env() -> Result<Self, String> {
        let secret = secret_from_env("AGENTDB_SECRET")?;
        let master_key = secret_from_env("AGENTDB_MASTER_KEY")?;
        if secret == master_key {
            return Err("AGENTDB_SECRET and AGENTDB_MASTER_KEY must be two different values: every client holds the secret, and only the server may hold the key that encrypts the files".to_owned());
        }
        let max_open_tenants = match std::env::var("AGENTDB_MAX_OPEN_TENANTS") {
            Ok(text) => text
                .parse()
                .ok()
                .filter(|max| *max > 0)
                .ok_or("AGENTDB_MAX_OPEN_TENANTS must be a whole number above 0")?,
            Err(_) => DEFAULT_MAX_OPEN_TENANTS,
        };
        Ok(Self {
            secret,
            master_key,
            data_dir: std::env::var("AGENTDB_DATA_DIR")
                .unwrap_or_else(|_| "data".to_owned())
                .into(),
            max_open_tenants,
            jev: Jev::from_env().ok(),
        })
    }
}

fn secret_from_env(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) if value.len() >= MIN_SECRET_LEN => Ok(value),
        _ => Err(format!(
            "the {name} environment variable must be set to at least {MIN_SECRET_LEN} characters. Generate one with: openssl rand -hex 32"
        )),
    }
}

struct App {
    secret: String,
    jev: Option<Jev>,
    tenants: Tenants,
    stopping: watch::Receiver<bool>,
}

impl App {
    /// Completes once the server has been told to stop.
    async fn shutting_down(&self) {
        let mut stopping = self.stopping.clone();
        while !*stopping.borrow_and_update() {
            if stopping.changed().await.is_err() {
                return;
            }
        }
    }

    async fn grace_ran_out(&self) {
        self.shutting_down().await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    }
}

/// Serves `config` on `listener` until `shutdown` completes. The server
/// then stops accepting connections, ends every change feed, gives the
/// requests in flight five seconds to finish and closes the tenant
/// databases.
///
/// # Errors
/// Fails when the listener stops accepting connections.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let (stop, stopping) = watch::channel(false);
    let app = Arc::new(App {
        secret: config.secret,
        jev: config.jev,
        tenants: Tenants::new(config.data_dir, config.master_key, config.max_open_tenants),
        stopping,
    });
    let serving = axum::serve(listener, router(&app)).with_graceful_shutdown(async move {
        shutdown.await;
        stop.send_replace(true);
    });
    tokio::select! {
        served = serving.into_future() => served?,
        () = app.grace_ran_out() => {}
    }
    app.tenants.close();
    Ok(())
}

fn router(app: &Arc<App>) -> Router {
    let tenant_routes = Router::new()
        .route("/describe", get(describe))
        .route("/migrate", post(migrate))
        .route("/find", post(find))
        .route("/ask", post(ask))
        .route("/batch", post(batch))
        .route("/changes", get(changes))
        .route("/subscribe", get(subscribe))
        .route("/tables/{table}/docs", post(insert))
        .route(
            "/tables/{table}/docs/{id}",
            get(get_doc).patch(update).delete(delete),
        )
        .route_layer(middleware::from_fn_with_state(Arc::clone(app), authorize));
    Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .nest("/v1/tenants/{tenant}", tenant_routes)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::clone(app))
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let sent = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let matches: bool = sent.as_bytes().ct_eq(app.secret.as_bytes()).into();
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

/// A JSON request body.
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        format!(
                            "the request body is larger than the {MAX_BODY_MIB} MiB one request may carry. Send fewer writes per batch, or smaller documents."
                        ),
                    )
                } else {
                    invalid_request(&rejection.body_text())
                }
            })?;
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|error| invalid_request(&error.to_string()))
    }
}

fn invalid_request(reason: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        format!("the request body is not valid: {reason}."),
    )
}

type Api<T> = Result<Json<T>, ApiError>;

/// Runs work that touches a disk off the async threads.
async fn blocking<T, F>(work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    spawn_blocking(work).await.map_err(ApiError::internal)?
}

/// Runs database work for one tenant. A tenant that has no file and is not
/// to get one is read as the empty database it would be.
async fn with_tenant<T, F>(app: Arc<App>, tenant: String, missing: Missing, work: F) -> Api<T>
where
    T: Send + 'static,
    F: FnOnce(&AgentDb, &App) -> Result<T, DbError> + Send + 'static,
{
    blocking(move || {
        let db = match app.tenants.open(&tenant, missing)? {
            Some(db) => db,
            None => Arc::new(AgentDb::open_in_memory()?),
        };
        Ok(Json(work(&db, &app)?))
    })
    .await
}

async fn describe(State(app): State<Arc<App>>, Path(tenant): Path<String>) -> Api<Value> {
    with_tenant(app, tenant, Missing::Skip, |db, _| {
        Ok(json!({"tables": db.describe()?}))
    })
    .await
}

#[derive(Deserialize)]
struct MigrateBody {
    changes: Vec<SchemaChange>,
}

async fn migrate(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    Body(body): Body<MigrateBody>,
) -> Api<Value> {
    with_tenant(app, tenant, Missing::Create, move |db, _| {
        db.migrate(&body.changes)?;
        Ok(json!({"tables": db.describe()?}))
    })
    .await
}

async fn find(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    Body(query): Body<Query>,
) -> Api<Page> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| db.find(&query)).await
}

#[derive(Deserialize)]
struct AskBody {
    text: String,
}

async fn ask(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    Body(body): Body<AskBody>,
) -> Api<Asked> {
    with_tenant(app, tenant, Missing::Skip, move |db, app| {
        let jev = app.jev.as_ref().ok_or(DbError::MissingApiKey)?;
        db.ask(jev, &body.text)
    })
    .await
}

#[derive(Deserialize)]
struct BatchBody {
    writes: Vec<Write>,
}

#[derive(Serialize)]
struct Docs {
    docs: Vec<Doc>,
}

async fn batch(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    Body(body): Body<BatchBody>,
) -> Api<Docs> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| {
        Ok(Docs {
            docs: db.batch(body.writes)?,
        })
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
    latest_seq: i64,
}

/// Up to 500 changes after `since`, and the `seq` of the newest write.
/// Without `since` there is nothing to replay, and the answer says only
/// where the log stands.
async fn changes(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    UrlQuery(Since { since }): UrlQuery<Since>,
) -> Api<Changes> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| {
        let changes = match since {
            Some(seq) => db.changes_since(seq)?,
            None => Vec::new(),
        };
        Ok(Changes {
            changes,
            latest_seq: db.latest_seq()?,
        })
    })
    .await
}

async fn insert(
    State(app): State<Arc<App>>,
    Path((tenant, table)): Path<(String, String)>,
    Body(doc): Body<Value>,
) -> Api<Doc> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| {
        db.insert(&table, doc)
    })
    .await
}

async fn get_doc(
    State(app): State<Arc<App>>,
    Path((tenant, table, id)): Path<(String, String, i64)>,
) -> Api<Doc> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| db.get(&table, id)).await
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
    Body(body): Body<UpdateBody>,
) -> Api<Doc> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| {
        db.update(&table, id, body.patch, body.version)
    })
    .await
}

#[derive(Deserialize)]
struct Expected {
    version: Option<i64>,
}

async fn delete(
    State(app): State<Arc<App>>,
    Path((tenant, table, id)): Path<(String, String, i64)>,
    UrlQuery(Expected { version }): UrlQuery<Expected>,
) -> Api<Value> {
    with_tenant(app, tenant, Missing::Skip, move |db, _| {
        db.delete(&table, id, version)?;
        Ok(json!({"deleted": true}))
    })
    .await
}
