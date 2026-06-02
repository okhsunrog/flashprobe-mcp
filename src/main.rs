//! MCP server for embedded flashing/monitoring. Currently espflash-backed
//! (serial); probe-rs is added in a later milestone.

mod backend;
mod capture;
mod detect;
mod esp_noise;
mod inputs;
mod server;
mod tools;

use anyhow::Result;
use rmcp::{ServiceExt, transport::io::stdio};
use server::EspflashServer;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    info!("Starting espflash MCP server");

    let server = EspflashServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
