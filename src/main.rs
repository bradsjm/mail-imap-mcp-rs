//! mail-imap-mcp-rs: Secure IMAP MCP server with stdio and optional HTTP transport
//!
//! This server provides read/write access to IMAP mailboxes via the Model
//! Context Protocol (MCP). It features cursor-based pagination, TLS-only IMAP
//! connections, and security-first design.
//!
//! # Architecture
//!
//! - [`main`]: Process entry point with env loading and transport selection
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
mod mailbox_codec;
mod message_id;
mod mime;
mod models;
mod pagination;
mod server;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::IpAddr;

use axum::Router;
use clap::{CommandFactory, Parser, ValueEnum, error::ErrorKind};
use rmcp::ServiceExt;
use rmcp::transport::{
    stdio,
    streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

use config::ServerConfig;

const DEFAULT_HTTP_BIND_ADDRESS: &str = "127.0.0.1";
const DEFAULT_HTTP_PORT: u16 = 8000;
const MCP_HTTP_PATH: &str = "/mcp";

/// Application CLI arguments.
#[derive(Clone, Debug, Eq, PartialEq, Parser)]
#[command(
    name = "mail-imap-mcp-rs",
    version,
    about = "Secure IMAP MCP server with stdio or optional streamable HTTP transport"
)]
struct CliArgs {
    /// Transport mode for the MCP server.
    #[arg(long, value_enum, default_value_t = TransportMode::Stdio)]
    transport: TransportMode,
    /// HTTP bind address for --transport http. Defaults to localhost only.
    #[arg(long, default_value = DEFAULT_HTTP_BIND_ADDRESS)]
    http_bind_address: String,
    /// HTTP port for --transport http.
    #[arg(long, default_value_t = DEFAULT_HTTP_PORT)]
    http_port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TransportMode {
    Stdio,
    Http,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HttpBindPolicy {
    Loopback,
    SpecificHost(String),
    Wildcard,
}

/// Application entry point
///
/// Initializes tracing from environment, loads config, and serves the MCP
/// server over stdio by default or streamable HTTP when requested explicitly.
///
/// # Environment Variables
///
/// See [`ServerConfig::load_from_env`] for full configuration options.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let args = match CliArgs::try_parse() {
        Ok(args) => args,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            print_help_output()?;
            return Ok(());
        }
        Err(error) => error.exit(),
    };

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    let config = ServerConfig::load_from_env()?;
    match args.transport {
        TransportMode::Stdio => serve_stdio(config).await?,
        TransportMode::Http => serve_http(config, &args).await?,
    }

    Ok(())
}

async fn serve_stdio(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("starting MCP server transport=stdio");
    let service = server::MailImapServer::new(config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn serve_http(
    config: ServerConfig,
    args: &CliArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown_token = CancellationToken::new();
    let (http_config, bind_policy) =
        build_http_transport_config(&args.http_bind_address, shutdown_token.child_token());
    log_http_bind_policy(&bind_policy, &args.http_bind_address, args.http_port);

    let router = build_http_router(config, http_config);
    let listener =
        tokio::net::TcpListener::bind((args.http_bind_address.as_str(), args.http_port)).await?;
    let local_addr = listener.local_addr()?;

    tracing::info!(
        transport = "http",
        configured_bind_address = %args.http_bind_address,
        configured_port = args.http_port,
        listener_address = %local_addr,
        listener_port = local_addr.port(),
        endpoint = %format!("http://{local_addr}{MCP_HTTP_PATH}"),
        "HTTP MCP server is running and waiting for connections"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::warn!("HTTP shutdown signal listener failed: {error}");
            }
            shutdown_token.cancel();
        })
        .await?;

    Ok(())
}

fn build_http_router(config: ServerConfig, http_config: StreamableHttpServerConfig) -> Router {
    let service_config = config.clone();
    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(server::MailImapServer::new(service_config.clone())),
        LocalSessionManager::default().into(),
        http_config,
    );

    Router::new().nest_service(MCP_HTTP_PATH, service)
}

fn build_http_transport_config(
    bind_address: &str,
    cancellation_token: CancellationToken,
) -> (StreamableHttpServerConfig, HttpBindPolicy) {
    let bind_policy = classify_http_bind_address(bind_address);
    let config = StreamableHttpServerConfig::default().with_cancellation_token(cancellation_token);

    (config, bind_policy)
}

