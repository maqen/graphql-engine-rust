//! Request policies derived from metadata and configuration: the allowlist.

use async_graphql_parser::types::ExecutableDocument;
use hge_core::{Error, Session};

use crate::state::AppState;

/// With `HASURA_GRAPHQL_ENABLE_ALLOWLIST`, a non-admin request must be one of the globally
/// allowlisted queries (V2's `checkQueryInAllowlist`, run right after parsing).
pub fn check_allowlist(
    state: &AppState,
    session: &Session,
    doc: &ExecutableDocument,
    text: &str,
) -> Result<(), Error> {
    match &state.prepared.allowlist {
        Some(allowlist) if !session.role.is_admin() => allowlist.check(doc, text),
        _ => Ok(()),
    }
}
