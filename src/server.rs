//! MCP server implementation with tool handlers
//!
//! Implements the `ServerHandler` trait and registers 10 MCP tools. Handles
//! input validation, business logic orchestration, and response formatting.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    MailboxInfo, ManageMailboxInput, MessageDetail, MessageSummary, Meta, OperationIdInput,
    SearchMessagesInput, ToolEnvelope, UpdateMessageFlagsInput,
};
use crate::pagination::{CursorEntry, CursorStore};

/// Maximum messages per search result page
const MAX_SEARCH_LIMIT: usize = 50;
/// Maximum attachments to return per message
const MAX_ATTACHMENTS: usize = 50;
/// Maximum UID search results stored in a cursor snapshot
const MAX_CURSOR_UIDS_STORED: usize = 20_000;
/// Maximum number of explicit message ids accepted by bulk write tools.
const MAX_BULK_MESSAGE_IDS: usize = 250;
/// Valid built-in IMAP system flags.
const VALID_SYSTEM_FLAGS: [&str; 5] = ["\\Seen", "\\Answered", "\\Flagged", "\\Deleted", "\\Draft"];
/// Maximum wall-clock budget for inline write execution before switching to background mode.
const WRITE_INLINE_BUDGET_MS: u64 = 1_500;

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
    /// Write operation state for inline/background execution.
    operations: Arc<Mutex<BTreeMap<String, StoredOperation>>>,
    /// Per-account destructive write serialization guards.
    account_write_locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
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
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            account_write_locks: Arc::new(Mutex::new(BTreeMap::new())),
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

    /// Tool: Apply one non-flag mutation to explicit messages.
    #[tool(
        name = "imap_apply_to_messages",
        description = "Apply one mutation action to explicit messages"
    )]
    async fn apply_to_messages(
        &self,
        Parameters(input): Parameters<ApplyToMessagesInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_apply_to_messages",
            self.apply_to_messages_impl(input)
                .await
                .map(|data| (operation_summary("apply_to_messages", &data), data)),
        )
    }

    /// Tool: Update flags on explicit messages.
    #[tool(
        name = "imap_update_message_flags",
        description = "Add, remove, or replace flags on explicit messages"
    )]
    async fn update_message_flags(
        &self,
        Parameters(input): Parameters<UpdateMessageFlagsInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_update_message_flags",
            self.update_message_flags_impl(input)
                .await
                .map(|data| (operation_summary("update_message_flags", &data), data)),
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
            self.manage_mailbox_impl(input)
                .await
                .map(|data| (operation_summary("manage_mailbox", &data), data)),
        )
    }

    /// Tool: Inspect a background write operation.
    #[tool(
        name = "imap_get_operation",
        description = "Get the status of a background IMAP write operation"
    )]
    async fn get_operation(
        &self,
        Parameters(input): Parameters<OperationIdInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_get_operation",
            self.get_operation_impl(input)
                .await
                .map(|data| (operation_summary("operation", &data), data)),
        )
    }

    /// Tool: Request cancellation for a background write operation.
    #[tool(
        name = "imap_cancel_operation",
        description = "Cancel a background IMAP write operation"
    )]
    async fn cancel_operation(
        &self,
        Parameters(input): Parameters<OperationIdInput>,
    ) -> Result<Json<ToolEnvelope<serde_json::Value>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_cancel_operation",
            self.cancel_operation_impl(input)
                .await
                .map(|data| (operation_summary("operation", &data), data)),
        )
    }
}

/// MCP server handler implementation
///
/// Provides server info and capabilities to MCP client.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for MailImapServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Secure IMAP MCP server. Read operations are enabled by default; write tools require MAIL_IMAP_WRITE_ENABLED=true.",
        )
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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
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

#[derive(Debug, Clone, serde::Serialize)]
struct MessageMutationResult {
    message_id: String,
    status: String,
    issues: Vec<ToolIssue>,
    source_mailbox: String,
    destination_mailbox: Option<String>,
    flags: Option<Vec<String>>,
    new_message_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct MailboxManagementResult {
    status: String,
    issues: Vec<ToolIssue>,
    account_id: String,
    action: String,
    mailbox: String,
    destination_mailbox: Option<String>,
}

#[derive(Debug, Clone)]
enum MessageActionInput {
    Move { destination_mailbox: String },
    Copy { destination_mailbox: String },
    Delete,
}

#[derive(Debug, Clone)]
enum FlagOperation {
    Add,
    Remove,
    Replace,
}

#[derive(Debug, Clone)]
struct FlagUpdateRequest {
    operation: FlagOperation,
    flags: Vec<String>,
}

#[derive(Debug, Clone)]
struct MessageMutationGroup {
    mailbox: String,
    uidvalidity: u32,
    entries: Vec<MessageId>,
}

#[derive(Debug, Clone)]
enum MailboxAction {
    Create {
        mailbox: String,
    },
    Rename {
        mailbox: String,
        destination_mailbox: String,
    },
    Delete {
        mailbox: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationState {
    Pending,
    Running,
    CancelRequested,
    Ok,
    Partial,
    Failed,
    Canceled,
}

impl OperationState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Ok | Self::Partial | Self::Failed | Self::Canceled
        )
    }

