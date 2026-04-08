//! MCP server implementation with tool handlers
//!
//! Implements the `ServerHandler` trait and registers 7 MCP tools. Handles
//! input validation, business logic orchestration, and response formatting.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;
use tracing::{error, warn};

use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::mailbox_codec::{decode_mailbox_name_for_display, normalize_mailbox_name};
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    AccountInfo, AccountOnlyInput, ApplyToMessagesInput, GetMessageInput, GetMessageRawInput,
    MailboxAction, MailboxInfo, ManageMailboxInput, MessageActionInput, MessageDetail,
    MessageSummary, Meta, SearchMessagesInput, SearchSelectorInput, ToolEnvelope,
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
    async fn list_accounts(&self) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        let accounts = self
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
        let next_account_id = accounts
            .first()
            .map(|a| a.account_id.clone())
            .unwrap_or_else(|| "default".to_owned());
        let data = serde_json::json!({
            "accounts": accounts,
            "next_action": next_action_list_mailboxes(&next_account_id),
        });
        finalize_tool(
            started,
            "imap_list_accounts",
            Ok((
                format!(
                    "{} account(s) configured",
                    self.config.accounts.values().len()
                ),
                data,
            )),
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
            "imap_list_mailboxes",
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
        finalize_tool(started, "imap_search_messages", result)
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
            "imap_get_message",
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
            "imap_get_message_raw",
            self.get_message_raw_impl(input)
                .await
                .map(|data| ("Raw message retrieved".to_owned(), data)),
        )
    }

    /// Tool: Apply one mutation to selected messages.
    #[tool(
        name = "imap_apply_to_messages",
        description = "Apply one mutation action to selected messages"
    )]
    async fn apply_to_messages(
        &self,
        Parameters(input): Parameters<ApplyToMessagesInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_apply_to_messages",
            self.apply_to_messages_impl(input).await.map(|data| {
                let matched = data["matched"].as_u64().unwrap_or(0);
                let action = data["action"].as_str().unwrap_or("mutation");
                (format!("{matched} message(s) processed for {action}"), data)
            }),
        )
    }

    /// Tool: Create, rename, or delete a mailbox.
    #[tool(
        name = "imap_manage_mailbox",
        description = "Create, rename, or delete a mailbox"
    )]
    async fn manage_mailbox(
        &self,
        Parameters(input): Parameters<ManageMailboxInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_manage_mailbox",
            self.manage_mailbox_impl(input).await.map(|data| {
                let action = data["action"].as_str().unwrap_or("manage");
                let mailbox = data["mailbox"].as_str().unwrap_or("mailbox");
                (format!("{action} completed for {mailbox}"), data)
            }),
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
    status: String,
    issues: Vec<ToolIssue>,
    next_action: NextAction,
    account_id: String,
    mailbox: String,
    total: usize,
    attempted: usize,
    returned: usize,
    failed: usize,
    messages: Vec<MessageSummary>,
    next_cursor: Option<String>,
    has_more: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct NextAction {
    instruction: String,
    tool: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ToolIssue {
    code: String,
    stage: String,
    message: String,
    retryable: bool,
    uid: Option<u32>,
    message_id: Option<String>,
}

impl ToolIssue {
    fn from_error(stage: &str, error: &AppError) -> Self {
        let (code, retryable) = match error {
            AppError::InvalidInput(_) => ("invalid_input", false),
            AppError::NotFound(_) => ("not_found", false),
            AppError::AuthFailed(_) => ("auth_failed", false),
            AppError::Timeout(_) => ("timeout", true),
            AppError::Conflict(_) => ("conflict", false),
            AppError::Internal(_) => ("internal", true),
        };
        Self {
            code: code.to_owned(),
            stage: stage.to_owned(),
            message: error.to_string(),
            retryable,
            uid: None,
            message_id: None,
        }
    }

    fn with_uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    fn with_message_id(mut self, message_id: &str) -> Self {
        self.message_id = Some(message_id.to_owned());
        self
    }
}

#[derive(Debug)]
struct SummaryBuildResult {
    messages: Vec<MessageSummary>,
    issues: Vec<ToolIssue>,
    attempted: usize,
    failed: usize,
}

#[derive(Debug, serde::Serialize)]
struct MessageMutationResult {
    message_id: String,
    status: String,
    issues: Vec<ToolIssue>,
    source_mailbox: String,
    destination_mailbox: Option<String>,
    destination_account_id: Option<String>,
    flags: Option<Vec<String>>,
    new_message_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct MailboxManagementResult {
    status: String,
    issues: Vec<ToolIssue>,
    account_id: String,
    action: String,
    mailbox: String,
    destination_mailbox: Option<String>,
}

/// Tool implementation methods
///
/// Private methods handle the actual business logic for each tool, separated
/// from the public `#[tool]` methods that handle response formatting.
impl MailImapServer {
    async fn list_mailboxes_impl(&self, input: AccountOnlyInput) -> AppResult<serde_json::Value> {
        validate_account_id(&input.account_id)?;
        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();

        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(ToolIssue::from_error("connect_authenticated", &error));
                log_runtime_issues(
                    "imap_list_mailboxes",
                    "failed",
                    &input.account_id,
                    None,
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "next_action": next_action_list_accounts(),
                    "account_id": account.account_id,
                    "mailboxes": []
                }));
            }
        };

        let items = match imap::list_all_mailboxes(&self.config, &mut session).await {
            Ok(items) => items,
            Err(error) => {
                issues.push(ToolIssue::from_error("list_mailboxes", &error));
                Vec::new()
            }
        };

        let mailboxes = items
            .into_iter()
            .take(200)
            .map(|item| MailboxInfo {
                name: decode_mailbox_name_for_display(item.name()),
                delimiter: item.delimiter().map(|d| d.to_string()),
            })
            .collect::<Vec<_>>();

        let status = status_from_counts(issues.is_empty(), !mailboxes.is_empty());
        log_runtime_issues(
            "imap_list_mailboxes",
            status,
            &input.account_id,
            None,
            &issues,
        );
        let next_action = preferred_mailbox_name(&mailboxes)
            .map(|mailbox| next_action_search_mailbox(&input.account_id, &mailbox))
            .unwrap_or_else(next_action_list_accounts);

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "next_action": next_action,
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
        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                let issue = ToolIssue::from_error("connect_authenticated", &error);
                let issues = vec![issue];
                log_runtime_issues(
                    "imap_search_messages",
                    "failed",
                    &input.account_id,
                    Some(&input.mailbox),
                    &issues,
                );
                return Ok(SearchResultData {
                    status: "failed".to_owned(),
                    issues,
                    next_action: next_action_list_mailboxes(&input.account_id),
                    account_id: input.account_id,
                    mailbox: input.mailbox,
                    total: 0,
                    attempted: 0,
                    returned: 0,
                    failed: 0,
                    messages: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                });
            }
        };
        let uidvalidity =
            match imap::select_mailbox_readonly(&self.config, &mut session, &input.mailbox).await {
                Ok(uidvalidity) => uidvalidity,
                Err(error) => {
                    let issue = ToolIssue::from_error("select_mailbox_readonly", &error);
                    let issues = vec![issue];
                    log_runtime_issues(
                        "imap_search_messages",
                        "failed",
                        &input.account_id,
                        Some(&input.mailbox),
                        &issues,
                    );
                    return Ok(SearchResultData {
                        status: "failed".to_owned(),
                        issues,
                        next_action: next_action_list_mailboxes(&input.account_id),
                        account_id: input.account_id,
                        mailbox: input.mailbox,
                        total: 0,
                        attempted: 0,
                        returned: 0,
                        failed: 0,
                        messages: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                    });
                }
            };

        let snapshot = if let Some(cursor) = input.cursor.clone() {
            resume_cursor_search(&self.cursors, &input, uidvalidity, cursor).await?
        } else {
            match start_new_search(&self.config, &mut session, &input).await {
                Ok(snapshot) => snapshot,
                Err(error) if is_hard_precondition_error(&error) => return Err(error),
                Err(error) => {
                    let issue = ToolIssue::from_error("uid_search", &error);
                    let issues = vec![issue];
                    log_runtime_issues(
                        "imap_search_messages",
                        "failed",
                        &input.account_id,
                        Some(&input.mailbox),
                        &issues,
                    );
                    return Ok(SearchResultData {
                        status: "failed".to_owned(),
                        issues,
                        next_action: next_action_list_mailboxes(&input.account_id),
                        account_id: input.account_id,
                        mailbox: input.mailbox,
                        total: 0,
                        attempted: 0,
                        returned: 0,
                        failed: 0,
                        messages: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                    });
                }
            }
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

        let SummaryBuildResult {
            messages,
            issues,
            attempted,
            failed,
        } = build_message_summaries(
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
        .await;

        let next_offset = offset + page_uids.len();
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

        let status = status_from_issue_and_counts(&issues, !messages.is_empty()).to_owned();
        log_runtime_issues(
            "imap_search_messages",
            &status,
            &input.account_id,
            Some(&input.mailbox),
            &issues,
        );
        let next_action = next_action_for_search_result(
            &status,
            &input.account_id,
            &input.mailbox,
            input.limit,
            next_cursor.as_deref(),
            &messages,
        );

        Ok(SearchResultData {
            status,
            issues,
            next_action,
            account_id: input.account_id,
            mailbox: input.mailbox,
            total,
            attempted,
            returned: messages.len(),
            failed,
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
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();

        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message": serde_json::Value::Null,
                }));
            }
        };
        ensure_uidvalidity_matches_readonly(&self.config, &mut session, &msg_id).await?;

        let raw = match imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message": serde_json::Value::Null,
                }));
            }
        };

        let parsed = mime::parse_message(
            &raw,
            input.body_max_chars,
            input.include_html,
            input.extract_attachment_text,
            attachment_text_max_chars,
        );

        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("parse_message", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message": serde_json::Value::Null,
                }));
            }
        };

        let headers = if input.include_headers || input.include_all_headers {
            Some(mime::curated_headers(
                &parsed.headers_all,
                input.include_all_headers,
            ))
        } else {
            None
        };

        let flags = match imap::fetch_flags(&self.config, &mut session, msg_id.uid).await {
            Ok(flags) => Some(flags),
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                None
            }
        };

        let detail = MessageDetail {
            message_id: encoded_message_id.clone(),
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
            flags,
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

        let status = status_from_issue_and_counts(&issues, true);
        log_runtime_issues(
            "imap_get_message",
            status,
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
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
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();
        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message_raw",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message_id": encoded_message_id,
                    "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "size_bytes": 0,
                    "raw_source_base64": serde_json::Value::Null,
                    "raw_source_encoding": serde_json::Value::Null,
                }));
            }
        };
        ensure_uidvalidity_matches_readonly(&self.config, &mut session, &msg_id).await?;

        let raw = match imap::fetch_raw_message(&self.config, &mut session, msg_id.uid).await {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message_raw",
                    "failed",
                    &input.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": input.account_id,
                    "message_id": encoded_message_id,
                    "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
                    "size_bytes": 0,
                    "raw_source_base64": serde_json::Value::Null,
                    "raw_source_encoding": serde_json::Value::Null,
                }));
            }
        };
        if raw.len() > input.max_bytes {
            return Err(AppError::InvalidInput(
                "message exceeds max_bytes; increase max_bytes".to_owned(),
            ));
        }

        log_runtime_issues(
            "imap_get_message_raw",
            "ok",
            &input.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": "ok",
            "issues": issues,
            "account_id": input.account_id,
            "message_id": encoded_message_id,
            "message_uri": build_message_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
            "message_raw_uri": build_message_raw_uri(&msg_id.account_id, &msg_id.mailbox, msg_id.uidvalidity, msg_id.uid),
            "size_bytes": raw.len(),
            "raw_source_base64": encode_raw_source_base64(&raw),
            "raw_source_encoding": "base64",
        }))
    }

    async fn apply_to_messages_impl(
        &self,
        input: ApplyToMessagesInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        validate_chars(input.max_messages, 1, 1_000, "max_messages")?;
        validate_message_action(&input.action, &input.account_id)?;

        let message_ids = self
            .resolve_message_selection(&input.account_id, &input.selector, input.max_messages)
            .await?;
        let action_name = message_action_name(&input.action);

        let mut results = Vec::with_capacity(message_ids.len());
        let mut issues = Vec::new();

        if input.dry_run {
            for msg_id in message_ids {
                results.push(build_planned_message_result(&msg_id, &input.action));
            }

            return Ok(serde_json::json!({
                "status": "ok",
                "issues": issues,
                "account_id": input.account_id,
                "action": action_name,
                "dry_run": true,
                "matched": results.len(),
                "attempted": 0,
                "succeeded": 0,
                "failed": 0,
                "results": results,
            }));
        }

        let matched = message_ids.len();
        let mut succeeded = 0usize;
        for msg_id in message_ids {
            let result = self
                .execute_message_action(&input.account_id, &input.action, &msg_id)
                .await;
            if result.status == "ok" {
                succeeded += 1;
            }
            issues.extend(result.issues.iter().cloned());
            results.push(result);
        }
        let failed = matched.saturating_sub(succeeded);
        let status = status_from_issue_and_counts(&issues, succeeded > 0).to_owned();
        log_runtime_issues(
            "imap_apply_to_messages",
            &status,
            &input.account_id,
            None,
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": input.account_id,
            "action": action_name,
            "dry_run": false,
            "matched": matched,
            "attempted": matched,
            "succeeded": succeeded,
            "failed": failed,
            "results": results,
        }))
    }

    async fn manage_mailbox_impl(&self, input: ManageMailboxInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;

        let (action_name, mailbox, destination_mailbox) = match &input.action {
            MailboxAction::Create { mailbox } => {
                validate_mailbox(mailbox)?;
                ("create", mailbox.clone(), None)
            }
            MailboxAction::Rename {
                mailbox,
                destination_mailbox,
            } => {
                validate_mailbox(mailbox)?;
                validate_mailbox(destination_mailbox)?;
                if normalize_mailbox_name(mailbox) == normalize_mailbox_name(destination_mailbox) {
                    return Err(AppError::InvalidInput(
                        "destination_mailbox must differ from mailbox".to_owned(),
                    ));
                }
                ("rename", mailbox.clone(), Some(destination_mailbox.clone()))
            }
            MailboxAction::Delete { mailbox } => {
                validate_mailbox(mailbox)?;
                ("delete", mailbox.clone(), None)
            }
        };

        let account = self.config.get_account(&input.account_id)?;
        let mut issues = Vec::new();
        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(ToolIssue::from_error("connect_authenticated", &error));
                let result = MailboxManagementResult {
                    status: "failed".to_owned(),
                    issues: issues.clone(),
                    account_id: input.account_id,
                    action: action_name.to_owned(),
                    mailbox,
                    destination_mailbox,
                };
                return serde_json::to_value(result).map_err(serialization_error);
            }
        };

        let operation = match &input.action {
            MailboxAction::Create { mailbox } => {
                imap::create_mailbox_path(&self.config, &mut session, mailbox).await
            }
            MailboxAction::Rename {
                mailbox,
                destination_mailbox,
            } => {
                match imap::create_parent_mailboxes(&self.config, &mut session, destination_mailbox)
                    .await
                {
                    Ok(()) => {
                        imap::rename_mailbox(
                            &self.config,
                            &mut session,
                            mailbox,
                            destination_mailbox,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                }
            }
            MailboxAction::Delete { mailbox } => {
                imap::delete_mailbox(&self.config, &mut session, mailbox).await
            }
        };

        if let Err(error) = operation {
            let stage = match &input.action {
                MailboxAction::Create { .. } => "create_mailbox",
                MailboxAction::Rename { .. } => "rename_mailbox",
                MailboxAction::Delete { .. } => "delete_mailbox",
            };
            issues.push(ToolIssue::from_error(stage, &error));
        }

        let status = status_from_issue_and_counts(&issues, issues.is_empty()).to_owned();
        log_runtime_issues(
            "imap_manage_mailbox",
            &status,
            &input.account_id,
            Some(&mailbox),
            &issues,
        );

        let result = MailboxManagementResult {
            status,
            issues,
            account_id: input.account_id,
            action: action_name.to_owned(),
            mailbox,
            destination_mailbox,
        };
        serde_json::to_value(result).map_err(serialization_error)
    }

    async fn resolve_message_selection(
        &self,
        account_id: &str,
        selector: &crate::models::MessageSelectorInput,
        max_messages: usize,
    ) -> AppResult<Vec<MessageId>> {
        match (&selector.message_ids, &selector.search) {
            (Some(message_ids), None) => {
                if message_ids.is_empty() {
                    return Err(AppError::InvalidInput(
                        "selector.message_ids must not be empty".to_owned(),
                    ));
                }
                if message_ids.len() > max_messages {
                    return Err(AppError::InvalidInput(format!(
                        "selector matched {} messages; narrow selection to at most {}",
                        message_ids.len(),
                        max_messages
                    )));
                }
                dedupe_and_parse_message_ids(account_id, message_ids)
            }
            (None, Some(search)) => {
                self.resolve_search_selection(account_id, search, max_messages)
                    .await
            }
            _ => Err(AppError::InvalidInput(
                "selector must include exactly one of message_ids or search".to_owned(),
            )),
        }
    }

    async fn resolve_search_selection(
        &self,
        account_id: &str,
        search: &SearchSelectorInput,
        max_messages: usize,
    ) -> AppResult<Vec<MessageId>> {
        let search_input = search_selector_to_search_input(account_id, search);
        validate_search_input(&search_input)?;

        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        let uidvalidity =
            imap::select_mailbox_readonly(&self.config, &mut session, &search_input.mailbox)
                .await?;

        let snapshot = if let Some(cursor) = search_input.cursor.clone() {
            resume_cursor_search(&self.cursors, &search_input, uidvalidity, cursor).await?
        } else {
            start_new_search(&self.config, &mut session, &search_input).await?
        };

        let selected = snapshot
            .uids_desc
            .into_iter()
            .skip(snapshot.offset)
            .collect::<Vec<_>>();
        if selected.len() > max_messages {
            return Err(AppError::InvalidInput(format!(
                "selector matched {} messages; narrow selection to at most {}",
                selected.len(),
                max_messages
            )));
        }

        Ok(selected
            .into_iter()
            .map(|uid| MessageId {
                account_id: account_id.to_owned(),
                mailbox: search_input.mailbox.clone(),
                uidvalidity,
                uid,
            })
            .collect())
    }

    async fn execute_message_action(
        &self,
        account_id: &str,
        action: &MessageActionInput,
        msg_id: &MessageId,
    ) -> MessageMutationResult {
        match action {
            MessageActionInput::Move {
                destination_mailbox,
            } => {
                self.execute_move_message(account_id, msg_id, destination_mailbox)
                    .await
            }
            MessageActionInput::Copy {
                destination_mailbox,
                destination_account_id,
            } => {
                self.execute_copy_message(
                    account_id,
                    msg_id,
                    destination_mailbox,
                    destination_account_id.as_deref(),
                )
                .await
            }
            MessageActionInput::Delete => self.execute_delete_message(account_id, msg_id).await,
            MessageActionInput::UpdateFlags {
                add_flags,
                remove_flags,
            } => {
                self.execute_update_flags(
                    account_id,
                    msg_id,
                    add_flags.clone().unwrap_or_default(),
                    remove_flags.clone().unwrap_or_default(),
                )
                .await
            }
        }
    }

    async fn execute_update_flags(
        &self,
        account_id: &str,
        msg_id: &MessageId,
        add_flags: Vec<String>,
        remove_flags: Vec<String>,
    ) -> MessageMutationResult {
        let encoded_message_id = msg_id.encode();
        let mut issues = Vec::new();
        let account = match self.config.get_account(account_id) {
            Ok(account) => account,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("account_lookup", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(msg_id, encoded_message_id, issues, None, None, None);
            }
        };

        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(msg_id, encoded_message_id, issues, None, None, None);
            }
        };
        if let Err(error) =
            ensure_uidvalidity_matches_readwrite(&self.config, &mut session, msg_id).await
        {
            issues.push(
                ToolIssue::from_error("select_mailbox_readwrite", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
            return failed_message_result(msg_id, encoded_message_id, issues, None, None, None);
        }

        if !add_flags.is_empty() {
            let query = format!("+FLAGS.SILENT ({})", add_flags.join(" "));
            if let Err(error) =
                imap::uid_store(&self.config, &mut session, msg_id.uid, &query).await
            {
                issues.push(
                    ToolIssue::from_error("uid_store_add_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            }
        }
        if !remove_flags.is_empty() {
            let query = format!("-FLAGS.SILENT ({})", remove_flags.join(" "));
            if let Err(error) =
                imap::uid_store(&self.config, &mut session, msg_id.uid, &query).await
            {
                issues.push(
                    ToolIssue::from_error("uid_store_remove_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            }
        }

        let flags = match imap::fetch_flags(&self.config, &mut session, msg_id.uid).await {
            Ok(flags) => Some(flags),
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_flags", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                None
            }
        };

        let has_flags = flags.is_some();
        finalize_message_result(
            msg_id,
            encoded_message_id,
            issues,
            None,
            None,
            flags,
            has_flags,
        )
    }

    async fn execute_copy_message(
        &self,
        account_id: &str,
        msg_id: &MessageId,
        destination_mailbox: &str,
        destination_account_id: Option<&str>,
    ) -> MessageMutationResult {
        let encoded_message_id = msg_id.encode();
        let destination_account_id = destination_account_id.unwrap_or(account_id).to_owned();
        let mut issues = Vec::new();

        let source_account = match self.config.get_account(account_id) {
            Ok(account) => account,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("account_lookup_source", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(destination_account_id),
                    None,
                );
            }
        };

        if destination_account_id == account_id {
            let mut session = match imap::connect_authenticated(&self.config, source_account).await
            {
                Ok(session) => session,
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("connect_authenticated_source", &error)
                            .with_message_id(&encoded_message_id),
                    );
                    return failed_message_result(
                        msg_id,
                        encoded_message_id,
                        issues,
                        Some(destination_mailbox.to_owned()),
                        Some(destination_account_id),
                        None,
                    );
                }
            };
            if let Err(error) =
                ensure_uidvalidity_matches_readwrite(&self.config, &mut session, msg_id).await
            {
                issues.push(
                    ToolIssue::from_error("select_mailbox_readwrite", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(destination_account_id),
                    None,
                );
            }
            if let Err(error) =
                imap::uid_copy(&self.config, &mut session, msg_id.uid, destination_mailbox).await
            {
                issues.push(
                    ToolIssue::from_error("uid_copy", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            }
            return finalize_message_result(
                msg_id,
                encoded_message_id,
                issues,
                Some(destination_mailbox.to_owned()),
                Some(destination_account_id),
                None,
                true,
            );
        }

        let mut source_session =
            match imap::connect_authenticated(&self.config, source_account).await {
                Ok(session) => session,
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("connect_authenticated_source", &error)
                            .with_message_id(&encoded_message_id),
                    );
                    return failed_message_result(
                        msg_id,
                        encoded_message_id,
                        issues,
                        Some(destination_mailbox.to_owned()),
                        Some(destination_account_id),
                        None,
                    );
                }
            };
        if let Err(error) =
            ensure_uidvalidity_matches_readonly(&self.config, &mut source_session, msg_id).await
        {
            issues.push(
                ToolIssue::from_error("select_mailbox_readonly", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
            return failed_message_result(
                msg_id,
                encoded_message_id,
                issues,
                Some(destination_mailbox.to_owned()),
                Some(destination_account_id),
                None,
            );
        }
        let raw = match imap::fetch_raw_message(&self.config, &mut source_session, msg_id.uid).await
        {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message_source", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(destination_account_id),
                    None,
                );
            }
        };

        let destination_account = match self.config.get_account(&destination_account_id) {
            Ok(account) => account,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("account_lookup_destination", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(destination_account_id),
                    None,
                );
            }
        };
        let mut destination_session =
            match imap::connect_authenticated(&self.config, destination_account).await {
                Ok(session) => session,
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("connect_authenticated_destination", &error)
                            .with_message_id(&encoded_message_id),
                    );
                    return failed_message_result(
                        msg_id,
                        encoded_message_id,
                        issues,
                        Some(destination_mailbox.to_owned()),
                        Some(destination_account_id),
                        None,
                    );
                }
            };
        if let Err(error) = imap::append(
            &self.config,
            &mut destination_session,
            destination_mailbox,
            &raw,
        )
        .await
        {
            issues.push(
                ToolIssue::from_error("append_destination", &error)
                    .with_message_id(&encoded_message_id),
            );
        }
        finalize_message_result(
            msg_id,
            encoded_message_id,
            issues,
            Some(destination_mailbox.to_owned()),
            Some(destination_account_id),
            None,
            true,
        )
    }

    async fn execute_move_message(
        &self,
        account_id: &str,
        msg_id: &MessageId,
        destination_mailbox: &str,
    ) -> MessageMutationResult {
        let encoded_message_id = msg_id.encode();
        let mut issues = Vec::new();
        let account = match self.config.get_account(account_id) {
            Ok(account) => account,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("account_lookup", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(account_id.to_owned()),
                    None,
                );
            }
        };

        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(account_id.to_owned()),
                    None,
                );
            }
        };
        if let Err(error) =
            ensure_uidvalidity_matches_readwrite(&self.config, &mut session, msg_id).await
        {
            issues.push(
                ToolIssue::from_error("select_mailbox_readwrite", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
            return failed_message_result(
                msg_id,
                encoded_message_id,
                issues,
                Some(destination_mailbox.to_owned()),
                Some(account_id.to_owned()),
                None,
            );
        }

        let caps = match imap::capabilities(&self.config, &mut session).await {
            Ok(caps) => caps,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("capabilities", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    Some(destination_mailbox.to_owned()),
                    Some(account_id.to_owned()),
                    None,
                );
            }
        };

        if caps.has_str("MOVE") {
            if let Err(error) =
                imap::uid_move(&self.config, &mut session, msg_id.uid, destination_mailbox).await
            {
                issues.push(
                    ToolIssue::from_error("uid_move", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
            }
        } else {
            let copied = if let Err(error) =
                imap::uid_copy(&self.config, &mut session, msg_id.uid, destination_mailbox).await
            {
                issues.push(
                    ToolIssue::from_error("uid_copy", &error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                false
            } else {
                true
            };
            if copied {
                let deleted = if let Err(error) = imap::uid_store(
                    &self.config,
                    &mut session,
                    msg_id.uid,
                    "+FLAGS.SILENT (\\Deleted)",
                )
                .await
                {
                    issues.push(
                        ToolIssue::from_error("uid_store_deleted", &error)
                            .with_uid(msg_id.uid)
                            .with_message_id(&encoded_message_id),
                    );
                    false
                } else {
                    true
                };
                if deleted
                    && let Err(error) =
                        imap::uid_expunge(&self.config, &mut session, msg_id.uid).await
                {
                    issues.push(
                        ToolIssue::from_error("uid_expunge", &error)
                            .with_uid(msg_id.uid)
                            .with_message_id(&encoded_message_id),
                    );
                }
            }
        }

        finalize_message_result(
            msg_id,
            encoded_message_id,
            issues,
            Some(destination_mailbox.to_owned()),
            Some(account_id.to_owned()),
            None,
            true,
        )
    }

    async fn execute_delete_message(
        &self,
        account_id: &str,
        msg_id: &MessageId,
    ) -> MessageMutationResult {
        let encoded_message_id = msg_id.encode();
        let mut issues = Vec::new();
        let account = match self.config.get_account(account_id) {
            Ok(account) => account,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("account_lookup", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    None,
                    Some(account_id.to_owned()),
                    None,
                );
            }
        };

        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                return failed_message_result(
                    msg_id,
                    encoded_message_id,
                    issues,
                    None,
                    Some(account_id.to_owned()),
                    None,
                );
            }
        };
        if let Err(error) =
            ensure_uidvalidity_matches_readwrite(&self.config, &mut session, msg_id).await
        {
            issues.push(
                ToolIssue::from_error("select_mailbox_readwrite", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
            return failed_message_result(
                msg_id,
                encoded_message_id,
                issues,
                None,
                Some(account_id.to_owned()),
                None,
            );
        }

        let flagged_deleted = if let Err(error) = imap::uid_store(
            &self.config,
            &mut session,
            msg_id.uid,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        {
            issues.push(
                ToolIssue::from_error("uid_store_deleted", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
            false
        } else {
            true
        };
        if flagged_deleted
            && let Err(error) = imap::uid_expunge(&self.config, &mut session, msg_id.uid).await
        {
            issues.push(
                ToolIssue::from_error("uid_expunge", &error)
                    .with_uid(msg_id.uid)
                    .with_message_id(&encoded_message_id),
            );
        }

        finalize_message_result(
            msg_id,
            encoded_message_id,
            issues,
            None,
            Some(account_id.to_owned()),
            None,
            true,
        )
    }
}

/// Calculate elapsed milliseconds
fn duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn status_from_counts(no_issues: bool, has_data: bool) -> &'static str {
    if no_issues {
        "ok"
    } else if has_data {
        "partial"
    } else {
        "failed"
    }
}

fn status_from_issue_and_counts(issues: &[ToolIssue], has_data: bool) -> &'static str {
    status_from_counts(issues.is_empty(), has_data)
}

fn app_error_code(error: &AppError) -> &'static str {
    match error {
        AppError::InvalidInput(_) => "invalid_input",
        AppError::NotFound(_) => "not_found",
        AppError::AuthFailed(_) => "auth_failed",
        AppError::Timeout(_) => "timeout",
        AppError::Conflict(_) => "conflict",
        AppError::Internal(_) => "internal",
    }
}

fn log_runtime_issues(
    tool: &str,
    status: &str,
    account_id: &str,
    mailbox: Option<&str>,
    issues: &[ToolIssue],
) {
    for issue in issues {
        let is_error = status == "failed" || matches!(issue.code.as_str(), "internal" | "timeout");
        if is_error {
            error!(
                tool,
                stage = %issue.stage,
                code = %issue.code,
                retryable = issue.retryable,
                account_id,
                mailbox = ?mailbox,
                uid = ?issue.uid,
                message_id = ?issue.message_id,
                message = %issue.message,
                "runtime imap issue"
            );
        } else {
            warn!(
                tool,
                stage = %issue.stage,
                code = %issue.code,
                retryable = issue.retryable,
                account_id,
                mailbox = ?mailbox,
                uid = ?issue.uid,
                message_id = ?issue.message_id,
                message = %issue.message,
                "runtime imap issue"
            );
        }
    }
}

fn next_action(instruction: &str, tool: &str, arguments: serde_json::Value) -> NextAction {
    NextAction {
        instruction: instruction.to_owned(),
        tool: tool.to_owned(),
        arguments,
    }
}

fn next_action_list_accounts() -> NextAction {
    next_action(
        "List configured accounts before retrying mailbox access.",
        "imap_list_accounts",
        serde_json::json!({}),
    )
}

fn next_action_list_mailboxes(account_id: &str) -> NextAction {
    next_action(
        "List mailboxes to choose a mailbox for message search.",
        "imap_list_mailboxes",
        serde_json::json!({
            "account_id": account_id,
        }),
    )
}

fn next_action_search_mailbox(account_id: &str, mailbox: &str) -> NextAction {
    next_action(
        "Search for messages in the selected mailbox.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailbox": mailbox,
            "limit": 10,
            "include_snippet": false,
        }),
    )
}

