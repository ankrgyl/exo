mod protocol;
mod server;

use std::sync::Arc;

use anyhow::Result;

use crate::{AgentHarnessKind, Harness};

pub const AGENT_SERVER_TRACING_TARGET: &str = "executor::agent_server";

pub async fn serve_agent_server_stdio(
    harness: Arc<dyn Harness>,
    harness_kind: AgentHarnessKind,
) -> Result<()> {
    server::AgentServer::new(harness, harness_kind)
        .serve(tokio::io::stdin(), tokio::io::stdout())
        .await
}

#[cfg(test)]
mod tests;