    fn status_label(self) -> &'static str {
        match self {
            Self::Pending => "accepted",
            Self::Running => "running",
            Self::CancelRequested => "running",
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    fn state_label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct OperationProgress {
    total_units: usize,
    completed_units: usize,
    failed_units: usize,
    remaining_units: usize,
    current_mailbox: Option<String>,
    phase: String,
}

impl OperationProgress {
    fn new(total_units: usize, phase: &str) -> Self {
        Self {
            total_units,
            completed_units: 0,
            failed_units: 0,
            remaining_units: total_units,
            current_mailbox: None,
            phase: phase.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
struct StoredOperation {
    operation_id: String,
    kind: String,
    state: OperationState,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    cancel_supported: bool,
    worker_started: bool,
    progress: OperationProgress,
    issues: Vec<ToolIssue>,
    result: Option<serde_json::Value>,
    spec: StoredOperationSpec,
}

#[derive(Debug, Clone)]
enum StoredOperationSpec {
    ApplyMessages(ApplyMessagesOperation),
    UpdateFlags(UpdateFlagsOperation),
    ManageMailbox(ManageMailboxOperation),
}

#[derive(Debug, Clone)]
struct ApplyMessagesOperation {
    account_id: String,
    action: MessageActionInput,
    message_ids: Vec<MessageId>,
    groups: Vec<MessageMutationGroup>,
    next_group_index: usize,
    result_by_id: BTreeMap<String, MessageMutationResult>,
}

#[derive(Debug, Clone)]
struct UpdateFlagsOperation {
    account_id: String,
    request: FlagUpdateRequest,
    message_ids: Vec<MessageId>,
    groups: Vec<MessageMutationGroup>,
    next_group_index: usize,
    result_by_id: BTreeMap<String, MessageMutationResult>,
}

#[derive(Debug, Clone)]
struct ManageMailboxOperation {
    account_id: String,
    action: MailboxAction,
    completed: bool,
    result: Option<MailboxManagementResult>,
}

#[derive(Debug, Clone)]
enum OperationStep {
    ApplyMessagesGroup {
        account_id: String,
        action: MessageActionInput,
        group: MessageMutationGroup,
    },
    UpdateFlagsGroup {
        account_id: String,
        request: FlagUpdateRequest,
        group: MessageMutationGroup,
    },
    ManageMailbox {
        account_id: String,
        action: MailboxAction,
    },
}

impl OperationStep {
    fn account_id(&self) -> &str {
        match self {
            Self::ApplyMessagesGroup { account_id, .. }
            | Self::UpdateFlagsGroup { account_id, .. }
            | Self::ManageMailbox { account_id, .. } => account_id,
        }
    }
}

#[derive(Debug)]
enum OperationStepOutcome {
    MessageResults(Vec<MessageMutationResult>),
    MailboxResult(MailboxManagementResult),
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

        let msg_id = parse_and_validate_message_id(&input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&msg_id.account_id)?;
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
                    &msg_id.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": msg_id.account_id,
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
                    &msg_id.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": msg_id.account_id,
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
                    &msg_id.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": msg_id.account_id,
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
                &msg_id.account_id,
                &msg_id.mailbox,
                msg_id.uidvalidity,
                msg_id.uid,
            ),
            message_raw_uri: build_message_raw_uri(
                &msg_id.account_id,
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
            &msg_id.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": status,
            "issues": issues,
            "account_id": msg_id.account_id,
            "message": detail,
        }))
    }

    async fn get_message_raw_impl(
        &self,
        input: GetMessageRawInput,
    ) -> AppResult<serde_json::Value> {
        validate_chars(input.max_bytes, 1_024, 1_000_000, "max_bytes")?;

        let msg_id = parse_and_validate_message_id(&input.message_id)?;
        let encoded_message_id = msg_id.encode();

        let account = self.config.get_account(&msg_id.account_id)?;
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
                    &msg_id.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": msg_id.account_id,
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
                    &msg_id.account_id,
                    Some(&msg_id.mailbox),
                    &issues,
                );
                return Ok(serde_json::json!({
                    "status": "failed",
                    "issues": issues,
                    "account_id": msg_id.account_id,
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
            &msg_id.account_id,
            Some(&msg_id.mailbox),
            &issues,
        );

        Ok(serde_json::json!({
            "status": "ok",
            "issues": issues,
            "account_id": msg_id.account_id,
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
        let action = build_message_action(&input)?;
        validate_message_action(&action)?;
        let (account_id, message_ids) = parse_bulk_message_ids(&input.message_ids)?;
        let spec = self
            .preflight_apply_message_operation(&account_id, action, message_ids)
            .await?;
        self.start_write_operation(spec).await
    }

    async fn update_message_flags_impl(
        &self,
        input: UpdateMessageFlagsInput,
    ) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        let request = build_flag_update_request(&input)?;
        validate_flag_update_request(&request)?;
        let (account_id, message_ids) = parse_bulk_message_ids(&input.message_ids)?;
        let spec = self
            .preflight_flag_operation(&account_id, request, message_ids)
            .await?;
        self.start_write_operation(spec).await
    }

    async fn manage_mailbox_impl(&self, input: ManageMailboxInput) -> AppResult<serde_json::Value> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        let action = build_mailbox_action(&input)?;
        let spec = self
            .preflight_manage_mailbox_operation(&input.account_id, action)
            .await?;
        self.start_write_operation(spec).await
    }

    async fn get_operation_impl(&self, input: OperationIdInput) -> AppResult<serde_json::Value> {
        validate_operation_id(&input.operation_id)?;
        self.ensure_operation_worker_running(&input.operation_id)
            .await?;
        self.operation_response(&input.operation_id).await
    }

    async fn cancel_operation_impl(&self, input: OperationIdInput) -> AppResult<serde_json::Value> {
        validate_operation_id(&input.operation_id)?;
        {
            let mut operations = self.operations.lock().await;
            let operation = operations.get_mut(&input.operation_id).ok_or_else(|| {
                AppError::NotFound(format!("operation '{}' not found", input.operation_id))
            })?;
            if !operation.state.is_terminal() {
                operation.state = OperationState::CancelRequested;
                operation.progress.phase = "cancel_requested".to_owned();
            }
        }
        self.ensure_operation_worker_running(&input.operation_id)
            .await?;
        self.operation_response(&input.operation_id).await
    }

    async fn preflight_apply_message_operation(
        &self,
        account_id: &str,
        action: MessageActionInput,
        message_ids: Vec<MessageId>,
    ) -> AppResult<StoredOperationSpec> {
        let groups = group_message_ids(&message_ids);
        if let Some(destination_mailbox) = destination_mailbox_for_action(&action) {
            self.ensure_mailbox_exists(account_id, destination_mailbox)
                .await?;
        }
        for group in &groups {
            if let MessageActionInput::Move {
                destination_mailbox,
            } = &action
                && normalize_mailbox_name(&group.mailbox)
                    == normalize_mailbox_name(destination_mailbox)
            {
                return Err(AppError::InvalidInput(
                    "destination_mailbox must differ from source mailbox for move".to_owned(),
                ));
            }
            self.connect_group_session(account_id, group, false)
                .await
                .map(|_| ())
                .map_err(|(_, error)| error)?;
        }
        Ok(StoredOperationSpec::ApplyMessages(ApplyMessagesOperation {
            account_id: account_id.to_owned(),
            action,
            message_ids,
            groups,
            next_group_index: 0,
            result_by_id: BTreeMap::new(),
        }))
    }

    async fn preflight_flag_operation(
        &self,
        account_id: &str,
        request: FlagUpdateRequest,
        message_ids: Vec<MessageId>,
    ) -> AppResult<StoredOperationSpec> {
        let groups = group_message_ids(&message_ids);
        for group in &groups {
            self.connect_group_session(account_id, group, false)
                .await
                .map(|_| ())
                .map_err(|(_, error)| error)?;
        }
        Ok(StoredOperationSpec::UpdateFlags(UpdateFlagsOperation {
            account_id: account_id.to_owned(),
            request,
            message_ids,
            groups,
            next_group_index: 0,
            result_by_id: BTreeMap::new(),
        }))
    }

    async fn preflight_manage_mailbox_operation(
        &self,
        account_id: &str,
        action: MailboxAction,
    ) -> AppResult<StoredOperationSpec> {
        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        match &action {
            MailboxAction::Create { mailbox } => {
                validate_mailbox(mailbox)?;
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
                imap::select_mailbox_readonly(&self.config, &mut session, mailbox).await?;
            }
            MailboxAction::Delete { mailbox } => {
                validate_mailbox(mailbox)?;
                imap::select_mailbox_readonly(&self.config, &mut session, mailbox).await?;
            }
        }
        Ok(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
            account_id: account_id.to_owned(),
            action,
            completed: false,
            result: None,
        }))
    }

    async fn start_write_operation(
        &self,
        spec: StoredOperationSpec,
    ) -> AppResult<serde_json::Value> {
        let operation_id = self.create_operation(spec).await;
        let deadline = Instant::now() + Duration::from_millis(WRITE_INLINE_BUDGET_MS);
        self.run_operation_until(&operation_id, Some(deadline))
            .await?;
        self.ensure_operation_worker_running(&operation_id).await?;
        self.operation_response(&operation_id).await
    }

    async fn create_operation(&self, spec: StoredOperationSpec) -> String {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let kind = operation_kind_label(&spec).to_owned();
        let total_units = operation_total_units(&spec);
        let operation = StoredOperation {
            operation_id: operation_id.clone(),
            kind,
            state: OperationState::Pending,
            created_at: now_utc_string(),
            started_at: None,
            finished_at: None,
            cancel_supported: true,
            worker_started: false,
            progress: OperationProgress::new(total_units, "pending"),
            issues: Vec::new(),
            result: None,
            spec,
        };
        let mut operations = self.operations.lock().await;
        operations.insert(operation_id.clone(), operation);
        operation_id
    }

    fn spawn_operation_worker(&self, operation_id: String) {
        let server = self.clone();
        tokio::spawn(async move {
            server.run_operation_worker(operation_id).await;
        });
    }

    async fn run_operation_worker(&self, operation_id: String) {
        let result = self.run_operation_until(&operation_id, None).await;
        if let Err(error) = self.clear_operation_worker_started(&operation_id).await {
            error!(
                operation_id = %operation_id,
                error = %error,
                "failed clearing operation worker state"
            );
        }
        if let Err(error) = result {
            error!(
                operation_id = %operation_id,
                error = %error,
                "background operation worker exited with error"
            );
            if let Err(fail_error) = self.fail_operation(&operation_id, &error).await {
                error!(
                    operation_id = %operation_id,
                    error = %fail_error,
                    "failed to mark operation as failed after worker error"
                );
            }
        }
    }

    async fn run_operation_until(
        &self,
        operation_id: &str,
        deadline: Option<Instant>,
    ) -> AppResult<()> {
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(());
            }
            let step = {
                let mut operations = self.operations.lock().await;
                let operation = operations.get_mut(operation_id).ok_or_else(|| {
                    AppError::NotFound(format!("operation '{operation_id}' not found"))
                })?;
                if operation.state.is_terminal() {
                    return Ok(());
                }
                if operation.started_at.is_none() {
                    operation.started_at = Some(now_utc_string());
                }
                if operation.state != OperationState::CancelRequested {
                    operation.state = OperationState::Running;
                }
                next_operation_step(operation)
            };

            let Some(step) = step else {
                self.finalize_operation(operation_id).await?;
                return Ok(());
            };

            let account_lock = self.account_write_lock(step.account_id()).await;
            let _account_guard = account_lock.lock().await;
            if self.operation_cancel_requested(operation_id).await? {
                self.finalize_operation(operation_id).await?;
                return Ok(());
            }
            let outcome = self.execute_operation_step(&step).await;
            self.apply_operation_step_outcome(operation_id, outcome)
                .await?;
        }
    }

    async fn operation_cancel_requested(&self, operation_id: &str) -> AppResult<bool> {
        let operations = self.operations.lock().await;
        let operation = operations
            .get(operation_id)
            .ok_or_else(|| AppError::NotFound(format!("operation '{operation_id}' not found")))?;
        Ok(operation.state == OperationState::CancelRequested)
    }

    async fn execute_operation_step(&self, step: &OperationStep) -> OperationStepOutcome {
        match step {
            OperationStep::ApplyMessagesGroup {
                account_id,
                action,
                group,
            } => {
                let results = match action {
                    MessageActionInput::Move {
                        destination_mailbox,
                    } => {
                        self.execute_move_group(account_id, group, destination_mailbox)
                            .await
                    }
                    MessageActionInput::Copy {
                        destination_mailbox,
                    } => {
                        self.execute_copy_group(account_id, group, destination_mailbox)
                            .await
                    }
                    MessageActionInput::Delete => {
                        self.execute_delete_group(account_id, group).await
                    }
                };
                OperationStepOutcome::MessageResults(results)
            }
            OperationStep::UpdateFlagsGroup {
                account_id,
                request,
                group,
            } => OperationStepOutcome::MessageResults(
                self.execute_flag_update_group(account_id, group, request)
                    .await,
            ),
            OperationStep::ManageMailbox { account_id, action } => {
                OperationStepOutcome::MailboxResult(
                    self.execute_manage_mailbox_action(account_id, action).await,
                )
            }
        }
    }

    async fn apply_operation_step_outcome(
        &self,
        operation_id: &str,
        outcome: OperationStepOutcome,
    ) -> AppResult<()> {
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| AppError::NotFound(format!("operation '{operation_id}' not found")))?;
        match (&mut operation.spec, outcome) {
            (
                StoredOperationSpec::ApplyMessages(spec),
                OperationStepOutcome::MessageResults(results),
            ) => {
                let had_failures = results.iter().any(|result| result.status != "ok");
                for result in results {
                    operation.issues.extend(result.issues.iter().cloned());
                    spec.result_by_id.insert(result.message_id.clone(), result);
                }
                spec.next_group_index += 1;
                operation.progress.completed_units += 1;
                if had_failures {
                    operation.progress.failed_units += 1;
                }
            }
            (
                StoredOperationSpec::UpdateFlags(spec),
                OperationStepOutcome::MessageResults(results),
            ) => {
                let had_failures = results.iter().any(|result| result.status != "ok");
                for result in results {
                    operation.issues.extend(result.issues.iter().cloned());
                    spec.result_by_id.insert(result.message_id.clone(), result);
                }
                spec.next_group_index += 1;
                operation.progress.completed_units += 1;
                if had_failures {
                    operation.progress.failed_units += 1;
                }
            }
            (
                StoredOperationSpec::ManageMailbox(spec),
                OperationStepOutcome::MailboxResult(result),
            ) => {
                operation.issues.extend(result.issues.iter().cloned());
                spec.completed = true;
                spec.result = Some(result.clone());
                operation.progress.completed_units = 1;
                if result.status != "ok" {
                    operation.progress.failed_units = 1;
                }
            }
            _ => {
                return Err(AppError::Internal(
                    "operation step outcome did not match stored operation type".to_owned(),
                ));
            }
        }
        operation.progress.remaining_units = operation
            .progress
            .total_units
            .saturating_sub(operation.progress.completed_units);
        Ok(())
    }

    async fn finalize_operation(&self, operation_id: &str) -> AppResult<()> {
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| AppError::NotFound(format!("operation '{operation_id}' not found")))?;
        if operation.state.is_terminal() {
            return Ok(());
        }

        let was_cancel_requested = operation.state == OperationState::CancelRequested;
        let result = match &mut operation.spec {
            StoredOperationSpec::ApplyMessages(spec) => {
                if was_cancel_requested {
                    append_canceled_message_results(
                        &mut spec.result_by_id,
                        &spec.groups[spec.next_group_index..],
                        destination_mailbox_for_action(&spec.action),
                    );
                }
                let ordered = order_group_results(&spec.message_ids, spec.result_by_id.clone());
                build_bulk_message_response(
                    spec.account_id.clone(),
                    Some(message_action_name(&spec.action)),
                    None,
                    ordered,
                )?
            }
            StoredOperationSpec::UpdateFlags(spec) => {
                if was_cancel_requested {
                    append_canceled_message_results(
                        &mut spec.result_by_id,
                        &spec.groups[spec.next_group_index..],
                        None,
                    );
                }
                let ordered = order_group_results(&spec.message_ids, spec.result_by_id.clone());
                build_bulk_message_response(
                    spec.account_id.clone(),
                    None,
                    Some(flag_operation_name(&spec.request.operation)),
                    ordered,
                )?
            }
            StoredOperationSpec::ManageMailbox(spec) => {
                if was_cancel_requested && !spec.completed {
                    let result = canceled_mailbox_result(&spec.account_id, &spec.action);
                    operation.issues.extend(result.issues.iter().cloned());
                    spec.result = Some(result);
                    spec.completed = true;
                }
                serde_json::to_value(spec.result.clone().ok_or_else(|| {
                    AppError::Internal("missing mailbox operation result".to_owned())
                })?)
                .map_err(serialization_error)?
            }
        };

        if was_cancel_requested {
            operation.state = OperationState::Canceled;
        } else {
            let result_status = result["status"].as_str().unwrap_or("failed");
            operation.state = match result_status {
                "ok" => OperationState::Ok,
                "partial" => OperationState::Partial,
                "failed" => OperationState::Failed,
                _ => OperationState::Failed,
            };
        }
        operation.progress.phase = operation.state.state_label().to_owned();
        operation.progress.current_mailbox = None;
        operation.progress.remaining_units = 0;
        operation.finished_at = Some(now_utc_string());
        operation.worker_started = false;
        if let Ok(issues) = serde_json::from_value::<Vec<ToolIssue>>(result["issues"].clone()) {
            operation.issues = issues;
        }
        operation.result = Some(result);
        Ok(())
    }

    async fn operation_response(&self, operation_id: &str) -> AppResult<serde_json::Value> {
        let operation = {
            let operations = self.operations.lock().await;
            operations.get(operation_id).cloned().ok_or_else(|| {
                AppError::NotFound(format!("operation '{operation_id}' not found"))
            })?
        };

        let mut response = serde_json::json!({
            "status": operation.state.status_label(),
            "issues": operation.issues,
            "operation": {
                "operation_id": operation.operation_id,
                "kind": operation.kind,
                "state": operation.state.state_label(),
                "done": operation.state.is_terminal(),
                "cancel_supported": operation.cancel_supported,
                "created_at": operation.created_at,
                "started_at": operation.started_at,
                "finished_at": operation.finished_at,
                "progress": operation.progress,
            },
            "result": operation.result.unwrap_or(serde_json::Value::Null),
        });
        if !operation.state.is_terminal() {
            response["next_action"] = serde_json::to_value(next_action_get_operation(operation_id))
                .map_err(serialization_error)?;
        }
        Ok(response)
    }

    async fn ensure_mailbox_exists(&self, account_id: &str, mailbox: &str) -> AppResult<()> {
        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        imap::select_mailbox_readonly(&self.config, &mut session, mailbox)
            .await
            .map(|_| ())
    }

    async fn account_write_lock(&self, account_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.account_write_locks.lock().await;
        locks
            .entry(account_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn ensure_operation_worker_running(&self, operation_id: &str) -> AppResult<()> {
        if self.try_mark_operation_worker_started(operation_id).await? {
            self.spawn_operation_worker(operation_id.to_owned());
        }
        Ok(())
    }

    async fn try_mark_operation_worker_started(&self, operation_id: &str) -> AppResult<bool> {
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| AppError::NotFound(format!("operation '{operation_id}' not found")))?;
        if operation.state.is_terminal() || operation.worker_started {
            return Ok(false);
        }
        operation.worker_started = true;
        Ok(true)
    }

    async fn clear_operation_worker_started(&self, operation_id: &str) -> AppResult<()> {
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| AppError::NotFound(format!("operation '{operation_id}' not found")))?;
        operation.worker_started = false;
        Ok(())
    }

    async fn fail_operation(&self, operation_id: &str, error: &AppError) -> AppResult<()> {
        let failure_issue = ToolIssue::from_error("operation_worker", error);
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| AppError::NotFound(format!("operation '{operation_id}' not found")))?;
        if operation.state.is_terminal() {
            return Ok(());
        }

        let result = match &mut operation.spec {
            StoredOperationSpec::ApplyMessages(spec) => {
                append_failed_message_results(
                    &mut spec.result_by_id,
                    &spec.groups[spec.next_group_index..],
                    &failure_issue,
                    destination_mailbox_for_action(&spec.action),
                );
                let ordered = order_group_results(&spec.message_ids, spec.result_by_id.clone());
                build_bulk_message_response(
                    spec.account_id.clone(),
                    Some(message_action_name(&spec.action)),
                    None,
                    ordered,
                )?
            }
            StoredOperationSpec::UpdateFlags(spec) => {
                append_failed_message_results(
                    &mut spec.result_by_id,
                    &spec.groups[spec.next_group_index..],
                    &failure_issue,
                    None,
                );
                let ordered = order_group_results(&spec.message_ids, spec.result_by_id.clone());
                build_bulk_message_response(
                    spec.account_id.clone(),
                    None,
                    Some(flag_operation_name(&spec.request.operation)),
                    ordered,
                )?
            }
            StoredOperationSpec::ManageMailbox(spec) => {
                if !spec.completed {
                    let (action_name, mailbox, destination_mailbox) =
                        mailbox_action_display(&spec.action);
                    let result = MailboxManagementResult {
                        status: "failed".to_owned(),
                        issues: vec![failure_issue.clone()],
                        account_id: spec.account_id.clone(),
                        action: action_name.to_owned(),
                        mailbox,
                        destination_mailbox,
                    };
                    spec.result = Some(result);
                    spec.completed = true;
                }
                serde_json::to_value(spec.result.clone().ok_or_else(|| {
                    AppError::Internal("missing mailbox operation result".to_owned())
                })?)
                .map_err(serialization_error)?
            }
        };

        operation.state = OperationState::Failed;
        operation.progress.phase = operation.state.state_label().to_owned();
        operation.progress.current_mailbox = None;
        operation.progress.failed_units = operation.progress.failed_units.max(
            operation
                .progress
                .total_units
                .saturating_sub(operation.progress.completed_units),
        );
        operation.progress.remaining_units = 0;
        operation.finished_at = Some(now_utc_string());
        operation.worker_started = false;
        if let Ok(issues) = serde_json::from_value::<Vec<ToolIssue>>(result["issues"].clone()) {
            operation.issues = issues;
        } else {
            operation.issues = vec![failure_issue];
        }
        operation.result = Some(result);
        Ok(())
    }

    async fn execute_manage_mailbox_action(
        &self,
        account_id: &str,
        action: &MailboxAction,
    ) -> MailboxManagementResult {
        let (action_name, mailbox, destination_mailbox) = mailbox_action_display(action);
        let account = match self.config.get_account(account_id) {
            Ok(account) => account,
            Err(error) => {
                return MailboxManagementResult {
                    status: "failed".to_owned(),
                    issues: vec![ToolIssue::from_error("account_lookup", &error)],
                    account_id: account_id.to_owned(),
                    action: action_name.to_owned(),
                    mailbox,
                    destination_mailbox,
                };
            }
        };

        let mut session = match imap::connect_authenticated(&self.config, account).await {
            Ok(session) => session,
            Err(error) => {
                return MailboxManagementResult {
                    status: "failed".to_owned(),
                    issues: vec![ToolIssue::from_error("connect_authenticated", &error)],
                    account_id: account_id.to_owned(),
                    action: action_name.to_owned(),
                    mailbox,
                    destination_mailbox,
                };
            }
        };

        let mut issues = Vec::new();
        let operation = match action {
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
            issues.push(ToolIssue::from_error(mailbox_action_stage(action), &error));
        }

        let status = status_from_issue_and_counts(&issues, issues.is_empty()).to_owned();
        log_runtime_issues(
            "imap_manage_mailbox",
            &status,
            account_id,
            Some(&mailbox),
            &issues,
        );
        MailboxManagementResult {
            status,
            issues,
            account_id: account_id.to_owned(),
            action: action_name.to_owned(),
            mailbox,
            destination_mailbox,
        }
    }

    async fn execute_copy_group(
        &self,
        account_id: &str,
        group: &MessageMutationGroup,
        destination_mailbox: &str,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        let mut session = match self.connect_group_session(account_id, group, false).await {
            Ok(session) => session,
            Err((stage, error)) => {
                return failed_group_results(group, stage, &error, Some(destination_mailbox), None);
            }
        };
        let result = imap::uid_copy_sequence(
            &self.config,
            &mut session,
            uid_set.as_str(),
            destination_mailbox,
        )
        .await;
        let issues = result
            .err()
            .map(|error| group_issues(group, "uid_copy", &error))
            .unwrap_or_default();
        finalize_group_results(group, issues, Some(destination_mailbox), None, true)
    }

    async fn execute_move_group(
        &self,
        account_id: &str,
        group: &MessageMutationGroup,
        destination_mailbox: &str,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        let mut session = match self.connect_group_session(account_id, group, false).await {
            Ok(session) => session,
            Err((stage, error)) => {
                return failed_group_results(group, stage, &error, Some(destination_mailbox), None);
            }
        };

        let mut issues = Vec::new();
        let caps = match imap::capabilities(&self.config, &mut session).await {
            Ok(caps) => caps,
            Err(error) => {
                return failed_group_results(
                    group,
                    "capabilities",
                    &error,
                    Some(destination_mailbox),
                    None,
                );
            }
        };

        if caps.has_str("MOVE") {
            if let Err(error) = imap::uid_move_sequence(
                &self.config,
                &mut session,
                uid_set.as_str(),
                destination_mailbox,
            )
            .await
            {
                issues.extend(group_issues(group, "uid_move", &error));
            }
            return finalize_group_results(group, issues, Some(destination_mailbox), None, true);
        }

        if let Err(error) = imap::uid_copy_sequence(
            &self.config,
            &mut session,
            uid_set.as_str(),
            destination_mailbox,
        )
        .await
        {
            return finalize_group_results(
                group,
                group_issues(group, "uid_copy", &error),
                Some(destination_mailbox),
                None,
                true,
            );
        }

        if let Err(error) = imap::uid_store_sequence(
            &self.config,
            &mut session,
            uid_set.as_str(),
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        {
            issues.extend(group_issues(group, "uid_store_deleted", &error));
            return finalize_group_results(group, issues, Some(destination_mailbox), None, true);
        }

        if let Err(error) =
            imap::uid_expunge_sequence(&self.config, &mut session, uid_set.as_str()).await
        {
            issues.extend(group_issues(group, "uid_expunge", &error));
        }
        finalize_group_results(group, issues, Some(destination_mailbox), None, true)
    }

    async fn execute_delete_group(
        &self,
        account_id: &str,
        group: &MessageMutationGroup,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        let mut session = match self.connect_group_session(account_id, group, false).await {
            Ok(session) => session,
            Err((stage, error)) => return failed_group_results(group, stage, &error, None, None),
        };

        let mut issues = Vec::new();
        if let Err(error) = imap::uid_store_sequence(
            &self.config,
            &mut session,
            uid_set.as_str(),
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        {
            issues.extend(group_issues(group, "uid_store_deleted", &error));
            return finalize_group_results(group, issues, None, None, true);
        }

        if let Err(error) =
            imap::uid_expunge_sequence(&self.config, &mut session, uid_set.as_str()).await
        {
            issues.extend(group_issues(group, "uid_expunge", &error));
        }

        finalize_group_results(group, issues, None, None, true)
    }

    async fn execute_flag_update_group(
        &self,
        account_id: &str,
        group: &MessageMutationGroup,
        request: &FlagUpdateRequest,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        let mut session = match self.connect_group_session(account_id, group, false).await {
            Ok(session) => session,
            Err((stage, error)) => return failed_group_results(group, stage, &error, None, None),
        };

        let query = match request.operation {
            FlagOperation::Add => format!("+FLAGS.SILENT ({})", request.flags.join(" ")),
            FlagOperation::Remove => format!("-FLAGS.SILENT ({})", request.flags.join(" ")),
            FlagOperation::Replace => format!("FLAGS.SILENT ({})", request.flags.join(" ")),
        };

        let mut issues = Vec::new();
        if let Err(error) =
            imap::uid_store_sequence(&self.config, &mut session, uid_set.as_str(), &query).await
        {
            issues.extend(group_issues(group, "uid_store_flags", &error));
            return finalize_group_results(group, issues, None, None, false);
        }

        let fetched_flags = match imap::fetch_flags_by_uid_set(
            &self.config,
            &mut session,
            uid_set.as_str(),
        )
        .await
        {
            Ok(flags) => Some(flags),
            Err(error) => {
                issues.extend(group_issues(group, "fetch_flags", &error));
                None
            }
        };

        let mut results = Vec::with_capacity(group.entries.len());
        for msg_id in &group.entries {
            let encoded_message_id = msg_id.encode();
            let mut message_issues = issues
                .iter()
                .filter(|issue| issue.message_id.as_deref() == Some(encoded_message_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let flags = fetched_flags
                .as_ref()
                .and_then(|by_uid| by_uid.get(&msg_id.uid).cloned());
            let has_flags = flags.is_some();
            if fetched_flags.is_some() && !has_flags {
                message_issues.push(ToolIssue {
                    code: "internal".to_owned(),
                    stage: "fetch_flags".to_owned(),
                    message: format!("UID {} missing from fetch_flags response", msg_id.uid),
                    retryable: true,
                    uid: Some(msg_id.uid),
                    message_id: Some(encoded_message_id.clone()),
                });
            }
            results.push(finalize_message_result(
                msg_id,
                encoded_message_id,
                message_issues,
                None,
                flags,
                has_flags,
            ));
        }
        results
    }

    async fn connect_group_session(
        &self,
        account_id: &str,
        group: &MessageMutationGroup,
        readonly: bool,
    ) -> Result<imap::ImapSession, (&'static str, AppError)> {
        let account = self
            .config
            .get_account(account_id)
            .map_err(|error| ("account_lookup", error))?;
        let mut session = imap::connect_authenticated(&self.config, account)
            .await
            .map_err(|error| ("connect_authenticated", error))?;
        let current_uidvalidity = if readonly {
            imap::select_mailbox_readonly(&self.config, &mut session, &group.mailbox)
                .await
                .map_err(|error| ("select_mailbox_readonly", error))?
        } else {
            imap::select_mailbox_readwrite(&self.config, &mut session, &group.mailbox)
                .await
                .map_err(|error| ("select_mailbox_readwrite", error))?
        };
        if current_uidvalidity != group.uidvalidity {
            return Err((
                if readonly {
                    "select_mailbox_readonly"
                } else {
                    "select_mailbox_readwrite"
                },
                AppError::Conflict("message uidvalidity no longer matches mailbox".to_owned()),
            ));
        }
        Ok(session)
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
        }),
    )
}

fn next_action_get_operation(operation_id: &str) -> NextAction {
    next_action(
        "Poll the operation until it reaches a terminal state.",
        "imap_get_operation",
        serde_json::json!({
            "operation_id": operation_id,
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

/// Parse message_id and validate its mailbox component.
fn parse_and_validate_message_id(message_id: &str) -> AppResult<MessageId> {
    let msg_id = MessageId::parse(message_id)?;
    validate_mailbox(&msg_id.mailbox)?;
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

struct SearchSnapshot {
    uids_desc: Vec<u32>,
    offset: usize,
    snippet_max_chars: Option<usize>,
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
        snippet_max_chars: input.snippet_max_chars.map(|value| value.clamp(50, 500)),
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

        let snippet = options
            .snippet_max_chars
            .and_then(|max_chars| subject.clone().map(|s| mime::truncate_chars(s, max_chars)));

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
    snippet_max_chars: Option<usize>,
}

fn serialization_error(error: serde_json::Error) -> AppError {
    AppError::Internal(format!("serialization failure: {error}"))
}

fn message_action_name(action: &MessageActionInput) -> &'static str {
    match action {
        MessageActionInput::Move { .. } => "move",
        MessageActionInput::Copy { .. } => "copy",
        MessageActionInput::Delete => "delete",
    }
}

fn build_message_action(input: &ApplyToMessagesInput) -> AppResult<MessageActionInput> {
    match input.action.as_str() {
        "move" => {
            let destination_mailbox = required_mailbox_field(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "move",
            )?;
            Ok(MessageActionInput::Move {
                destination_mailbox,
            })
        }
        "copy" => {
            let destination_mailbox = required_mailbox_field(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "copy",
            )?;
            Ok(MessageActionInput::Copy {
                destination_mailbox,
            })
        }
        "delete" => {
            reject_field_for_action(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "delete",
            )?;
            Ok(MessageActionInput::Delete)
        }
        _ => Err(AppError::InvalidInput(format!(
            "action must be one of move, copy, delete; got '{}'",
            input.action
        ))),
    }
}

fn flag_operation_name(operation: &FlagOperation) -> &'static str {
    match operation {
        FlagOperation::Add => "add",
        FlagOperation::Remove => "remove",
        FlagOperation::Replace => "replace",
    }
}

fn build_flag_update_request(input: &UpdateMessageFlagsInput) -> AppResult<FlagUpdateRequest> {
    let operation = match input.operation.as_str() {
        "add" => FlagOperation::Add,
        "remove" => FlagOperation::Remove,
        "replace" => FlagOperation::Replace,
        _ => {
            return Err(AppError::InvalidInput(format!(
                "operation must be one of add, remove, replace; got '{}'",
                input.operation
            )));
        }
    };
    Ok(FlagUpdateRequest {
        operation,
        flags: input.flags.clone(),
    })
}

fn build_mailbox_action(input: &ManageMailboxInput) -> AppResult<MailboxAction> {
    match input.action.as_str() {
        "create" => {
            reject_field_for_action(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "create",
            )?;
            Ok(MailboxAction::Create {
                mailbox: input.mailbox.clone(),
            })
        }
        "rename" => Ok(MailboxAction::Rename {
            mailbox: input.mailbox.clone(),
            destination_mailbox: required_mailbox_field(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "rename",
            )?,
        }),
        "delete" => {
            reject_field_for_action(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "delete",
            )?;
            Ok(MailboxAction::Delete {
                mailbox: input.mailbox.clone(),
            })
        }
        _ => Err(AppError::InvalidInput(format!(
            "action must be one of create, rename, delete; got '{}'",
            input.action
        ))),
    }
}

fn required_mailbox_field(value: Option<&String>, field: &str, action: &str) -> AppResult<String> {
    match value {
        Some(value) => Ok(value.clone()),
        None => Err(AppError::InvalidInput(format!(
            "{field} is required for action={action}"
        ))),
    }
}

fn reject_field_for_action<T>(value: Option<&T>, field: &str, action: &str) -> AppResult<()> {
    if value.is_some() {
        return Err(AppError::InvalidInput(format!(
            "{field} is not allowed for action={action}"
        )));
    }
    Ok(())
}

fn validate_message_action(action: &MessageActionInput) -> AppResult<()> {
    match action {
        MessageActionInput::Move {
            destination_mailbox,
        } => validate_mailbox(destination_mailbox),
        MessageActionInput::Copy {
            destination_mailbox,
        } => {
            validate_mailbox(destination_mailbox)?;
            Ok(())
        }
        MessageActionInput::Delete => Ok(()),
    }
}

fn validate_flag_update_request(request: &FlagUpdateRequest) -> AppResult<()> {
    if request.flags.is_empty() {
        return Err(AppError::InvalidInput(
            "flags must contain at least one entry".to_owned(),
        ));
    }
    validate_flags(&request.flags, "flags")
}

fn parse_bulk_message_ids(message_ids: &[String]) -> AppResult<(String, Vec<MessageId>)> {
    if message_ids.is_empty() {
        return Err(AppError::InvalidInput(
            "message_ids must contain at least one entry".to_owned(),
        ));
    }
    if message_ids.len() > MAX_BULK_MESSAGE_IDS {
        return Err(AppError::InvalidInput(format!(
            "message_ids must contain at most {MAX_BULK_MESSAGE_IDS} entries"
        )));
    }
    dedupe_and_parse_message_ids(message_ids)
}

fn dedupe_and_parse_message_ids(message_ids: &[String]) -> AppResult<(String, Vec<MessageId>)> {
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    let mut account_id: Option<String> = None;
    for message_id in message_ids {
        let parsed = parse_and_validate_message_id(message_id)?;
        match &account_id {
            Some(existing) if existing != &parsed.account_id => {
                return Err(AppError::InvalidInput(
                    "message_ids must all belong to the same account".to_owned(),
                ));
            }
            Some(_) => {}
            None => account_id = Some(parsed.account_id.clone()),
        }
        let encoded = parsed.encode();
        if seen.insert(encoded) {
            resolved.push(parsed);
        }
    }
    let account_id = account_id.ok_or_else(|| {
        AppError::InvalidInput("message_ids must contain at least one entry".to_owned())
    })?;
    Ok((account_id, resolved))
}

fn failed_message_result(
    msg_id: &MessageId,
    encoded_message_id: String,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<String>,
    flags: Option<Vec<String>>,
) -> MessageMutationResult {
    MessageMutationResult {
        message_id: encoded_message_id,
        status: "failed".to_owned(),
        issues,
        source_mailbox: msg_id.mailbox.clone(),
        destination_mailbox,
        flags,
        new_message_id: None,
    }
}

fn finalize_message_result(
    msg_id: &MessageId,
    encoded_message_id: String,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<String>,
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
        flags,
        new_message_id: None,
    }
}

fn build_bulk_message_response(
    account_id: String,
    action: Option<&str>,
    operation: Option<&str>,
    results: Vec<MessageMutationResult>,
) -> AppResult<serde_json::Value> {
    let matched = results.len();
    let mut issues = Vec::new();
    let mut succeeded = 0usize;
    for result in &results {
        if result.status == "ok" {
            succeeded += 1;
        }
        issues.extend(result.issues.iter().cloned());
    }
    let failed = matched.saturating_sub(succeeded);
    let status = status_from_issue_and_counts(&issues, succeeded > 0).to_owned();
    let mut response = serde_json::json!({
        "status": status,
        "issues": issues,
        "account_id": account_id,
        "matched": matched,
        "attempted": matched,
        "succeeded": succeeded,
        "failed": failed,
        "results": results,
    });
    if let Some(action) = action {
        response["action"] = serde_json::Value::String(action.to_owned());
    }
    if let Some(operation) = operation {
        response["operation"] = serde_json::Value::String(operation.to_owned());
    }
    Ok(response)
}

fn operation_summary(noun: &str, data: &serde_json::Value) -> String {
    let status = data["status"].as_str().unwrap_or("running");
    let kind = data["operation"]["kind"].as_str().unwrap_or(noun);
    let label = kind.strip_prefix("imap_").unwrap_or(kind);
    match status {
        "accepted" => format!("{label} accepted"),
        "running" => format!("{label} running"),
        "ok" => format!("{label} completed"),
        "partial" => format!("{label} completed with issues"),
        "failed" => format!("{label} failed"),
        "canceled" => format!("{label} canceled"),
        _ => format!("{label} {status}"),
    }
}

fn operation_kind_label(spec: &StoredOperationSpec) -> &'static str {
    match spec {
        StoredOperationSpec::ApplyMessages(_) => "imap_apply_to_messages",
        StoredOperationSpec::UpdateFlags(_) => "imap_update_message_flags",
        StoredOperationSpec::ManageMailbox(_) => "imap_manage_mailbox",
    }
}

fn operation_total_units(spec: &StoredOperationSpec) -> usize {
    match spec {
        StoredOperationSpec::ApplyMessages(spec) => spec.groups.len(),
        StoredOperationSpec::UpdateFlags(spec) => spec.groups.len(),
        StoredOperationSpec::ManageMailbox(_) => 1,
    }
}

fn next_operation_step(operation: &mut StoredOperation) -> Option<OperationStep> {
    if operation.state == OperationState::CancelRequested {
        return None;
    }
    match &mut operation.spec {
        StoredOperationSpec::ApplyMessages(spec) => {
            let group = spec.groups.get(spec.next_group_index)?.clone();
            operation.progress.current_mailbox = Some(group.mailbox.clone());
            operation.progress.phase = format!("processing_{}", message_action_name(&spec.action));
            Some(OperationStep::ApplyMessagesGroup {
                account_id: spec.account_id.clone(),
                action: spec.action.clone(),
                group,
            })
        }
        StoredOperationSpec::UpdateFlags(spec) => {
            let group = spec.groups.get(spec.next_group_index)?.clone();
            operation.progress.current_mailbox = Some(group.mailbox.clone());
            operation.progress.phase = format!(
                "processing_{}",
                flag_operation_name(&spec.request.operation)
            );
            Some(OperationStep::UpdateFlagsGroup {
                account_id: spec.account_id.clone(),
                request: spec.request.clone(),
                group,
            })
        }
        StoredOperationSpec::ManageMailbox(spec) => {
            if spec.completed {
                return None;
            }
            let (_, mailbox, _) = mailbox_action_display(&spec.action);
            operation.progress.current_mailbox = Some(mailbox.clone());
            operation.progress.phase = format!("processing_{}", mailbox_action_name(&spec.action));
            Some(OperationStep::ManageMailbox {
                account_id: spec.account_id.clone(),
                action: spec.action.clone(),
            })
        }
    }
}

fn destination_mailbox_for_action(action: &MessageActionInput) -> Option<&str> {
    match action {
        MessageActionInput::Move {
            destination_mailbox,
        }
        | MessageActionInput::Copy {
            destination_mailbox,
        } => Some(destination_mailbox.as_str()),
        MessageActionInput::Delete => None,
    }
}

fn canceled_tool_issue(uid: Option<u32>, message_id: Option<String>) -> ToolIssue {
    ToolIssue {
        code: "canceled".to_owned(),
        stage: "operation_canceled".to_owned(),
        message: "operation was canceled before this item ran".to_owned(),
        retryable: false,
        uid,
        message_id,
    }
}

fn append_canceled_message_results(
    result_by_id: &mut BTreeMap<String, MessageMutationResult>,
    remaining_groups: &[MessageMutationGroup],
    destination_mailbox: Option<&str>,
) {
    for group in remaining_groups {
        for msg_id in &group.entries {
            let encoded = msg_id.encode();
            result_by_id.insert(
                encoded.clone(),
                failed_message_result(
                    msg_id,
                    encoded.clone(),
                    vec![canceled_tool_issue(Some(msg_id.uid), Some(encoded))],
                    destination_mailbox.map(ToOwned::to_owned),
                    None,
                ),
            );
        }
    }
}

fn append_failed_message_results(
    result_by_id: &mut BTreeMap<String, MessageMutationResult>,
    remaining_groups: &[MessageMutationGroup],
    issue: &ToolIssue,
    destination_mailbox: Option<&str>,
) {
    for group in remaining_groups {
        for msg_id in &group.entries {
            let encoded = msg_id.encode();
            result_by_id.insert(
                encoded.clone(),
                failed_message_result(
                    msg_id,
                    encoded,
                    vec![
                        issue
                            .clone()
                            .with_uid(msg_id.uid)
                            .with_message_id(&msg_id.encode()),
                    ],
                    destination_mailbox.map(ToOwned::to_owned),
                    None,
                ),
            );
        }
    }
}

fn mailbox_action_name(action: &MailboxAction) -> &'static str {
    match action {
        MailboxAction::Create { .. } => "create",
        MailboxAction::Rename { .. } => "rename",
        MailboxAction::Delete { .. } => "delete",
    }
}

fn mailbox_action_stage(action: &MailboxAction) -> &'static str {
    match action {
        MailboxAction::Create { .. } => "create_mailbox",
        MailboxAction::Rename { .. } => "rename_mailbox",
        MailboxAction::Delete { .. } => "delete_mailbox",
    }
}