fn classify_http_bind_address(bind_address: &str) -> HttpBindPolicy {
    if bind_address.eq_ignore_ascii_case("localhost") {
        return HttpBindPolicy::Loopback;
    }

    match bind_address.parse::<IpAddr>() {
        Ok(ip) if ip.is_loopback() => HttpBindPolicy::Loopback,
        Ok(ip) if ip.is_unspecified() => HttpBindPolicy::Wildcard,
        Ok(ip) => HttpBindPolicy::SpecificHost(ip.to_string()),
        Err(_) => HttpBindPolicy::SpecificHost(bind_address.to_owned()),
    }
}

fn log_http_bind_policy(bind_policy: &HttpBindPolicy, bind_address: &str, port: u16) {
    match bind_policy {
        HttpBindPolicy::Loopback => tracing::info!(
            bind_address = bind_address,
            port,
            "HTTP transport is limited to localhost by default"
        ),
        HttpBindPolicy::SpecificHost(host) => tracing::warn!(
            bind_address = bind_address,
            host = host,
            port,
            "HTTP transport is not loopback-only; do not leave this server publicly reachable"
        ),
        HttpBindPolicy::Wildcard => tracing::warn!(
            bind_address = bind_address,
            port,
            "HTTP transport is bound to all interfaces; do not leave this server publicly reachable"
        ),
    }
}

fn print_help_output() -> io::Result<()> {
    let env_map: BTreeMap<String, String> = std::env::vars().collect();
    let output = build_help_output(&env_map);
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()
}

fn build_help_output(env_map: &BTreeMap<String, String>) -> String {
    let account_sections = discover_account_sections(env_map);
    let mut cmd = CliArgs::command();
    let mut help = Vec::new();
    let _ = cmd.write_long_help(&mut help);

    let mut out = String::from_utf8_lossy(&help).into_owned();
    out.push_str("\n\nIMAP environment setup\n");
    out.push_str("  Required per account section MAIL_IMAP_<ACCOUNT>_:\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_HOST\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_USER\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_PASS\n");
    out.push_str("  Optional per account section:\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_PORT (default: 993)\n");
    out.push_str("    MAIL_IMAP_<ACCOUNT>_SECURE (default: true)\n");
    out.push_str(
        "  If no account section is discovered from environment, DEFAULT is used by convention.\n\n",
    );

    out.push_str("Transport notes\n");
    out.push_str("  stdio is the default transport.\n");
    out.push_str(&format!(
        "  HTTP mode serves plain streamable HTTP on {MCP_HTTP_PATH}.\n"
    ));
    out.push_str(&format!(
        "  If not specified, HTTP binds to localhost only ({DEFAULT_HTTP_BIND_ADDRESS}:{DEFAULT_HTTP_PORT}).\n"
    ));
    out.push_str(
        "  Warning: do not leave the HTTP transport publicly available unless exposure is intentional and protected by a trusted boundary.\n\n",
    );

    out.push_str("Discovered account sections (from current environment)\n");
    if account_sections.is_empty() {
        out.push_str("  (none discovered)\n");
    } else {
        for section in &account_sections {
            out.push_str(&format!("  [{}]\n", section));
            for suffix in ["HOST", "USER", "PASS", "PORT", "SECURE"] {
                let key = format!("MAIL_IMAP_{}_{}", section, suffix);
                let value = env_map.get(&key).map(String::as_str);
                out.push_str(&format!("    {}={}\n", key, redact_value(&key, value)));
            }
        }
    }
    out.push('\n');

    out.push_str("Global policy defaults\n");
    out.push_str("  MAIL_IMAP_WRITE_ENABLED=false\n");
    out.push_str("  MAIL_IMAP_CA_CERT_PATH=<unset>\n");
    out.push_str("  MAIL_IMAP_CONNECT_TIMEOUT_MS=30000\n");
    out.push_str("  MAIL_IMAP_GREETING_TIMEOUT_MS=15000\n");
    out.push_str("  MAIL_IMAP_SOCKET_TIMEOUT_MS=300000\n");
    out.push_str("  MAIL_IMAP_CURSOR_TTL_SECONDS=600\n");
    out.push_str("  MAIL_IMAP_CURSOR_MAX_ENTRIES=512\n");
    out.push_str("  MAIL_IMAP_OPERATION_MAX_ENTRIES=256\n\n");

    out.push_str("Send/write gate policy\n");
    out.push_str(
        "  Read tools are enabled by default. Write-path tools are blocked unless MAIL_IMAP_WRITE_ENABLED=true.\n",
    );
    out.push_str(
        "  This gate protects against accidental mailbox mutations (copy, move, flag updates, delete).\n",
    );

    out
}

