//! MCP server implementation with tool handlers
//!
//! Implements the `ServerHandler` trait and registers 10 MCP tools. Handles
//! input validation, business logic orchestration, and response formatting.

use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    AccountInfo, AccountOnlyInput, CopyMessageInput, DeleteMessageInput, GetMessageInput,
    GetMessageRawInput, MailboxInfo, MessageDetail, MessageSummary, Meta, MoveMessageInput,
    SearchMessagesInput, ToolEnvelope, UpdateMessageFlagsInput,
};
use crate::pagination::{CursorEntry, CursorStore};

/// Maximum messages per search result page
const MAX_SEARCH_LIMIT: usize = 50;
/// Maximum attachments to return per message
const MAX_ATTACHMENTS: usize = 50;
/// Maximum UID search results stored in a cursor snapshot
const MAX_CURSOR_UIDS_STORED: usize = 20_000;

/// IMAP MCP server
///
/// Holds shared configuration and cursor store. Implements MCP tool handlers via
/// `#[tool]` attribute macro and `ServerHandler` trait.
#[derive(Clone)]
pub struct MailImapServer {
    /// Server config (accounts, timeouts, write flag)
    config: Arc<ServerConfig>,
    /// Cursor store for search pagination (protected by mutex)
    cursors: Arc<Mutex<CursorStore>>,
    /// Tool router for dispatching MCP tool calls
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MailImapServer {
    /// Create a new MCP server instance
    ///
    /// Initializes cursor store with configured TTL and max entries.
    pub fn new(config: ServerConfig) -> Self {
        let cursor_store = CursorStore::new(config.cursor_ttl_seconds, config.cursor_max_entries);
        Self {
            config: Arc::new(config),
            cursors: Arc::new(Mutex::new(cursor_store)),
            tool_router: Self::tool_router(),
        }
    }

    /// Tool: List configured IMAP accounts
    ///
    /// Returns account metadata (host, port, secure) without exposing
    /// credentials.
    #[tool(
        name = "imap_list_accounts",
        description = "List configured IMAP accounts"
    )]
    async fn list_accounts(&self) -> Result<Json<ToolEnvelope<Vec<AccountInfo>>>, ErrorData> {
        let started = Instant::now();
        let data = self
            .config
            .accounts
            .values()
            .map(|a| AccountInfo {
                account_id: a.account_id.clone(),
                host: a.host.clone(),
                port: a.port,
                secure: a.secure,
            })
            .collect::<Vec<_>>();
        finalize_tool(
            started,
            Ok((format!("{} account(s) configured", data.len()), data)),
        )
    }

    /// Tool: Verify account connectivity and capabilities
    ///
    /// Tests TCP/TLS connection, authentication, and retrieves server
    /// capabilities list.
    #[tool(
        name = "imap_verify_account",
        description = "Verify account connectivity and capabilities"
    )]
    async fn verify_account(
        &self,
        Parameters(input): Parameters<AccountOnlyInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.verify_account_impl(input)
                .await
                .map(|data| ("Account verification succeeded".to_owned(), data)),
        )
    }

    /// Tool: List mailboxes for an account
    ///
    /// Returns up to 200 visible mailboxes/folders.
    #[tool(
        name = "imap_list_mailboxes",
        description = "List mailboxes for an account"
    )]
    async fn list_mailboxes(
        &self,
        Parameters(input): Parameters<AccountOnlyInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.list_mailboxes_impl(input).await.map(|data| {
                (
                    format!(
                        "{} mailbox(es)",
                        data["mailboxes"].as_array().map_or(0, Vec::len)
                    ),
                    data,
                )
            }),
        )
    }

    /// Tool: Search messages with cursor pagination
    ///
    /// Supports multiple search criteria (query, from, to, subject, date
    /// ranges, unread filter). Returns cursors for efficient pagination
    /// across large result sets.
    #[tool(
        name = "imap_search_messages",
        description = "Search messages with cursor pagination"
    )]
    async fn search_messages(
        &self,
        Parameters(input): Parameters<SearchMessagesInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let result = self.search_messages_impl(input).await.and_then(|data| {
            let summary = format!("{} message(s) returned", data.messages.len());
            let serialized = serde_json::to_value(data)
                .map_err(|e| AppError::Internal(format!("serialization failure: {e}")))?;
            Ok((summary, serialized))
        });
        finalize_tool(started, result)
    }

    /// Tool: Get parsed message details
    ///
    /// Returns structured message data with headers, body text/HTML, and
    /// attachments. Supports bounded enrichment (char limits, optional HTML).
    #[tool(name = "imap_get_message", description = "Get parsed message details")]
    async fn get_message(
        &self,
        Parameters(input): Parameters<GetMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.get_message_impl(input)
                .await
                .map(|data| ("Message retrieved".to_owned(), data)),
        )
    }

    /// Tool: Get bounded RFC822 message source
    ///
    /// Returns raw RFC822 bytes (as string) up to `max_bytes`. Useful for
    /// diagnostics or tools that need full message source.
    #[tool(
        name = "imap_get_message_raw",
        description = "Get bounded RFC822 source"
    )]
    async fn get_message_raw(
        &self,
        Parameters(input): Parameters<GetMessageRawInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.get_message_raw_impl(input)
                .await
                .map(|data| ("Raw message retrieved".to_owned(), data)),
        )
    }

    /// Tool: Add or remove IMAP flags
    ///
    /// Modifies message flags (e.g., `\Seen`, `\Flagged`, `\Draft`,
    /// custom flags). Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(
        name = "imap_update_message_flags",
        description = "Add or remove IMAP flags"
    )]
    async fn update_message_flags(
        &self,
        Parameters(input): Parameters<UpdateMessageFlagsInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.update_flags_impl(input)
                .await
                .map(|data| ("Flags updated".to_owned(), data)),
        )
    }

    /// Tool: Copy message to mailbox
    ///
    /// Copies message to same or different account. Cross-account copy uses
    /// `APPEND`. Requires `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_copy_message", description = "Copy a message to mailbox")]
    async fn copy_message(
        &self,
        Parameters(input): Parameters<CopyMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.copy_message_impl(input)
                .await
                .map(|data| ("Message copied".to_owned(), data)),
        )
    }

    /// Tool: Move message to mailbox
    ///
    /// Moves message within same account. Prefers `MOVE` capability,
    /// falls back to `COPY` + `DELETE`. Requires
    /// `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_move_message", description = "Move a message to mailbox")]
    async fn move_message(
        &self,
        Parameters(input): Parameters<MoveMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.move_message_impl(input)
                .await
                .map(|data| ("Message moved".to_owned(), data)),
        )
    }

    /// Tool: Delete message from mailbox
    ///
    /// Marks message as `\Deleted` and immediately expunges. Requires
    /// explicit `confirm=true` and `MAIL_IMAP_WRITE_ENABLED=true`.
    #[tool(name = "imap_delete_message", description = "Delete a message")]
    async fn delete_message(
        &self,
        Parameters(input): Parameters<DeleteMessageInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            self.delete_message_impl(input)
                .await
                .map(|data| ("Message deleted".to_owned(), data)),
        )
    }
}