fn preferred_mailbox_name(mailboxes: &[MailboxInfo]) -> Option<String> {
    mailboxes
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("INBOX"))
        .map(|m| m.name.clone())
        .or_else(|| mailboxes.first().map(|m| m.name.clone()))
}

fn next_action_for_search_result(
    status: &str,
    account_id: &str,
    mailbox: &str,
    limit: usize,
    cursor: Option<&str>,
    messages: &[MessageSummary],
) -> NextAction {
    if let Some(cursor) = cursor {
        return next_action(
            "Continue pagination to retrieve more messages.",
            "imap_search_messages",
            serde_json::json!({
                "account_id": account_id,
                "mailbox": mailbox,
                "cursor": cursor,
                "limit": limit,
                "include_snippet": false,
            }),
        );
    }

    if status == "failed" {
        return next_action_list_mailboxes(account_id);
    }

    if let Some(first) = messages.first() {
        return next_action(
            "Open a message to inspect full content and headers.",
            "imap_get_message",
            serde_json::json!({
                "account_id": account_id,
                "message_id": first.message_id,
            }),
        );
    }

    next_action(
        "Retry search with broader criteria.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailbox": mailbox,
            "limit": limit,
            "include_snippet": false,
        }),
    )
}

fn is_hard_precondition_error(error: &AppError) -> bool {
    matches!(error, AppError::InvalidInput(_) | AppError::Conflict(_))
}

