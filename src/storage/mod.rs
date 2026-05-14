//! Persistence layer. All SQL + on-disk state lives here.
//!
//! Submodules are organized by domain (not by table) so callers see
//! `storage::soul`, `storage::messages`, etc. rather than a flat namespace.
//!
//! `storage::db` is the only submodule that knows about pool construction,
//! migrations, and the shared `now_ts()` helper — every other submodule
//! takes a `&SqlitePool` from a caller that got one through `db::init_pool`.

pub mod conversations;
pub mod db;
pub mod facts;
pub mod messages;
pub mod soul;
pub mod summaries;
pub mod users;