/// MCP server handler implementation
///
/// Provides server info and capabilities to MCP client.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for MailImapServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Secure IMAP MCP server. Read operations are enabled by default; write tools require MAIL_IMAP_WRITE_ENABLED=true.".to_owned(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Search result data structure
#[derive(Debug, serde::Serialize)]
struct SearchResultData {
    account_id: String,
    mailbox: String,
    total: usize,
    messages: Vec<MessageSummary>,
    next_cursor: Option<String>,
    has_more: bool,
}

/// Tool implementation methods
///
/// Private methods handle the actual business logic for each tool, separated
/// from the public `#[tool]` methods that handle response formatting.
impl MailImapServer {
    async fn verify_account_impl(&self, input: AccountOnlyInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;

        let started = Instant::now();
        imap::noop(&self.config, &mut session).await?;
        let caps = imap::capabilities(&self.config, &mut session).await?;

        let mut capabilities = caps.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>();
        capabilities.sort();
        capabilities.truncate(256);

        Ok(serde_json::json!({
            "account_id": account.account_id,
            "ok": true,
            "latency_ms": duration_ms(started),
            "server": { "host": account.host, "port": account.port, "secure": account.secure },
            "capabilities": capabilities
        }))
    }

    async fn list_mailboxes_impl(&self, input: AccountOnlyInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;

        let items = imap::list_all_mailboxes(&self.config, &mut session).await?;

        let mailboxes = items
            .into_iter()
            .take(200)
            .map(|item| MailboxInfo {
                name: item.name().to_owned(),
                delimiter: item.delimiter().map(|d| d.to_string()),
            })
            .collect::<Vec<_>>();

        Ok(serde_json::json!({
            "account_id": account.account_id,
            "mailboxes": mailboxes,
        }))
    }

