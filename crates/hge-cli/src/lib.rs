//! A Rust replacement for the `hasura` V2 command line tool, for projects served by this
//! engine. Project files, commands, flags, output and migration state follow hasura CLI
//! v2.51.0, so existing projects keep working unchanged.

pub mod client;
pub mod cmd;
pub mod config;
pub mod context;
pub mod error;
pub mod gofmt;
pub mod gojson;
pub mod json;
pub mod logger;
pub mod metadata;
pub mod migrate;
pub mod table;
pub mod yaml;