fn mailbox_action_display(action: &MailboxAction) -> (&'static str, String, Option<String>) {
    match action {
        MailboxAction::Create { mailbox } => ("create", mailbox.clone(), None),
        MailboxAction::Rename {
            mailbox,
            destination_mailbox,
        } => ("rename", mailbox.clone(), Some(destination_mailbox.clone())),
        MailboxAction::Delete { mailbox } => ("delete", mailbox.clone(), None),
    }
}

fn canceled_mailbox_result(account_id: &str, action: &MailboxAction) -> MailboxManagementResult {
    let (action_name, mailbox, destination_mailbox) = mailbox_action_display(action);
    MailboxManagementResult {
        status: "failed".to_owned(),
        issues: vec![canceled_tool_issue(None, None)],
        account_id: account_id.to_owned(),
        action: action_name.to_owned(),
        mailbox,
        destination_mailbox,
    }
}

fn now_utc_string() -> String {
    Utc::now().to_rfc3339()
}

fn group_message_ids(message_ids: &[MessageId]) -> Vec<MessageMutationGroup> {
    let mut grouped = BTreeMap::<(String, u32), Vec<MessageId>>::new();
    for msg_id in message_ids {
        grouped
            .entry((msg_id.mailbox.clone(), msg_id.uidvalidity))
            .or_default()
            .push(msg_id.clone());
    }
    grouped
        .into_iter()
        .map(|((mailbox, uidvalidity), entries)| MessageMutationGroup {
            mailbox,
            uidvalidity,
            entries,
        })
        .collect()
}