    async fn search_messages_impl(
        &self,
        input: SearchMessagesInput,
    ) -> AppResult<SearchResultData> {
        validate_search_input(&input)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &input.mailbox).await?;

        let snapshot = if let Some(cursor) = input.cursor.clone() {
            resume_cursor_search(&self.cursors, &input, uidvalidity, cursor).await?
        } else {
            start_new_search(&self.config, &mut session, &input).await?
        };

        let SearchSnapshot {
            uids_desc,
            offset,
            include_snippet,
            snippet_max_chars,
            cursor_id_from_request,
        } = snapshot;

        let total = uids_desc.len();
        if offset > total {
            return Err(AppError::InvalidInput(
                "cursor offset is out of range".to_owned(),
            ));
        }

        let limit = input.limit.clamp(1, MAX_SEARCH_LIMIT);
        let page_uids = uids_desc
            .iter()
            .skip(offset)
            .take(limit)
            .copied()
            .collect::<Vec<_>>();

        let messages = build_message_summaries(
            &self.config,
            &mut session,
            &page_uids,
            SummaryBuildOptions {
                account_id: &input.account_id,
                mailbox: &input.mailbox,
                uidvalidity,
                include_snippet,
                snippet_max_chars,
            },
        )
        .await?;

        let next_offset = offset + messages.len();
        let has_more = next_offset < total;
        let next_cursor = if has_more {
            let mut store = self.cursors.lock().await;
            if let Some(existing) = cursor_id_from_request {
                store.update_offset(&existing, next_offset);
                Some(existing)
            } else {
                let id = store.create(CursorEntry {
                    account_id: input.account_id.clone(),
                    mailbox: input.mailbox.clone(),
                    uidvalidity,
                    uids_desc,
                    offset: next_offset,
                    include_snippet,
                    snippet_max_chars,
                    expires_at: Instant::now(),
                });
                Some(id)
            }
        } else {
            if let Some(existing) = cursor_id_from_request {
                let mut store = self.cursors.lock().await;
                store.delete(&existing);
            }
            None
        };

