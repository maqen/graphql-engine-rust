//! The interface between the engine and a database backend.

use std::future::Future;
use std::pin::Pin;

use hge_core::{Error, Session};

use crate::{Mutation, QueryRoot};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Executes IR against one source.
pub trait SourceExecutor: Send + Sync {
    /// Run the root fields of one operation that target this source, returning one JSON
    /// document (as text) per field, in order. Implementations should use a single round trip
    /// where possible.
    fn execute_query<'a>(
        &'a self,
        fields: &'a [&'a QueryRoot],
        session: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>>;

    /// Run mutation root fields in order, in one transaction, returning one JSON document
    /// per field. Any error rolls back all of them.
    fn execute_mutations<'a>(
        &'a self,
        fields: &'a [&'a Mutation],
        session: &'a Session,
    ) -> BoxFuture<'a, Result<Vec<String>, Error>>;
}
