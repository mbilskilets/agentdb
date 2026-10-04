//! The change feed: every write to a tenant as a server-sent event, for as
//! long as the subscriber listens.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query as UrlQuery, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use super::error::ApiError;
use super::tenants::Missing;
use super::{App, Since, blocking};
use crate::{AgentDb, Change, DbError};

/// How many events may wait for a subscriber that reads slowly. Past that
/// the feed takes no more changes until the subscriber has caught up.
const SUBSCRIBER_BUFFER: usize = 64;

/// Streams changes as server-sent events, one per write, each with its
/// `seq` as the event id. With `?since=N` the stream starts after change
/// `N`; without it, with the next write.
///
/// Changes come from the change log until it runs out and live from then
/// on, in `seq` order with none missing and none repeated. A subscriber too
/// slow for the live writes is caught up from the log again. One that falls
/// further behind than the log reaches is sent a last event named `error`
/// with the `changes_trimmed` error, and the stream ends.
pub(super) async fn subscribe(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    UrlQuery(Since { since }): UrlQuery<Since>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let started = Feed::start(Arc::clone(&app), tenant.clone(), since).await?;
    let (events, stream) = mpsc::channel(SUBSCRIBER_BUFFER);
    tokio::spawn(async move {
        tokio::select! {
            () = run(&app, tenant, since, started, &events) => {}
            () = events.closed() => {}
            () = app.shutting_down() => {}
        }
    });
    Ok(Sse::new(ReceiverStream::new(stream).map(Ok)).keep_alive(KeepAlive::default()))
}

/// Feeds the subscriber until it leaves. A failure is the last event it is
/// sent.
async fn run(
    app: &Arc<App>,
    tenant: String,
    since: Option<i64>,
    started: Option<(Feed, CaughtUp)>,
    events: &mpsc::Sender<Event>,
) {
    let fed = async {
        let (feed, caught_up) = match started {
            Some(started) => started,
            None => Feed::start_once_created(app, tenant, since.unwrap_or(0)).await?,
        };
        feed.send_all(caught_up, events).await
    };
    if let Err(Stopped::Failed(error)) = fed.await
        && let Ok(last_event) = events.reserve().await
    {
        last_event.send(error.event());
    }
}

/// Why a feed stopped.
enum Stopped {
    Unsubscribed,
    Failed(ApiError),
}

impl From<SendError<Event>> for Stopped {
    fn from(_: SendError<Event>) -> Self {
        Self::Unsubscribed
    }
}

impl From<ApiError> for Stopped {
    fn from(error: ApiError) -> Self {
        Self::Failed(error)
    }
}

/// The next changes for a subscriber: a page of the change log, and the
/// live writes that follow it.
struct CaughtUp {
    live: broadcast::Receiver<Change>,
    page: Vec<Change>,
}

/// Subscribes to the live writes and only then reads the log after `last`,
/// so no write falls between the two. A write that shows up in both is told
/// apart by its `seq`.
fn catch_up(db: &AgentDb, last: i64) -> Result<CaughtUp, DbError> {
    let live = db.subscribe();
    let page = db.changes_since(last)?;
    Ok(CaughtUp { live, page })
}

/// One subscriber's place in a tenant's changes. Holding the database keeps
/// the server from closing it as idle.
struct Feed {
    db: Arc<AgentDb>,
    /// The `seq` of the newest change sent, or the one to start after.
    last: i64,
}

impl Feed {
    /// `None` while the tenant has no database.
    async fn start(
        app: Arc<App>,
        tenant: String,
        since: Option<i64>,
    ) -> Result<Option<(Self, CaughtUp)>, ApiError> {
        blocking(move || {
            let Some(db) = app.tenants.open(&tenant, Missing::Skip)? else {
                return Ok(None);
            };
            let last = match since {
                Some(seq) => seq,
                None => db.latest_seq()?,
            };
            let caught_up = catch_up(&db, last)?;
            Ok(Some((Self { db, last }, caught_up)))
        })
        .await
    }

    /// Waits for the schema change that creates the tenant's database.
    async fn start_once_created(
        app: &Arc<App>,
        tenant: String,
        since: i64,
    ) -> Result<(Self, CaughtUp), ApiError> {
        let mut created = app.tenants.created();
        loop {
            let started = Self::start(Arc::clone(app), tenant.clone(), Some(since)).await?;
            if let Some(started) = started {
                return Ok(started);
            }
            created.changed().await.map_err(ApiError::internal)?;
        }
    }

    /// Sends every change after `last`: from the log until it runs out,
    /// then live, and from the log again whenever the subscriber was too
    /// slow for the live writes.
    async fn send_all(
        mut self,
        mut caught_up: CaughtUp,
        events: &mpsc::Sender<Event>,
    ) -> Result<(), Stopped> {
        loop {
            let CaughtUp { mut live, page } = caught_up;
            let log_ran_out = page.is_empty();
            for change in page {
                self.send(&change, events).await?;
            }
            if log_ran_out {
                self.send_live(&mut live, events).await?;
            }
            let (db, last) = (Arc::clone(&self.db), self.last);
            caught_up = blocking(move || Ok(catch_up(&db, last)?)).await?;
        }
    }

    /// Sends live writes until the subscriber has fallen too far behind to
    /// be sent the next one.
    async fn send_live(
        &mut self,
        live: &mut broadcast::Receiver<Change>,
        events: &mpsc::Sender<Event>,
    ) -> Result<(), Stopped> {
        loop {
            match live.recv().await {
                Ok(change) if change.seq > self.last => self.send(&change, events).await?,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => return Ok(()),
                Err(closed @ RecvError::Closed) => return Err(ApiError::internal(closed).into()),
            }
        }
    }

