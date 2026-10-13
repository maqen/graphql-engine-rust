//! The interface between the engine and a database backend.

use std::future::Future;
use std::pin::Pin;

use hge_core::{Code, Error, Session};

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

    /// Build the multiplexed form of a subscription's root fields (all on this source): one
    /// statement serving many cohorts, where each cohort supplies its own values (see
    /// [`MultiplexedQuery`]). Backends without subscriptions keep the default.
    fn multiplexed_query(
        &self,
        fields: &[&QueryRoot],
        session: &Session,
    ) -> Result<MultiplexedQuery, Error> {
        let _ = (fields, session);
        Err(Error::new(
            Code::NotSupported,
            "subscriptions are not supported on this source",
        ))
    }

    /// Run a [`MultiplexedQuery`] for a batch of cohorts. `cohorts` is a JSON array holding
    /// each cohort's `variables` array. Returns one row per cohort.
    fn execute_multiplexed<'a>(
        &'a self,
        sql: &'a str,
        cohorts: &'a str,
    ) -> BoxFuture<'a, Result<Vec<MultiplexedRow>, Error>> {
        let _ = (sql, cohorts);
        Box::pin(async {
            Err(Error::new(
                Code::NotSupported,
                "subscriptions are not supported on this source",
            ))
        })
    }
}

/// A statement whose text depends only on the shape of a subscription; every value
/// (arguments, variables, session variables, stream cursor) is read from the cohort's
/// variables. Subscribers whose `sql` is equal can share one poller; those whose `variables`
/// are equal too share one cohort (and one result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiplexedQuery {
    pub sql: String,
    /// The values of this subscriber, in text form (`None` is SQL `NULL`).
    pub variables: Vec<Option<String>>,
    /// Streaming subscriptions: the index in `variables` of the cursor value, which advances
    /// after every non-empty batch. Each row then has one extra trailing column: the new
    /// cursor value (`None` when the batch was empty).
    pub cursor: Option<usize>,
}

/// The result for one cohort of a multiplexed batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiplexedRow {
    /// Position of the cohort in the batch.
    pub index: usize,
    /// One JSON document per root field (`None` is `null`), then the cursor for streams.
    pub columns: Vec<Option<String>>,
}
