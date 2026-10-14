//! TODO(module docs)

use serde_json::Value;

use crate::ctx::Ctx;
use crate::response::ApiResult;
use crate::runtime::Runtime;

/// Commands this module implements (`pg_` prefix stripped for backend commands).
pub const COMMANDS: &[&str] = &[];

pub async fn run<R: Runtime>(
    _ctx: &mut Ctx<'_, R>,
    command: &str,
    _args: &Value,
) -> ApiResult<Value> {
    Err(super::not_implemented(command))
}
