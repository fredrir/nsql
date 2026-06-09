//! Database backends. Each exposes `run(target_or_profile, sql, all) ->
//! Result<QueryResult>`, keeping the editor loop and rendering engine-agnostic.
//! Adding an engine = a new module here + an arm in `crate::db::run`.

pub mod postgres;
pub mod sqlite;
