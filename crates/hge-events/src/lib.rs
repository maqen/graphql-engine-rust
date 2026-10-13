//! Event triggers, cron triggers and one-off scheduled events, compatible with Hasura V2.
//!
//! * [`ddl`] / [`catalog`]: the `hdb_catalog` source catalog and per-trigger SQL functions,
//!   byte-compatible with what V2 installs, so a database can move between V2 and this engine.
//! * [`processor`]: the delivery loop (row locking, batching, retries, invocation logs).
//! * [`delivery`] / [`webhook`]: one webhook attempt and its V2 bookkeeping.
//! * [`json`]: aeson-compatible JSON encoding for bodies and logs.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod cron;
pub mod ddl;
pub mod delivery;
pub mod json;
pub mod model;
pub mod processor;
pub mod scheduled;
pub mod store;
pub mod triggers;
pub mod webhook;

pub use model::{EventTriggerConf, Op};
pub use processor::{EventingConfig, TriggersProvider};
pub use triggers::{EventTriggerInfo, EventTriggers, SourceTriggers};