fn build_uid_set(entries: &[MessageId]) -> String {
    let mut uids = entries.iter().map(|entry| entry.uid).collect::<Vec<_>>();
    uids.sort_unstable();
    let mut ranges = Vec::new();
    let mut start = uids[0];
    let mut end = uids[0];
    for uid in uids.into_iter().skip(1) {
        if uid == end + 1 {
            end = uid;
            continue;
        }
        ranges.push(format_uid_range(start, end));
        start = uid;
        end = uid;
    }
    ranges.push(format_uid_range(start, end));
    ranges.join(",")
}

fn format_uid_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

fn group_issues(group: &MessageMutationGroup, stage: &str, error: &AppError) -> Vec<ToolIssue> {
    group
        .entries
        .iter()
        .map(|msg_id| {
            ToolIssue::from_error(stage, error)
                .with_uid(msg_id.uid)
                .with_message_id(&msg_id.encode())
        })
        .collect()
}

fn failed_group_results(
    group: &MessageMutationGroup,
    stage: &str,
    error: &AppError,
    destination_mailbox: Option<&str>,
    flags: Option<Vec<String>>,
) -> Vec<MessageMutationResult> {
    group
        .entries
        .iter()
        .map(|msg_id| {
            failed_message_result(
                msg_id,
                msg_id.encode(),
                vec![
                    ToolIssue::from_error(stage, error)
                        .with_uid(msg_id.uid)
                        .with_message_id(&msg_id.encode()),
                ],
                destination_mailbox.map(ToOwned::to_owned),
                flags.clone(),
            )
        })
        .collect()
}