    async fn send(&mut self, change: &Change, events: &mpsc::Sender<Event>) -> Result<(), Stopped> {
        let event = Event::default()
            .id(change.seq.to_string())
            .json_data(change)
            .map_err(ApiError::internal)?;
        events.send(event).await?;
        self.last = change.seq;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path as FilePath;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::BodyDataStream;
    use axum::extract::{Path, Query as UrlQuery, State};
    use axum::response::IntoResponse;
    use serde_json::{Value, json};
    use tokio::sync::watch;
    use tokio::task::spawn_blocking;
    use tokio_stream::StreamExt;

    use super::super::tenants::{Missing, Tenants};
    use super::super::{App, Since};
    use super::{SUBSCRIBER_BUFFER, subscribe};
    use crate::{AgentDb, FieldType, TableDef, Write};

    const RETAINED_CHANGES: usize = 10_000;
    const LIVE_BUFFER: usize = 1_024;

    /// A server's state, and the switch that would shut it down.
    fn app(dir: &FilePath) -> (Arc<App>, watch::Sender<bool>) {
        let (stop, stopping) = watch::channel(false);
        let app = App {
            secret: "secret".to_owned(),
            jev: None,
            tenants: Tenants::new(dir.to_owned(), "master".to_owned(), 4),
            stopping,
        };
        (Arc::new(app), stop)
    }

    fn notes(app: &App) -> Arc<AgentDb> {
        let db = app.tenants.open("acme", Missing::Create).unwrap().unwrap();
        db.define_table(&TableDef::new("notes").required("text", FieldType::Text))
            .unwrap();
        db
    }

    async fn write_notes(db: &Arc<AgentDb>, count: usize) {
        let db = Arc::clone(db);
        spawn_blocking(move || {
            let writes = vec![
                Write::Insert {
                    table: "notes".to_owned(),
                    doc: json!({"text": "hello"}),
                };
                count
            ];
            for batch in writes.chunks(500) {
                db.batch(batch.to_vec()).unwrap();
            }
        })
        .await
        .unwrap();
    }

    /// One server-sent event as its name and data. A keep-alive comment
    /// carries no data and is `None`.
    fn parse(event: &str) -> Option<(String, Value)> {
        let field = |name: &str| event.lines().find_map(|line| line.strip_prefix(name));
        let data = serde_json::from_str(field("data: ")?).unwrap();
        Some((field("event: ").unwrap_or("change").to_owned(), data))
    }

    /// The events of a change feed that nobody reads until asked to.
    struct Events {
        body: BodyDataStream,
        unread: String,
    }

    impl Events {
        async fn subscribe(app: &Arc<App>, since: i64) -> Self {
            let since = UrlQuery(Since { since: Some(since) });
            let feed = subscribe(State(Arc::clone(app)), Path("acme".to_owned()), since)
                .await
                .unwrap();
            Self {
                body: feed.into_response().into_body().into_data_stream(),
                unread: String::new(),
            }
        }

        /// The text of the next event, or `None` when the feed has ended.
        async fn next_block(&mut self) -> Option<String> {
            while !self.unread.contains("\n\n") {
                let chunk = self.body.next().await?.unwrap();
                self.unread.push_str(std::str::from_utf8(&chunk).unwrap());
            }
            let (event, rest) = self.unread.split_once("\n\n").unwrap();
            let event = event.to_owned();
            self.unread = rest.to_owned();
            Some(event)
        }

        /// The next event as its name and data, or `None` when the feed
        /// has ended.
        async fn next(&mut self) -> Option<(String, Value)> {
            let mut event = None;
            while event.is_none() {
                event = parse(&self.next_block().await?);
            }
            event
        }

        async fn next_seq(&mut self) -> i64 {
            let (name, change) = self.next().await.unwrap();
            assert_eq!(name, "change");
            change["seq"].as_i64().unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscriber_too_slow_for_live_writes_is_caught_up_from_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _stop) = app(dir.path());
        let db = notes(&app);
        let mut events = Events::subscribe(&app, 0).await;
        assert_eq!(events.next_seq().await, 1);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let written = SUBSCRIBER_BUFFER + 2 * LIVE_BUFFER;
        write_notes(&db, written).await;

        for expected in 2..=i64::try_from(written).unwrap() + 1 {
            assert_eq!(events.next_seq().await, expected);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscriber_behind_the_whole_log_is_told_and_the_feed_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _stop) = app(dir.path());
        let db = notes(&app);
        let mut events = Events::subscribe(&app, 0).await;
        assert_eq!(events.next_seq().await, 1);
        tokio::time::sleep(Duration::from_millis(100)).await;

        write_notes(&db, SUBSCRIBER_BUFFER + LIVE_BUFFER + RETAINED_CHANGES).await;

        let mut received = Vec::new();
        while let Some(event) = events.next().await {
            received.push(event);
        }
        let ((name, error), changes) = received.split_last().unwrap();
        let seqs = changes
            .iter()
            .map(|(_, change)| change["seq"].as_i64().unwrap());
        assert!(seqs.eq((2..).take(changes.len())));
        assert_eq!(name, "error");
        assert_eq!(error["error"]["code"], "changes_trimmed");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("cannot replay changes after seq "));
    }
}