fn discover_account_sections(env_map: &BTreeMap<String, String>) -> Vec<String> {
    let mut sections: Vec<String> = env_map
        .keys()
        .filter_map(|key| {
            let remainder = key.strip_prefix("MAIL_IMAP_")?;
            for suffix in ["_HOST", "_USER", "_PASS", "_PORT", "_SECURE"] {
                if let Some(section) = remainder.strip_suffix(suffix)
                    && !section.is_empty()
                {
                    return Some(section.to_owned());
                }
            }
            None
        })
        .collect();

    sections.sort();
    sections.dedup();
    sections
}

fn redact_value(key: &str, value: Option<&str>) -> String {
    match value {
        Some(v) if is_secret_key(key) && !v.is_empty() => "<redacted>".to_owned(),
        Some("") => "<empty>".to_owned(),
        Some(v) => v.to_owned(),
        None => "<unset>".to_owned(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.contains("PASS") || key.contains("SECRET") || key.contains("TOKEN")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use clap::Parser;
    use rmcp::transport::StreamableHttpServerConfig;
    use secrecy::SecretString;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::config::{AccountConfig, ServerConfig};

    use super::{
        CliArgs, DEFAULT_HTTP_BIND_ADDRESS, DEFAULT_HTTP_PORT, HttpBindPolicy, MCP_HTTP_PATH,
        TransportMode, build_help_output, build_http_router, build_http_transport_config,
        classify_http_bind_address, discover_account_sections, is_secret_key, redact_value,
    };

    const HTTP_INITIALIZE_MAX_BYTES: usize = 1024 * 1024;

    fn parse_args<const N: usize>(args: [&str; N]) -> CliArgs {
        CliArgs::try_parse_from(args).expect("CLI args should parse")
    }

    fn test_server_config() -> ServerConfig {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            "default".to_owned(),
            AccountConfig {
                account_id: "default".to_owned(),
                host: "imap.example.com".to_owned(),
                port: 993,
                secure: true,
                user: "user@example.com".to_owned(),
                pass: SecretString::new("secret".to_owned().into()),
            },
        );

        ServerConfig {
            accounts,
            trusted_ca_certs: Vec::new(),
            write_enabled: false,
            connect_timeout_ms: 30_000,
            greeting_timeout_ms: 15_000,
            socket_timeout_ms: 300_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 512,
            read_session_cache_ttl_seconds: 120,
            read_session_cache_max_per_account: 4,
            operation_max_entries: 256,
        }
    }

    #[test]
    fn cli_defaults_to_stdio_and_localhost_http_settings() {
        let args = parse_args(["mail-imap-mcp-rs"]);
        assert_eq!(args.transport, TransportMode::Stdio);
        assert_eq!(args.http_bind_address, DEFAULT_HTTP_BIND_ADDRESS);
        assert_eq!(args.http_port, DEFAULT_HTTP_PORT);
    }

    #[test]
    fn cli_accepts_explicit_http_transport_settings() {
        let args = parse_args([
            "mail-imap-mcp-rs",
            "--transport",
            "http",
            "--http-bind-address",
            "0.0.0.0",
            "--http-port",
            "8123",
        ]);
        assert_eq!(args.transport, TransportMode::Http);
        assert_eq!(args.http_bind_address, "0.0.0.0");
        assert_eq!(args.http_port, 8123);
    }

    #[test]
    fn discovers_account_sections_from_env_like_keys() {
        let mut env_map = BTreeMap::new();
        env_map.insert(
            "MAIL_IMAP_DEFAULT_HOST".to_owned(),
            "imap.example.com".to_owned(),
        );
        env_map.insert(
            "MAIL_IMAP_WORK_USER".to_owned(),
            "work@example.com".to_owned(),
        );
        env_map.insert("MAIL_IMAP_WORK_PASS".to_owned(), "secret".to_owned());
        env_map.insert("MAIL_IMAP_WRITE_ENABLED".to_owned(), "true".to_owned());

        assert_eq!(
            discover_account_sections(&env_map),
            vec!["DEFAULT".to_owned(), "WORK".to_owned()]
        );
    }

    #[test]
    fn redacts_secret_values_and_marks_unset() {
        assert_eq!(
            redact_value("MAIL_IMAP_DEFAULT_PASS", Some("abc")),
            "<redacted>"
        );
        assert_eq!(redact_value("MAIL_IMAP_DEFAULT_HOST", Some("imap")), "imap");
        assert_eq!(redact_value("MAIL_IMAP_DEFAULT_USER", None), "<unset>");
    }

    #[test]
    fn detects_secret_keys_case_insensitively() {
        assert!(is_secret_key("mail_imap_default_pass"));
        assert!(is_secret_key("MAIL_IMAP_API_TOKEN"));
        assert!(!is_secret_key("MAIL_IMAP_DEFAULT_HOST"));
    }

    #[test]
    fn classifies_http_bind_addresses() {
        assert_eq!(
            classify_http_bind_address(DEFAULT_HTTP_BIND_ADDRESS),
            HttpBindPolicy::Loopback
        );
        assert_eq!(
            classify_http_bind_address("localhost"),
            HttpBindPolicy::Loopback
        );
        assert_eq!(
            classify_http_bind_address("192.168.1.10"),
            HttpBindPolicy::SpecificHost("192.168.1.10".to_owned())
        );
        assert_eq!(
            classify_http_bind_address("0.0.0.0"),
            HttpBindPolicy::Wildcard
        );
    }

    #[test]
    fn http_transport_config_keeps_loopback_defaults() {
        let (config, bind_policy) =
            build_http_transport_config(DEFAULT_HTTP_BIND_ADDRESS, CancellationToken::new());

        assert_eq!(bind_policy, HttpBindPolicy::Loopback);
        assert!(config.stateful_mode);
    }

    #[test]
    fn http_transport_config_keeps_defaults_for_explicit_non_loopback_host() {
        let (config, bind_policy) =
            build_http_transport_config("192.168.1.10", CancellationToken::new());

        assert_eq!(
            bind_policy,
            HttpBindPolicy::SpecificHost("192.168.1.10".to_owned())
        );
        assert!(config.stateful_mode);
    }

    #[test]
    fn http_transport_config_keeps_defaults_for_wildcard_bind() {
        let (config, bind_policy) =
            build_http_transport_config("0.0.0.0", CancellationToken::new());

        assert_eq!(bind_policy, HttpBindPolicy::Wildcard);
        assert!(config.stateful_mode);
    }

    #[test]
    fn help_output_includes_transport_flags_and_security_guidance() {
        let mut env_map = BTreeMap::new();
        env_map.insert(
            "MAIL_IMAP_DEFAULT_HOST".to_owned(),
            "imap.example.com".to_owned(),
        );
        env_map.insert(
            "MAIL_IMAP_DEFAULT_USER".to_owned(),
            "user@example.com".to_owned(),
        );
        env_map.insert("MAIL_IMAP_DEFAULT_PASS".to_owned(), "top-secret".to_owned());

        let help = build_help_output(&env_map);
        assert!(help.contains("--transport <TRANSPORT>"));
        assert!(help.contains("--http-bind-address <HTTP_BIND_ADDRESS>"));
        assert!(help.contains("--http-port <HTTP_PORT>"));
        assert!(help.contains("HTTP mode serves plain streamable HTTP on /mcp."));
        assert!(help.contains("do not leave the HTTP transport publicly available"));
        assert!(help.contains("MAIL_IMAP_DEFAULT_PASS=<redacted>"));
    }

    #[tokio::test]
    async fn http_router_serves_initialize_on_mcp_path_only() {
        let router = build_http_router(
            test_server_config(),
            StreamableHttpServerConfig::default().with_sse_keep_alive(None),
        );

        let initialize_request = Request::builder()
            .method("POST")
            .uri(MCP_HTTP_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json,text/event-stream")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            ))
            .expect("request should build");

        let response = router
            .clone()
            .oneshot(initialize_request)
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(content_type.contains("text/event-stream"));

        let body = to_bytes(response.into_body(), HTTP_INITIALIZE_MAX_BYTES)
            .await
            .expect("response body should be readable");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains(r#""jsonrpc":"2.0""#));
        assert!(body.contains(r#""id":1"#));

        let not_found = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
    }
}
