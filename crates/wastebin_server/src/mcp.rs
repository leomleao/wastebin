//! MCP server for agents, mounted at `/mcp` unless disabled with `WASTEBIN_MCP=false`.

use axum::Router;

use crate::AppState;

/// Routes of the MCP server. Merged into the app outside the timeout and compression layers,
/// and only when MCP is enabled.
pub(crate) fn router(state: AppState) -> Router {
    drop(state);
    Router::new()
}
