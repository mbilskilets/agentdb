//! The tenant databases the server has open.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use axum::http::StatusCode;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tokio::sync::watch;

use super::error::ApiError;
use crate::AgentDb;

const MAX_TENANT_LEN: usize = 64;

/// What to do about a tenant that has no database file yet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Missing {
    /// Create the file. Only a schema change asks for this, because nothing
    /// else can succeed in a database without tables.
    Create,
    /// Leave the disk alone.
    Skip,
}

enum Slot {
    /// The file is being opened or closed. Whoever needs the tenant waits.
    Busy,
    Open {
        db: Arc<AgentDb>,
        last_used: Instant,
    },
}

enum Claimed<'a> {
    Open(Arc<AgentDb>),
    /// The tenant is not open, and it is the caller's to open.
    Ours(Busy<'a>),
}

/// Marks one tenant as being opened or closed. Dropping it lets everyone who
/// waits for that tenant carry on.
struct Busy<'a> {
    tenants: &'a Tenants,
    id: &'a str,
}

impl Busy<'_> {
    fn opened(self, db: &Arc<AgentDb>) -> Result<(), ApiError> {
        self.tenants.lock()?.insert(
            self.id.to_owned(),
            Slot::Open {
                db: Arc::clone(db),
                last_used: Instant::now(),
            },
        );
        Ok(())
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut slots = self
            .tenants
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(slots.get(self.id), Some(Slot::Busy)) {
            slots.remove(self.id);
        }
        drop(slots);
        self.tenants.settled.notify_all();
    }
}

/// The open tenant databases, at most `max_open` of them. Opening one more
/// closes the idle one that went unused the longest.
///
/// A tenant never has two handles at once: a subscriber hears only the
/// writes made through the handle it subscribed on. The lock on `slots` is
/// never held while a file is looked for, opened or closed, so one tenant's
/// slow disk does not hold up the others.
pub(super) struct Tenants {
    data_dir: PathBuf,
    master_key: String,
    max_open: usize,
    slots: Mutex<HashMap<String, Slot>>,
    settled: Condvar,
    opened: watch::Sender<()>,
}

impl Tenants {
    pub(super) fn new(data_dir: PathBuf, master_key: String, max_open: usize) -> Self {
        Self {
            data_dir,
            master_key,
            max_open,
            slots: Mutex::new(HashMap::new()),
            settled: Condvar::new(),
            opened: watch::Sender::new(()),
        }
    }

    /// Returns the tenant's database, opening its file if needed. `None`
    /// when the tenant has no file and `missing` says to leave it that way.
    pub(super) fn open(
        &self,
        id: &str,
        missing: Missing,
    ) -> Result<Option<Arc<AgentDb>>, ApiError> {
        check_id(id)?;
        let path = self.data_dir.join(format!("{id}.db"));
        if missing == Missing::Skip && !path.try_exists().map_err(ApiError::internal)? {
            return Ok(None);
        }
        let busy = match self.claim(id)? {
            Claimed::Open(db) => return Ok(Some(db)),
            Claimed::Ours(busy) => busy,
        };
        self.make_room()?;
        std::fs::create_dir_all(&self.data_dir).map_err(ApiError::internal)?;
        let db = Arc::new(AgentDb::open(path, &self.key(id)?)?);
        busy.opened(&db)?;
        self.opened.send_replace(());
        Ok(Some(db))
    }

    /// Changes whenever a tenant's database is opened. A subscriber that
    /// waits for a tenant without a file looks again then.
    pub(super) fn opened(&self) -> watch::Receiver<()> {
        self.opened.subscribe()
    }

    /// Closes every database that no request and no subscriber still holds.
    pub(super) fn close(&self) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let closing: Vec<_> = slots
            .extract_if(|_, slot| matches!(slot, Slot::Open { .. }))
            .collect();
        drop(slots);
        drop(closing);
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, Slot>>, ApiError> {
        self.slots.lock().map_err(ApiError::internal)
    }

    fn claim<'a>(&'a self, id: &'a str) -> Result<Claimed<'a>, ApiError> {
        let mut slots = self.lock()?;
        loop {
            match slots.get_mut(id) {
                Some(Slot::Open { db, last_used }) => {
                    *last_used = Instant::now();
                    return Ok(Claimed::Open(Arc::clone(db)));
                }
                Some(Slot::Busy) => {
                    slots = self.settled.wait(slots).map_err(ApiError::internal)?;
                }
                None => {
                    slots.insert(id.to_owned(), Slot::Busy);
                    return Ok(Claimed::Ours(Busy { tenants: self, id }));
                }
            }
        }
    }

    /// Closes the idle tenant that went unused the longest, if opening one
    /// more would exceed `max_open`. A tenant is idle when no request and no
    /// subscriber holds its database.
    fn make_room(&self) -> Result<(), ApiError> {
        let mut slots = self.lock()?;
        if slots.len() <= self.max_open {
            return Ok(());
        }
        let idle_longest = slots
            .iter()
            .filter_map(|(id, slot)| match slot {
                Slot::Open { db, last_used } if Arc::strong_count(db) == 1 => {
                    Some((*last_used, id.clone()))
                }
                _ => None,
            })
            .min();
        let Some((_, id)) = idle_longest else {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "too_many_open_tenants",
                format!(
                    "the server has {} tenant databases open and every one is in use by a request or a subscriber, so it cannot open another right now. Retry in a moment. If this keeps happening, the operator should raise AGENTDB_MAX_OPEN_TENANTS.",
                    self.max_open
                ),
            ));
        };
        let closing = slots.insert(id.clone(), Slot::Busy);
        drop(slots);
        let busy = Busy {
            tenants: self,
            id: &id,
        };
        drop(closing);
        drop(busy);
        Ok(())
    }

    /// A raw 256-bit key, unique per tenant. Raw keys skip the slow
    /// passphrase stretching, which a random key does not need.
    fn key(&self, id: &str) -> Result<String, ApiError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.master_key.as_bytes())
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

/// Tenant ids are lowercase only, so two ids can never name the same file
/// on a filesystem that ignores letter case.
fn check_id(id: &str) -> Result<(), ApiError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_TENANT_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_tenant",
            format!(
                "invalid tenant id `{id}`. Use 1 to {MAX_TENANT_LEN} lowercase letters, digits, `_` or `-`."
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{Missing, Tenants};

    #[test]
    fn requests_arriving_together_share_one_handle() {
        let dir = tempfile::tempdir().unwrap();
        let tenants = Tenants::new(dir.path().to_owned(), "master".to_owned(), 4);
        let handles: Vec<_> = thread::scope(|scope| {
            let opening: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| tenants.open("acme", Missing::Create).unwrap().unwrap()))
                .collect();
            opening
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });
        assert!(handles.iter().all(|db| Arc::ptr_eq(db, &handles[0])));
    }
}
