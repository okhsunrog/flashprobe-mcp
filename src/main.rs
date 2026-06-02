//! flashprobe-mcp: an MCP server for flashing and monitoring embedded targets
//! over two backends — probe-rs (JTAG/SWD + RTT) and espflash (UART).

mod backend;
mod capture;
mod detect;
mod esp_noise;
mod inputs;
mod server;
mod tools;

use anyhow::Result;
use rmcp::{ServiceExt, transport::io::stdio};
use server::Server;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    info!("Starting flashprobe MCP server");

    let server = Server::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
