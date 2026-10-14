//! Primitives shared by every hasura-rs crate.
//!
//! This crate is IO-free: it holds the error type (with Hasura V2 error codes), session and
//! role types, table names, and the backend-agnostic database catalog model that backends
//! produce by introspection and the schema builder consumes.

pub mod catalog;
pub mod error;
pub mod hash_order;
pub mod inconsistency;
pub mod names;
pub mod scientific;
pub mod session;

pub use catalog::{
    ColumnInfo, ColumnType, EnumTableRow, ForeignKey, FunctionArg, FunctionCatalog, NamedType,
    NativeQueryCheck, ResultColumn, SourceCatalog, TableCatalog, TableKind, TableMutability,
    TypeKind, UniqueConstraint, Volatility,
};
pub use error::{Code, Error, Result};
pub use hash_order::sort_v2_hash_order;
pub use inconsistency::InconsistentObject;
pub use names::{QualifiedFunction, QualifiedTable};
pub use scientific::scientific_show;
pub use session::{ADMIN_ROLE, RoleName, Session, SessionVariables};