/// Build a standardized MCP tool response envelope from business logic output
fn finalize_tool<T>(
    started: Instant,
    tool: &str,
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
        Err(e) => {
            error!(
                tool,
                code = app_error_code(&e),
                message = %e,
                "hard mcp error"
            );
            Err(e.to_error_data())
        }
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
    if entry.account_id != input.account_id
        || normalize_mailbox_name(&entry.mailbox) != normalize_mailbox_name(&input.mailbox)
    {
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
) -> SummaryBuildResult {
    let mut messages = Vec::with_capacity(uids.len());
    let mut issues = Vec::new();
    let mut failed = 0usize;

    for uid in uids {
        let (header_bytes, flags) = match imap::fetch_headers_and_flags(config, session, *uid).await
        {
            Ok(result) => result,
            Err(error) => {
                failed += 1;
                issues
                    .push(ToolIssue::from_error("fetch_headers_and_flags", &error).with_uid(*uid));
                continue;
            }
        };

        let headers = match mime::parse_header_bytes(&header_bytes) {
            Ok(headers) => headers,
            Err(error) => {
                failed += 1;
                issues.push(ToolIssue::from_error("parse_header_bytes", &error).with_uid(*uid));
                continue;
            }
        };

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

    SummaryBuildResult {
        messages,
        issues,
        attempted: uids.len(),
        failed,
    }
}

struct SummaryBuildOptions<'a> {
    account_id: &'a str,
    mailbox: &'a str,
    uidvalidity: u32,
    include_snippet: bool,
    snippet_max_chars: usize,
}

fn serialization_error(error: serde_json::Error) -> AppError {
    AppError::Internal(format!("serialization failure: {error}"))
}

fn message_action_name(action: &MessageActionInput) -> &'static str {
    match action {
        MessageActionInput::Move { .. } => "move",
        MessageActionInput::Copy { .. } => "copy",
        MessageActionInput::Delete => "delete",
        MessageActionInput::UpdateFlags { .. } => "update_flags",
    }
}