        Ok(SearchResultData {
            account_id: input.account_id,
            mailbox: input.mailbox,
            total,
            messages,
            next_cursor: next_cursor.clone(),
            has_more: next_cursor.is_some(),
        })
    }

    async fn get_message_impl(&self, input: GetMessageInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        validate_chars(input.body_max_chars, 100, 20_000, "body_max_chars")?;
        let attachment_text_max_chars = input.attachment_text_max_chars.unwrap_or(10_000);
        if input.attachment_text_max_chars.is_some() && !input.extract_attachment_text {
            return Err(AppError::InvalidInput(
                "attachment_text_max_chars requires extract_attachment_text=true".to_owned(),
            ));
        }
        validate_chars(
            attachment_text_max_chars,
            100,
            50_000,
            "attachment_text_max_chars",
        )?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        ensure_uidvalidity_matches_readonly(&self.config, &mut session, &msg_id).await?;

        let raw = imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await?;
        let parsed = mime::parse_message(
            &raw,
            input.body_max_chars,
            input.include_html,
            input.extract_attachment_text,
            attachment_text_max_chars,
        )?;
        let headers = if input.include_headers || input.include_all_headers {
            Some(mime::curated_headers(
                &parsed.headers_all,
                input.include_all_headers,
            ))
        } else {
            None
        };

        let flags = imap::fetch_flags(&self.config, &mut session, msg_id.uid).await?;
        let detail = MessageDetail {
            message_id: msg_id.encode(),
            message_uri: build_message_uri(
                &input.account_id,
                &msg_id.mailbox,
                msg_id.uidvalidity,
                msg_id.uid,
            ),
            message_raw_uri: build_message_raw_uri(
                &input.account_id,
                &msg_id.mailbox,
                msg_id.uidvalidity,
                msg_id.uid,
            ),
            mailbox: msg_id.mailbox.clone(),
            uidvalidity: msg_id.uidvalidity,
            uid: msg_id.uid,
            date: parsed.date,
            from: parsed.from,
            to: parsed.to,
            cc: parsed.cc,
            subject: parsed.subject,
            flags: Some(flags),
            headers,
            body_text: parsed.body_text,
            body_html: parsed.body_html_sanitized,
            attachments: Some(
                parsed
                    .attachments
                    .into_iter()
                    .take(MAX_ATTACHMENTS)
                    .collect(),
            ),
        };

        Ok(serde_json::json!({
            "account_id": input.account_id,
            "message": detail,
        }))
    }

    async fn get_message_raw_impl(
        &self,
        input: GetMessageRawInput,
    ) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        validate_chars(input.max_bytes, 1_024, 1_000_000, "max_bytes")?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        ensure_uidvalidity_matches_readonly(&self.config, &mut session, &msg_id).await?;

        let raw = imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await?;
        if raw.len() > input.max_bytes {
            return Err(AppError::InvalidInput(
                "message exceeds max_bytes; increase max_bytes".to_owned(),
            ));
        }

        Ok(serde_json::json!({
            "account_id": input.account_id,
            "message_id": msg_id.encode(),
            "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
            "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
            "size_bytes": raw.len(),
            "raw_source": String::from_utf8_lossy(&raw).to_string(),
        }))
    }

    async fn update_flags_impl(
        &self,
        input: UpdateMessageFlagsInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;

        let add_flags = input.add_flags.unwrap_or_default();
        let remove_flags = input.remove_flags.unwrap_or_default();
        if add_flags.is_empty() && remove_flags.is_empty() {
            return Err(AppError::InvalidInput(
                "at least one of add_flags/remove_flags is required".to_owned(),
            ));
        }
        validate_flags(&add_flags, "add_flags")?;
        validate_flags(&remove_flags, "remove_flags")?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;

        if !add_flags.is_empty() {
            imap::uid_store(
                &self.config,
                &mut session,
                msg_id.uid,
                format!("+FLAGS.SILENT ({})", add_flags.join(" ")).as_str(),
            )
            .await?;
        }
        if !remove_flags.is_empty() {
            imap::uid_store(
                &self.config,
                &mut session,
                msg_id.uid,
                format!("-FLAGS.SILENT ({})", remove_flags.join(" ")).as_str(),
            )
            .await?;
        }

        let flags = imap::fetch_flags(&self.config, &mut session, msg_id.uid).await?;
        Ok(serde_json::json!({
            "account_id": input.account_id,
            "message_id": msg_id.encode(),
            "flags": flags,
        }))
    }

    async fn copy_message_impl(&self, input: CopyMessageInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.destination_mailbox)?;
        let destination_account_id = input
            .destination_account_id
            .clone()
            .unwrap_or_else(|| input.account_id.clone());
        validate_account_id(&destination_account_id)?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;

        if destination_account_id == input.account_id {
            let account = self.config.get_account(&input.account_id)?;
            let mut session = imap::connect_authenticated(&self.config, account).await?;
            ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;
            imap::uid_copy(
                &self.config,
                &mut session,
                msg_id.uid,
                input.destination_mailbox.as_str(),
            )
            .await?;
        } else {
            let source = self.config.get_account(&input.account_id)?;
            let mut source_session = imap::connect_authenticated(&self.config, source).await?;
            ensure_uidvalidity_matches_readonly(&self.config, &mut source_session, &msg_id).await?;
            let raw =
                imap::fetch_raw_message(&self.config, &mut source_session, msg_id.uid).await?;

            let destination = self.config.get_account(&destination_account_id)?;
            let mut destination_session =
                imap::connect_authenticated(&self.config, destination).await?;
            imap::append(
                &self.config,
                &mut destination_session,
                input.destination_mailbox.as_str(),
                raw.as_slice(),
            )
            .await?;
        }

        Ok(serde_json::json!({
            "source_account_id": input.account_id,
            "destination_account_id": destination_account_id,
            "source_mailbox": msg_id.mailbox,
            "destination_mailbox": input.destination_mailbox,
            "message_id": msg_id.encode(),
            "new_message_id": serde_json::Value::Null,
        }))
    }

    async fn move_message_impl(&self, input: MoveMessageInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.destination_mailbox)?;

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;

        let caps = imap::capabilities(&self.config, &mut session).await?;
        if caps.has_str("MOVE") {
            imap::uid_move(
                &self.config,
                &mut session,
                msg_id.uid,
                input.destination_mailbox.as_str(),
            )
            .await?;
        } else {
            imap::uid_copy(
                &self.config,
                &mut session,
                msg_id.uid,
                input.destination_mailbox.as_str(),
            )
            .await?;
            imap::uid_store(
                &self.config,
                &mut session,
                msg_id.uid,
                "+FLAGS.SILENT (\\Deleted)",
            )
            .await?;
            imap::uid_expunge(&self.config, &mut session, msg_id.uid).await?;
        }

        Ok(serde_json::json!({
            "account_id": input.account_id,
            "source_mailbox": msg_id.mailbox,
            "destination_mailbox": input.destination_mailbox,
            "message_id": msg_id.encode(),
            "new_message_id": serde_json::Value::Null,
        }))
    }

    async fn delete_message_impl(&self, input: DeleteMessageInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        if !input.confirm {
            return Err(AppError::InvalidInput(
                "delete requires confirm=true".to_owned(),
            ));
        }

        let msg_id = parse_and_validate_message_id(&input.account_id, &input.message_id)?;

        let account = self.config.get_account(&input.account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        ensure_uidvalidity_matches_readwrite(&self.config, &mut session, &msg_id).await?;

        imap::uid_store(
            &self.config,
            &mut session,
            msg_id.uid,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await?;
        imap::uid_expunge(&self.config, &mut session, msg_id.uid).await?;

        Ok(serde_json::json!({
            "account_id": input.account_id,
            "mailbox": msg_id.mailbox,
            "message_id": msg_id.encode(),
        }))
    }
}

/// Calculate elapsed milliseconds
fn duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Build a standardized MCP tool response envelope from business logic output
fn finalize_tool<T>(
    started: Instant,
    result: AppResult<(String, T)>,
) -> Result<Json<ToolEnvelope<T>>, ErrorData>
where
    T: schemars::JsonSchema,
{
    match result {
        Ok((summary, data)) => Ok(Json(ToolEnvelope {
            summary,
            data,
            meta: Meta::now(duration_ms(started)),
        })),
        Err(e) => Err(e.to_error_data()),
    }
}

/// Parse message_id, validate mailbox, and enforce account_id match.
fn parse_and_validate_message_id(account_id: &str, message_id: &str) -> AppResult<MessageId> {
    let msg_id = MessageId::parse(message_id)?;
    validate_mailbox(&msg_id.mailbox)?;
    if msg_id.account_id != account_id {
        return Err(AppError::InvalidInput(
            "message_id account does not match account_id".to_owned(),
        ));
    }
    Ok(msg_id)
}

/// Select mailbox readonly and ensure uidvalidity still matches message_id.
async fn ensure_uidvalidity_matches_readonly(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    msg_id: &MessageId,
) -> AppResult<()> {
    let current_uidvalidity =
        imap::select_mailbox_readonly(config, session, &msg_id.mailbox).await?;
    if current_uidvalidity != msg_id.uidvalidity {
        return Err(AppError::Conflict(
            "message uidvalidity no longer matches mailbox".to_owned(),
        ));
    }
    Ok(())
}

/// Select mailbox readwrite and ensure uidvalidity still matches message_id.
async fn ensure_uidvalidity_matches_readwrite(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    msg_id: &MessageId,
) -> AppResult<()> {
    let current_uidvalidity =
        imap::select_mailbox_readwrite(config, session, &msg_id.mailbox).await?;
    if current_uidvalidity != msg_id.uidvalidity {
        return Err(AppError::Conflict(
            "message uidvalidity no longer matches mailbox".to_owned(),
        ));
    }
    Ok(())
}

struct SearchSnapshot {
    uids_desc: Vec<u32>,
    offset: usize,
    include_snippet: bool,
    snippet_max_chars: usize,
    cursor_id_from_request: Option<String>,
}

async fn resume_cursor_search(
    cursors: &Arc<Mutex<CursorStore>>,
    input: &SearchMessagesInput,
    uidvalidity: u32,
    cursor_id: String,
) -> AppResult<SearchSnapshot> {
    let mut store = cursors.lock().await;
    let entry = store
        .get(&cursor_id)
        .ok_or_else(|| AppError::InvalidInput("cursor is invalid or expired".to_owned()))?;
    if entry.account_id != input.account_id || entry.mailbox != input.mailbox {
        return Err(AppError::InvalidInput(
            "cursor does not match account/mailbox".to_owned(),
        ));
    }
    if entry.uidvalidity != uidvalidity {
        store.delete(&cursor_id);
        return Err(AppError::Conflict(
            "mailbox snapshot changed; rerun search".to_owned(),
        ));
    }
    Ok(SearchSnapshot {
        uids_desc: entry.uids_desc,
        offset: entry.offset,
        include_snippet: entry.include_snippet,
        snippet_max_chars: entry.snippet_max_chars,
        cursor_id_from_request: Some(cursor_id),
    })
}

async fn start_new_search(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    input: &SearchMessagesInput,
) -> AppResult<SearchSnapshot> {
    let query = build_search_query(input)?;
    let searched_uids = imap::uid_search(config, session, &query).await?;
    if searched_uids.len() > MAX_CURSOR_UIDS_STORED {
        return Err(AppError::InvalidInput(format!(
            "search matched {} messages; narrow filters to at most {} results",
            searched_uids.len(),
            MAX_CURSOR_UIDS_STORED
        )));
    }

    Ok(SearchSnapshot {
        uids_desc: searched_uids,
        offset: 0,
        include_snippet: input.include_snippet,
        snippet_max_chars: input.snippet_max_chars.unwrap_or(200).clamp(50, 500),
        cursor_id_from_request: None,
    })
}

async fn build_message_summaries(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    uids: &[u32],
    options: SummaryBuildOptions<'_>,
) -> AppResult<Vec<MessageSummary>> {
    let mut messages = Vec::with_capacity(uids.len());
    for uid in uids {
        let (header_bytes, flags) = imap::fetch_headers_and_flags(config, session, *uid).await?;
        let headers = mime::parse_header_bytes(&header_bytes)?;
        let date = header_value(&headers, "date");
        let from = header_value(&headers, "from");
        let subject = header_value(&headers, "subject");

        let snippet = if options.include_snippet {
            subject
                .clone()
                .map(|s| mime::truncate_chars(s, options.snippet_max_chars))
        } else {
            None
        };

        let message_id = MessageId {
            account_id: options.account_id.to_owned(),
            mailbox: options.mailbox.to_owned(),
            uidvalidity: options.uidvalidity,
            uid: *uid,
        }
        .encode();
        let message_uri = build_message_uri(
            options.account_id,
            options.mailbox,
            options.uidvalidity,
            *uid,
        );
        let message_raw_uri = build_message_raw_uri(
            options.account_id,
            options.mailbox,
            options.uidvalidity,
            *uid,
        );

        messages.push(MessageSummary {
            message_id,
            message_uri,
            message_raw_uri,
            mailbox: options.mailbox.to_owned(),
            uidvalidity: options.uidvalidity,
            uid: *uid,
            date,
            from,
            subject,
            flags: Some(flags),
            snippet,
        });
    }
    Ok(messages)
}

struct SummaryBuildOptions<'a> {
    account_id: &'a str,
    mailbox: &'a str,
    uidvalidity: u32,
    include_snippet: bool,
    snippet_max_chars: usize,
}