fn finalize_group_results(
    group: &MessageMutationGroup,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<&str>,
    flags: Option<Vec<String>>,
    success_on_no_issues: bool,
) -> Vec<MessageMutationResult> {
    group
        .entries
        .iter()
        .map(|msg_id| {
            let encoded = msg_id.encode();
            let message_issues = issues
                .iter()
                .filter(|issue| issue.message_id.as_deref() == Some(encoded.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            finalize_message_result(
                msg_id,
                encoded,
                message_issues,
                destination_mailbox.map(ToOwned::to_owned),
                flags.clone(),
                success_on_no_issues,
            )
        })
        .collect()
}

fn order_group_results(
    message_ids: &[MessageId],
    mut result_by_id: BTreeMap<String, MessageMutationResult>,
) -> Vec<MessageMutationResult> {
    message_ids
        .iter()
        .map(|msg_id| {
            let encoded = msg_id.encode();
            result_by_id.remove(&encoded).unwrap_or_else(|| {
                failed_message_result(
                    msg_id,
                    encoded,
                    vec![ToolIssue {
                        code: "internal".to_owned(),
                        stage: "result_assembly".to_owned(),
                        message: "missing mutation result for message".to_owned(),
                        retryable: true,
                        uid: Some(msg_id.uid),
                        message_id: Some(msg_id.encode()),
                    }],
                    None,
                    None,
                )
            })
        })
        .collect()
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

fn validate_operation_id(operation_id: &str) -> AppResult<()> {
    if operation_id.is_empty() || operation_id.len() > 64 {
        return Err(AppError::InvalidInput(
            "operation_id must be 1..64 chars".to_owned(),
        ));
    }
    validate_no_controls(operation_id, "operation_id")
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
        validate_flag(flag).map_err(|_| invalid_flag_error(field, flag))?;
    }
    Ok(())
}

fn validate_flag(flag: &str) -> AppResult<()> {
    if flag.is_empty() || flag.len() > 64 {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    let atom = match flag.strip_prefix('\\') {
        Some(rest) => {
            if rest.is_empty() || !VALID_SYSTEM_FLAGS.contains(&flag) {
                return Err(AppError::InvalidInput("invalid flag".to_owned()));
            }
            rest
        }
        None => flag,
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

fn invalid_flag_error(field: &str, flag: &str) -> AppError {
    AppError::InvalidInput(format!(
        "{field} contains invalid flag '{flag}'. Valid standard flags are {}. Custom keywords may also be allowed by the server if they are valid IMAP atoms.",
        VALID_SYSTEM_FLAGS.join(", ")
    ))
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
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::Mutex;

    use super::{
        FlagOperation, FlagUpdateRequest, MailImapServer, MailboxAction, ManageMailboxOperation,
        OperationState, StoredOperationSpec, build_flag_update_request, build_mailbox_action,
        build_message_action, dedupe_and_parse_message_ids, encode_raw_source_base64,
        escape_imap_quoted, next_action_for_search_result, parse_bulk_message_ids,
        resume_cursor_search, validate_flag, validate_flag_update_request, validate_mailbox,
        validate_search_input, validate_search_text,
    };
    use crate::config::ServerConfig;
    use crate::models::{
        ApplyToMessagesInput, ManageMailboxInput, MessageSummary, OperationIdInput,
        SearchMessagesInput, UpdateMessageFlagsInput, validate_client_safe_input_schema,
    };
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
    fn validate_flag_update_request_lists_valid_standard_flags() {
        let err = validate_flag_update_request(&FlagUpdateRequest {
            operation: FlagOperation::Add,
            flags: vec!["\\Read".to_owned()],
        })
        .expect_err("must reject unknown system flag");
        let message = err.to_string();
        assert!(message.contains("Valid standard flags are"));
        assert!(message.contains("\\Seen"));
        assert!(message.contains("\\Answered"));
        assert!(message.contains("\\Flagged"));
        assert!(message.contains("\\Deleted"));
        assert!(message.contains("\\Draft"));
    }

    #[test]
    fn validate_flag_update_request_rejects_empty_flags() {
        let err = validate_flag_update_request(&FlagUpdateRequest {
            operation: FlagOperation::Add,
            flags: Vec::new(),
        })
        .expect_err("must reject empty flags");
        assert!(
            err.to_string()
                .contains("flags must contain at least one entry")
        );
    }

    #[test]
    fn build_flag_update_request_rejects_unknown_operation() {
        let input = UpdateMessageFlagsInput {
            message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
            operation: "merge".to_owned(),
            flags: vec!["\\Seen".to_owned()],
        };

        let err = build_flag_update_request(&input).expect_err("must reject unknown operation");
        assert!(
            err.to_string()
                .contains("operation must be one of add, remove, replace")
        );
    }

    #[test]
    fn dedupe_and_parse_message_ids_removes_duplicates() {
        let message_ids = vec![
            "imap:default:INBOX:42:7".to_owned(),
            "imap:default:INBOX:42:7".to_owned(),
            "imap:default:INBOX:42:8".to_owned(),
        ];

        let (account_id, parsed) =
            dedupe_and_parse_message_ids(&message_ids).expect("message ids should parse");
        assert_eq!(account_id, "default");
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
    fn parse_bulk_message_ids_rejects_empty_input() {
        let err = parse_bulk_message_ids(&[]).expect_err("must reject empty message_ids");
        assert!(
            err.to_string()
                .contains("message_ids must contain at least one entry")
        );
    }

    #[test]
    fn build_message_action_requires_destination_for_move() {
        let input = ApplyToMessagesInput {
            message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
            action: "move".to_owned(),
            destination_mailbox: None,
        };

        let err = build_message_action(&input).expect_err("move must require destination_mailbox");
        assert!(
            err.to_string()
                .contains("destination_mailbox is required for action=move")
        );
    }

    #[test]
    fn build_message_action_requires_destination_for_copy() {
        let input = ApplyToMessagesInput {
            message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
            action: "copy".to_owned(),
            destination_mailbox: None,
        };

        let err = build_message_action(&input).expect_err("copy must require destination_mailbox");
        assert!(
            err.to_string()
                .contains("destination_mailbox is required for action=copy")
        );
    }

    #[test]
    fn build_mailbox_action_rejects_destination_for_delete() {
        let input = ManageMailboxInput {
            account_id: "default".to_owned(),
            action: "delete".to_owned(),
            mailbox: "Archive".to_owned(),
            destination_mailbox: Some("Archive/Elsewhere".to_owned()),
        };

        let err = build_mailbox_action(&input).expect_err("delete must reject destination_mailbox");
        assert!(
            err.to_string()
                .contains("destination_mailbox is not allowed for action=delete")
        );
    }

    #[test]
    fn all_published_tool_input_schemas_are_client_safe() {
        let server = MailImapServer::new(schema_test_server_config());
        for tool in server.tool_router.list_all() {
            let schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());
            validate_client_safe_input_schema(&schema).unwrap_or_else(|error| {
                panic!("tool {} published unsafe schema: {error}", tool.name)
            });
        }
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
            snippet_max_chars: None,
        };

        let err = validate_search_input(&input).expect_err("must reject conflicting date filters");
        assert!(
            err.to_string()
                .contains("last_days cannot be combined with start_date/end_date")
        );
    }

    #[test]
    fn validate_search_input_accepts_snippet_size_without_boolean_toggle() {
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
            snippet_max_chars: Some(200),
        };

        validate_search_input(&input).expect("snippet_max_chars alone should enable snippets");
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
                snippet_max_chars: Some(200),
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
                snippet_max_chars: Some(120),
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
            snippet_max_chars: None,
        };

        let snapshot = resume_cursor_search(&cursors, &input, 42, cursor_id)
            .await
            .expect("decoded cursor should resume from legacy encoded request");
        assert_eq!(snapshot.snippet_max_chars, Some(120));
    }

    #[tokio::test]
    async fn operation_response_includes_next_action_while_running() {
        let server = MailImapServer::new(schema_test_server_config());
        let operation_id = server
            .create_operation(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: MailboxAction::Create {
                    mailbox: "Archive".to_owned(),
                },
                completed: false,
                result: None,
            }))
            .await;
        {
            let mut operations = server.operations.lock().await;
            let operation = operations
                .get_mut(&operation_id)
                .expect("operation should exist");
            operation.state = OperationState::Running;
            operation.started_at = Some("2026-01-01T00:00:00Z".to_owned());
            operation.progress.phase = "processing_create".to_owned();
        }

        let response = server
            .operation_response(&operation_id)
            .await
            .expect("operation response should be available");
        assert_eq!(response["status"], "running");
        assert_eq!(response["operation"]["done"], false);
        assert_eq!(response["next_action"]["tool"], "imap_get_operation");
    }

    #[tokio::test]
    async fn cancel_operation_marks_running_operation() {
        let server = MailImapServer::new(schema_test_server_config());
        let operation_id = server
            .create_operation(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: MailboxAction::Create {
                    mailbox: "Archive".to_owned(),
                },
                completed: false,
                result: None,
            }))
            .await;

        let response = server
            .cancel_operation_impl(OperationIdInput {
                operation_id: operation_id.clone(),
            })
            .await
            .expect("cancel operation should succeed");
        assert_eq!(response["operation"]["state"], "cancel_requested");
        assert_eq!(response["status"], "running");
    }

    #[test]
    fn search_result_next_action_for_get_message_omits_account_id() {
        let next_action = next_action_for_search_result(
            "ok",
            "default",
            "INBOX",
            10,
            None,
            &[MessageSummary {
                message_id: "imap:default:INBOX:42:7".to_owned(),
                message_uri: "imap://default/messages/1".to_owned(),
                message_raw_uri: "imap://default/messages/1/raw".to_owned(),
                mailbox: "INBOX".to_owned(),
                uidvalidity: 42,
                uid: 7,
                date: Some("2026-01-01T00:00:00Z".to_owned()),
                from: Some("sender@example.com".to_owned()),
                subject: Some("subject".to_owned()),
                flags: Some(vec!["\\Seen".to_owned()]),
                snippet: Some("snippet".to_owned()),
            }],
        );

        assert_eq!(next_action.tool, "imap_get_message");
        assert_eq!(
            next_action.arguments["message_id"],
            "imap:default:INBOX:42:7"
        );
        assert!(next_action.arguments.get("account_id").is_none());
    }

    #[tokio::test]
    async fn get_operation_restarts_cancel_requested_operation_without_worker() {
        let server = MailImapServer::new(schema_test_server_config());
        let operation_id = server
            .create_operation(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: MailboxAction::Create {
                    mailbox: "Archive".to_owned(),
                },
                completed: false,
                result: None,
            }))
            .await;
        {
            let mut operations = server.operations.lock().await;
            let operation = operations
                .get_mut(&operation_id)
                .expect("operation should exist");
            operation.state = OperationState::CancelRequested;
            operation.progress.phase = "cancel_requested".to_owned();
            operation.worker_started = false;
        }

        let initial = server
            .get_operation_impl(OperationIdInput {
                operation_id: operation_id.clone(),
            })
            .await
            .expect("operation response should be available");
        assert_eq!(initial["operation"]["done"], false);

        for _ in 0..20 {
            let response = server
                .operation_response(&operation_id)
                .await
                .expect("operation response should be available");
            if response["operation"]["done"] == serde_json::Value::Bool(true) {
                assert_eq!(response["status"], "canceled");
                assert!(response.get("next_action").is_none());
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("operation did not reach a terminal state after restart");
    }

    fn schema_test_server_config() -> ServerConfig {
        ServerConfig {
            accounts: BTreeMap::new(),
            trusted_ca_certs: Vec::new(),
            write_enabled: false,
            connect_timeout_ms: 30_000,
            greeting_timeout_ms: 15_000,
            socket_timeout_ms: 300_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 512,
        }
    }
}