fn search_selector_to_search_input(
    account_id: &str,
    search: &SearchSelectorInput,
) -> SearchMessagesInput {
    SearchMessagesInput {
        account_id: account_id.to_owned(),
        mailbox: search.mailbox.clone(),
        cursor: search.cursor.clone(),
        query: search.query.clone(),
        from: search.from.clone(),
        to: search.to.clone(),
        subject: search.subject.clone(),
        unread_only: search.unread_only,
        last_days: search.last_days,
        start_date: search.start_date.clone(),
        end_date: search.end_date.clone(),
        limit: MAX_SEARCH_LIMIT,
        include_snippet: false,
        snippet_max_chars: None,
    }
}

fn validate_message_action(action: &MessageActionInput, account_id: &str) -> AppResult<()> {
    match action {
        MessageActionInput::Move {
            destination_mailbox,
        } => validate_mailbox(destination_mailbox),
        MessageActionInput::Copy {
            destination_mailbox,
            destination_account_id,
        } => {
            validate_mailbox(destination_mailbox)?;
            if let Some(destination_account_id) = destination_account_id {
                validate_account_id(destination_account_id)?;
            } else {
                validate_account_id(account_id)?;
            }
            Ok(())
        }
        MessageActionInput::Delete => Ok(()),
        MessageActionInput::UpdateFlags {
            add_flags,
            remove_flags,
        } => {
            let add_flags = add_flags.clone().unwrap_or_default();
            let remove_flags = remove_flags.clone().unwrap_or_default();
            if add_flags.is_empty() && remove_flags.is_empty() {
                return Err(AppError::InvalidInput(
                    "update_flags requires at least one of add_flags/remove_flags".to_owned(),
                ));
            }
            validate_flags(&add_flags, "add_flags")?;
            validate_flags(&remove_flags, "remove_flags")
        }
    }
}

