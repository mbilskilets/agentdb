//! A small document database built for AI agents.
//!
//! One encrypted file per tenant. Tables hold JSON documents that are checked
//! against a declared schema, and every error says how to fix the call.

mod ask;
mod change;
mod db;
mod error;
mod jev;
mod migrate;
mod query;
mod schema;
pub mod server;
mod write;

pub use ask::Asked;
pub use change::{Change, ChangeKind};
pub use db::{AgentDb, Doc, Page, TableInfo};
pub use error::{DbError, Result};
pub use jev::{Answer, Jev, Judge, Judgement, Question, Usage};
pub use migrate::SchemaChange;
pub use query::{Filter, Op, Query, Sort};
pub use schema::{Field, FieldType, TableDef};
pub use write::Write;
