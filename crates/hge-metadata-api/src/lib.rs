//! The metadata and schema APIs: `POST /v1/metadata`, `POST /v2/query`, and the V2-compatible
//! metadata catalog (`hdb_catalog`) they persist to.
//!
//! * [`catalog::PgCatalog`] — `hdb_catalog` creation/upgrade, metadata storage with
//!   optimistic `resource_version` locking, schema-sync notifications, CLI/console state.
//! * [`api::handle_metadata`] — `/v1/metadata` commands (`ops::*`).
//!
//! The server plugs in through [`runtime::Runtime`].

pub mod api;
pub mod catalog;
pub mod ctx;
pub mod ops;
pub mod response;
pub mod run_sql;
pub mod runtime;
pub mod schema_diff;

pub use api::handle_metadata;
pub use catalog::PgCatalog;
pub use response::{ApiError, ApiResponse};
pub use runtime::{ApiSettings, Installed, PrepareOptions, Reload, RemoteReload, Runtime};