/// Validate account_id format
fn validate_account_id(account_id: &str) -> AppResult<()> {
    if account_id.is_empty() || account_id.len() > 64 {
        return Err(AppError::InvalidInput(
            "account_id must be 1..64 characters".to_owned(),
        ));
    }
    if !account_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(AppError::InvalidInput(
            "account_id must match [A-Za-z0-9_-]+".to_owned(),
        ));
    }
    Ok(())
}

/// Validate mailbox name format
fn validate_mailbox(mailbox: &str) -> AppResult<()> {
    if mailbox.is_empty() || mailbox.len() > 256 {
        return Err(AppError::InvalidInput(
            "mailbox must be 1..256 characters".to_owned(),
        ));
    }
    validate_no_controls(mailbox, "mailbox")?;
    Ok(())
}

/// Reject IMAP control characters in user-provided values
fn validate_no_controls(value: &str, field: &str) -> AppResult<()> {
    if value.chars().any(|ch| ch.is_ascii_control()) {
        return Err(AppError::InvalidInput(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Validate numeric value in range
fn validate_chars(value: usize, min: usize, max: usize, field: &str) -> AppResult<()> {
    if value < min || value > max {
        return Err(AppError::InvalidInput(format!(
            "{field} must be in range {min}..{max}"
        )));
    }
    Ok(())
}

/// Validate search messages input
fn validate_search_input(input: &SearchMessagesInput) -> AppResult<()> {
    validate_mailbox(&input.mailbox)?;
    validate_chars(input.limit, 1, 50, "limit")?;
    if let Some(v) = input.last_days
        && !(1..=365).contains(&v)
    {
        return Err(AppError::InvalidInput(
            "last_days must be in range 1..365".to_owned(),
        ));
    }
    if let Some(v) = input.snippet_max_chars {
        validate_chars(v, 50, 500, "snippet_max_chars")?;
        if !input.include_snippet {
            return Err(AppError::InvalidInput(
                "snippet_max_chars requires include_snippet=true".to_owned(),
            ));
        }
    }

    if let Some(v) = &input.query {
        validate_search_text(v)?;
    }
    if let Some(v) = &input.from {
        validate_search_text(v)?;
    }
    if let Some(v) = &input.to {
        validate_search_text(v)?;
    }
    if let Some(v) = &input.subject {
        validate_search_text(v)?;
    }

    let has_filters = input.query.is_some()
        || input.from.is_some()
        || input.to.is_some()
        || input.subject.is_some()
        || input.unread_only.is_some()
        || input.last_days.is_some()
        || input.start_date.is_some()
        || input.end_date.is_some();
    if input.cursor.is_some() && has_filters {
        return Err(AppError::InvalidInput(
            "cursor cannot be combined with search criteria".to_owned(),
        ));
    }

    if input.last_days.is_some() && (input.start_date.is_some() || input.end_date.is_some()) {
        return Err(AppError::InvalidInput(
            "last_days cannot be combined with start_date/end_date".to_owned(),
        ));
    }

    if let (Some(start), Some(end)) = (&input.start_date, &input.end_date) {
        let start_d = parse_ymd(start)?;
        let end_d = parse_ymd(end)?;
        if start_d > end_d {
            return Err(AppError::InvalidInput(
                "start_date must be <= end_date".to_owned(),
            ));
        }
    }

    Ok(())
}

/// Validate search text field bounds and characters
fn validate_search_text(input: &str) -> AppResult<()> {
    if input.is_empty() || input.len() > 256 {
        return Err(AppError::InvalidInput(
            "search text fields must be 1..256 chars".to_owned(),
        ));
    }
    validate_no_controls(input, "search text")
}

/// Build IMAP SEARCH query string from input
fn build_search_query(input: &SearchMessagesInput) -> AppResult<String> {
    let mut parts = Vec::new();
    if let Some(v) = &input.query {
        parts.push(format!("TEXT \"{}\"", escape_imap_quoted(v)?));
    }
    if let Some(v) = &input.from {
        parts.push(format!("FROM \"{}\"", escape_imap_quoted(v)?));
    }
    if let Some(v) = &input.to {
        parts.push(format!("TO \"{}\"", escape_imap_quoted(v)?));
    }
    if let Some(v) = &input.subject {
        parts.push(format!("SUBJECT \"{}\"", escape_imap_quoted(v)?));
    }
    if input.unread_only.unwrap_or(false) {
        parts.push("UNSEEN".to_owned());
    }
    if let Some(days) = input.last_days {
        let since = Utc::now().date_naive() - ChronoDuration::days(i64::from(days));
        parts.push(format!("SINCE {}", imap_date(since)));
    }
    if let Some(start) = &input.start_date {
        parts.push(format!("SINCE {}", imap_date(parse_ymd(start)?)));
    }
    if let Some(end) = &input.end_date {
        let end_exclusive = parse_ymd(end)? + ChronoDuration::days(1);
        parts.push(format!("BEFORE {}", imap_date(end_exclusive)));
    }

    if parts.is_empty() {
        Ok("ALL".to_owned())
    } else {
        Ok(parts.join(" "))
    }
}

/// Escape backslashes and quotes for IMAP quoted strings
fn escape_imap_quoted(input: &str) -> AppResult<String> {
    validate_search_text(input)?;
    Ok(input.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Validate and normalize IMAP flag atoms
fn validate_flags(flags: &[String], field: &str) -> AppResult<()> {
    for flag in flags {
        validate_flag(flag).map_err(|_| {
            AppError::InvalidInput(format!(
                "{field} contains invalid flag '{flag}'; flags must not contain whitespace, control chars, quotes, parentheses, or braces"
            ))
        })?;
    }
    Ok(())
}

fn validate_flag(flag: &str) -> AppResult<()> {
    if flag.is_empty() || flag.len() > 64 {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    let atom = if let Some(rest) = flag.strip_prefix('\\') {
        if rest.is_empty() {
            return Err(AppError::InvalidInput("invalid flag".to_owned()));
        }
        rest
    } else {
        flag
    };

    if atom.chars().any(|ch| {
        ch.is_ascii_control()
            || ch.is_ascii_whitespace()
            || matches!(ch, '"' | '(' | ')' | '{' | '}' | '\\')
    }) {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    Ok(())
}

/// Format date as IMAP SEARCH date (e.g., "1-Jan-2025")
fn imap_date(date: NaiveDate) -> String {
    date.format("%-d-%b-%Y").to_string()
}

/// Parse YYYY-MM-DD date string
fn parse_ymd(input: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .map_err(|_| AppError::InvalidInput(format!("invalid date '{input}', expected YYYY-MM-DD")))
}

/// Get header value by case-insensitive key
fn header_value(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .find_map(|(k, v)| k.eq_ignore_ascii_case(key).then(|| v.clone()))
}

/// Check if write operations are enabled
fn require_write_enabled(config: &ServerConfig) -> AppResult<()> {
    if !config.write_enabled {
        return Err(AppError::InvalidInput(
            "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true".to_owned(),
        ));
    }
    Ok(())
}

/// Build message URI for display
fn build_message_uri(account_id: &str, mailbox: &str, uidvalidity: u32, uid: u32) -> String {
    format!(
        "imap://{}/mailbox/{}/message/{}/{}",
        account_id,
        urlencoding::encode(mailbox),
        uidvalidity,
        uid
    )
}

/// Build raw message URI
fn build_message_raw_uri(account_id: &str, mailbox: &str, uidvalidity: u32, uid: u32) -> String {
    format!(
        "{}/raw",
        build_message_uri(account_id, mailbox, uidvalidity, uid)
    )
}

#[cfg(test)]
mod tests {
    use super::{escape_imap_quoted, validate_flag, validate_mailbox, validate_search_text};

    #[test]
    fn rejects_control_chars_in_search_text() {
        let err = validate_search_text("hello\nworld").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn rejects_control_chars_in_mailbox() {
        let err = validate_mailbox("INBOX\r").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn escape_rejects_linebreaks() {
        let err = escape_imap_quoted("a\nb").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn validate_flag_allows_common_flags() {
        validate_flag("\\Seen").expect("system flag must be valid");
        validate_flag("Important").expect("keyword flag must be valid");
        validate_flag("$MailFlagBit0").expect("keyword flag must be valid");
    }

    #[test]
    fn validate_flag_rejects_injection_like_value() {
        let err = validate_flag("\\Seen) UID FETCH 1:* (BODY[]").expect_err("must fail");
        assert!(err.to_string().contains("invalid flag"));
    }
}
