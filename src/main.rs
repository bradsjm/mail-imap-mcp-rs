//! mail-imap-mcp-rs: Secure IMAP MCP server over stdio
//!
//! This server provides read/write access to IMAP mailboxes via the Model
//! Context Protocol (MCP) over stdio. It features cursor-based pagination,
//! TLS-only connections, and security-first design.
//!
//! # Architecture
//!
//! - [`main`]: Process entry point with env loading and stdio serving
//! - [`config`]: Environment-driven configuration for accounts and server settings
//! - [`errors`]: Application error model with MCP error mapping
//! - [`imap`]: IMAP transport/session operations with timeout wrappers
//! - [`server`]: MCP tool handlers with validation and business orchestration
//! - [`models`]: Input/output DTOs and schema-bearing types
//! - [`mime`]: Message parsing, header/body extraction, and sanitization
//! - [`message_id`]: Stable, opaque message ID parse/encode logic
//! - [`pagination`]: Cursor storage with TTL and eviction behavior

mod config;
mod errors;
mod imap;
mod message_id;
mod mime;
mod models;
mod pagination;
mod server;

use config::ServerConfig;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

/// Application entry point
///
/// Initializes tracing from environment, loads config, and serves the MCP
/// server over stdio. This process expects to be spawned by an MCP client
/// via `stdio` transport.
///
/// # Environment Variables
///
/// See [`ServerConfig::load_from_env`] for full configuration options.
///
/// # Example
///
/// ```no_run
/// MAIL_IMAP_DEFAULT_HOST=imap.example.com \
/// MAIL_IMAP_DEFAULT_USER=user@example.com \
/// MAIL_IMAP_DEFAULT_PASS=secret \
/// cargo run
/// ```
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let config = ServerConfig::load_from_env()?;
    let service = server::MailImapServer::new(config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