fn dedupe_and_parse_message_ids(
    account_id: &str,
    message_ids: &[String],
) -> AppResult<Vec<MessageId>> {
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    for message_id in message_ids {
        let parsed = parse_and_validate_message_id(account_id, message_id)?;
        let encoded = parsed.encode();
        if seen.insert(encoded) {
            resolved.push(parsed);
        }
    }
    Ok(resolved)
}

fn build_planned_message_result(
    msg_id: &MessageId,
    action: &MessageActionInput,
) -> MessageMutationResult {
    let (destination_mailbox, destination_account_id, flags) = match action {
        MessageActionInput::Move {
            destination_mailbox,
        } => (
            Some(destination_mailbox.clone()),
            Some(msg_id.account_id.clone()),
            None,
        ),
        MessageActionInput::Copy {
            destination_mailbox,
            destination_account_id,
        } => (
            Some(destination_mailbox.clone()),
            Some(
                destination_account_id
                    .clone()
                    .unwrap_or_else(|| msg_id.account_id.clone()),
            ),
            None,
        ),
        MessageActionInput::Delete => (None, Some(msg_id.account_id.clone()), None),
        MessageActionInput::UpdateFlags { .. } => (None, Some(msg_id.account_id.clone()), None),
    };
    MessageMutationResult {
        message_id: msg_id.encode(),
        status: "planned".to_owned(),
        issues: Vec::new(),
        source_mailbox: msg_id.mailbox.clone(),
        destination_mailbox,
        destination_account_id,
        flags,
        new_message_id: None,
    }
}

fn failed_message_result(
    msg_id: &MessageId,
    encoded_message_id: String,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<String>,
    destination_account_id: Option<String>,
    flags: Option<Vec<String>>,
) -> MessageMutationResult {
    MessageMutationResult {
        message_id: encoded_message_id,
        status: "failed".to_owned(),
        issues,
        source_mailbox: msg_id.mailbox.clone(),
        destination_mailbox,
        destination_account_id,
        flags,
        new_message_id: None,
    }
}

fn finalize_message_result(
    msg_id: &MessageId,
    encoded_message_id: String,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<String>,
    destination_account_id: Option<String>,
    flags: Option<Vec<String>>,
    success_on_no_issues: bool,
) -> MessageMutationResult {
    let status = status_from_issue_and_counts(&issues, success_on_no_issues || flags.is_some());
    MessageMutationResult {
        message_id: encoded_message_id,
        status: status.to_owned(),
        issues,
        source_mailbox: msg_id.mailbox.clone(),
        destination_mailbox,
        destination_account_id,
        flags,
        new_message_id: None,
    }
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

    if input.cursor.is_some() {
        return Ok(());
    }

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

fn encode_raw_source_base64(raw: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(raw)
}

#[cfg(test)]
/// Tests for server-side validation and encoding helpers.
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::sync::Mutex;

    use super::{
        dedupe_and_parse_message_ids, encode_raw_source_base64, escape_imap_quoted,
        resume_cursor_search, validate_flag, validate_mailbox, validate_message_action,
        validate_search_input, validate_search_text,
    };
    use crate::models::{MessageActionInput, SearchMessagesInput};
    use crate::pagination::{CursorEntry, CursorStore};

    /// Tests that control characters in search text are rejected.
    #[test]
    fn rejects_control_chars_in_search_text() {
        let err = validate_search_text("hello\nworld").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    /// Tests that control characters in mailbox names are rejected.
    #[test]
    fn rejects_control_chars_in_mailbox() {
        let err = validate_mailbox("INBOX\r").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    /// Tests that line breaks are rejected in IMAP quoted strings.
    #[test]
    fn escape_rejects_linebreaks() {
        let err = escape_imap_quoted("a\nb").expect_err("must fail");
        assert!(err.to_string().contains("control characters"));
    }

    /// Tests that common IMAP flags are accepted.
    #[test]
    fn validate_flag_allows_common_flags() {
        validate_flag("\\Seen").expect("system flag must be valid");
        validate_flag("Important").expect("keyword flag must be valid");
        validate_flag("$MailFlagBit0").expect("keyword flag must be valid");
    }

    /// Tests that injection-like flag values are rejected.
    #[test]
    fn validate_flag_rejects_injection_like_value() {
        let err = validate_flag("\\Seen) UID FETCH 1:* (BODY[]").expect_err("must fail");
        assert!(err.to_string().contains("invalid flag"));
    }

    #[test]
    fn validate_message_action_rejects_empty_flag_update() {
        let err = validate_message_action(
            &MessageActionInput::UpdateFlags {
                add_flags: None,
                remove_flags: None,
            },
            "default",
        )
        .expect_err("must reject empty flag update");
        assert!(
            err.to_string()
                .contains("at least one of add_flags/remove_flags")
        );
    }

    #[test]
    fn dedupe_and_parse_message_ids_removes_duplicates() {
        let message_ids = vec![
            "imap:default:INBOX:42:7".to_owned(),
            "imap:default:INBOX:42:7".to_owned(),
            "imap:default:INBOX:42:8".to_owned(),
        ];

        let parsed = dedupe_and_parse_message_ids("default", &message_ids)
            .expect("message ids should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].uid, 7);
        assert_eq!(parsed[1].uid, 8);
    }

    /// Tests that raw message sources are correctly base64 encoded.
    #[test]
    fn encodes_raw_source_as_base64() {
        let raw = [0_u8, 159, 255];
        assert_eq!(encode_raw_source_base64(&raw), "AJ//");
    }

    #[test]
    fn validate_search_input_allows_replayed_criteria_when_cursor_present() {
        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "Donations".to_owned(),
            cursor: Some("cursor-id".to_owned()),
            query: Some(".*".to_owned()),
            from: Some(".*".to_owned()),
            to: Some(".*".to_owned()),
            subject: Some(".*".to_owned()),
            unread_only: Some(false),
            last_days: Some(365),
            start_date: Some("2025-01-01".to_owned()),
            end_date: Some("2025-12-31".to_owned()),
            limit: 50,
            include_snippet: false,
            snippet_max_chars: Some(200),
        };

        validate_search_input(&input).expect("cursor mode should ignore replayed criteria");
    }

    #[test]
    fn validate_search_input_still_rejects_conflicting_dates_without_cursor() {
        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "Donations".to_owned(),
            cursor: None,
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: Some(30),
            start_date: Some("2025-01-01".to_owned()),
            end_date: Some("2025-12-31".to_owned()),
            limit: 50,
            include_snippet: false,
            snippet_max_chars: None,
        };

        let err = validate_search_input(&input).expect_err("must reject conflicting date filters");
        assert!(
            err.to_string()
                .contains("last_days cannot be combined with start_date/end_date")
        );
    }

    #[test]
    fn validate_search_input_still_rejects_snippet_size_without_snippets_on_new_search() {
        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "Donations".to_owned(),
            cursor: None,
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 50,
            include_snippet: false,
            snippet_max_chars: Some(200),
        };

        let err = validate_search_input(&input)
            .expect_err("must reject snippet_max_chars without include_snippet");
        assert!(
            err.to_string()
                .contains("snippet_max_chars requires include_snippet=true")
        );
    }

    #[tokio::test]
    async fn resume_cursor_accepts_legacy_encoded_mailbox_with_decoded_input() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let cursor_id = {
            let mut store = cursors.lock().await;
            store.create(CursorEntry {
                account_id: "default".to_owned(),
                mailbox: "&ZeVnLIqe-".to_owned(),
                uidvalidity: 42,
                uids_desc: vec![10, 9],
                offset: 1,
                include_snippet: false,
                snippet_max_chars: 200,
                expires_at: Instant::now(),
            })
        };

        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "日本語".to_owned(),
            cursor: Some(cursor_id.clone()),
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 10,
            include_snippet: false,
            snippet_max_chars: None,
        };

        let snapshot = resume_cursor_search(&cursors, &input, 42, cursor_id)
            .await
            .expect("legacy encoded cursor should resume");
        assert_eq!(snapshot.offset, 1);
        assert_eq!(snapshot.uids_desc, vec![10, 9]);
    }

    #[tokio::test]
    async fn resume_cursor_accepts_decoded_mailbox_with_legacy_encoded_input() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let cursor_id = {
            let mut store = cursors.lock().await;
            store.create(CursorEntry {
                account_id: "default".to_owned(),
                mailbox: "日本語".to_owned(),
                uidvalidity: 42,
                uids_desc: vec![10, 9],
                offset: 1,
                include_snippet: true,
                snippet_max_chars: 120,
                expires_at: Instant::now(),
            })
        };

        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "&ZeVnLIqe-".to_owned(),
            cursor: Some(cursor_id.clone()),
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 10,
            include_snippet: false,
            snippet_max_chars: None,
        };

        let snapshot = resume_cursor_search(&cursors, &input, 42, cursor_id)
            .await
            .expect("decoded cursor should resume from legacy encoded request");
        assert!(snapshot.include_snippet);
        assert_eq!(snapshot.snippet_max_chars, 120);
    }
}
