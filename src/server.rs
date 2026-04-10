//! MCP server implementation with tool handlers.

mod read;
mod session_cache;
mod types;
mod validation;
mod write_ops;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::models::{
    AccountInfo, AccountOnlyInput, ApplyToMessagesInput, GetMessageInput, GetMessageRawInput,
    ManageMailboxInput, OperationIdInput, SearchMessagesInput, UpdateMessageFlagsInput,
};
use crate::pagination::CursorStore;

use self::session_cache::{IdleSessionCache, ReadSessionCache, ReadSessionLease};
use self::types::{
    GetMessageData, GetMessageRawData, ListAccountsData, ListMailboxesData, OperationStatusData,
    SearchResultData, StoredOperation, finalize_tool, operation_summary,
};

/// Maximum messages per search result page.
const MAX_SEARCH_LIMIT: usize = 50;
/// Maximum UID search results stored in a cursor snapshot.
const MAX_CURSOR_UIDS_STORED: usize = 20_000;
/// Maximum number of explicit message ids accepted by bulk write tools.
const MAX_BULK_MESSAGE_IDS: usize = 250;
/// Valid built-in IMAP system flags.
const VALID_SYSTEM_FLAGS: [&str; 5] = ["\\Seen", "\\Answered", "\\Flagged", "\\Deleted", "\\Draft"];
/// Maximum wall-clock budget for inline write execution before switching to background mode.
const WRITE_INLINE_BUDGET_MS: u64 = 1_500;

#[derive(Clone)]
pub struct MailImapServer {
    config: Arc<ServerConfig>,
    cursors: Arc<Mutex<CursorStore>>,
    read_sessions: Arc<ReadSessionCache>,
    operations: Arc<Mutex<BTreeMap<String, StoredOperation>>>,
    account_write_locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MailImapServer {
    pub fn new(config: ServerConfig) -> Self {
        let cursor_store = CursorStore::new(config.cursor_ttl_seconds, config.cursor_max_entries);
        let read_session_cache = IdleSessionCache::new(
            Duration::from_secs(config.read_session_cache_ttl_seconds),
            config.read_session_cache_max_per_account,
        );
        Self {
            config: Arc::new(config),
            cursors: Arc::new(Mutex::new(cursor_store)),
            read_sessions: Arc::new(Mutex::new(read_session_cache)),
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            account_write_locks: Arc::new(Mutex::new(BTreeMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "imap_list_accounts",
        description = "List configured IMAP accounts"
    )]
    async fn list_accounts(
        &self,
    ) -> Result<Json<crate::models::ToolEnvelope<ListAccountsData>>, ErrorData> {
        let started = Instant::now();
        let accounts = self
            .config
            .accounts
            .values()
            .map(|account| AccountInfo {
                account_id: account.account_id.clone(),
                host: account.host.clone(),
                port: account.port,
                secure: account.secure,
            })
            .collect::<Vec<_>>();
        let next_account_id = accounts
            .first()
            .map(|account| account.account_id.clone())
            .unwrap_or_else(|| "default".to_owned());
        let data = ListAccountsData {
            accounts,
            next_action: types::next_action_list_mailboxes(&next_account_id),
        };
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

    #[tool(
        name = "imap_list_mailboxes",
        description = "List mailboxes for an account"
    )]
    async fn list_mailboxes(
        &self,
        Parameters(input): Parameters<AccountOnlyInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<ListMailboxesData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_list_mailboxes",
            self.list_mailboxes_impl(input)
                .await
                .map(|data| (format!("{} mailbox(es)", data.mailboxes.len()), data)),
        )
    }

    #[tool(
        name = "imap_search_messages",
        description = "Search messages with cursor pagination"
    )]
    async fn search_messages(
        &self,
        Parameters(input): Parameters<SearchMessagesInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<SearchResultData>>, ErrorData> {
        let started = Instant::now();
        let result = self
            .search_messages_impl(input)
            .await
            .map(|data| (format!("{} message(s) returned", data.messages.len()), data));
        finalize_tool(started, "imap_search_messages", result)
    }

    #[tool(name = "imap_get_message", description = "Get parsed message details")]
    async fn get_message(
        &self,
        Parameters(input): Parameters<GetMessageInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<GetMessageData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_get_message",
            self.get_message_impl(input)
                .await
                .map(|data| ("Message retrieved".to_owned(), data)),
        )
    }

    #[tool(
        name = "imap_get_message_raw",
        description = "Get bounded RFC822 source"
    )]
    async fn get_message_raw(
        &self,
        Parameters(input): Parameters<GetMessageRawInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<GetMessageRawData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_get_message_raw",
            self.get_message_raw_impl(input)
                .await
                .map(|data| ("Raw message retrieved".to_owned(), data)),
        )
    }

    #[tool(
        name = "imap_apply_to_messages",
        description = "Apply one mutation action to explicit messages"
    )]
    async fn apply_to_messages(
        &self,
        Parameters(input): Parameters<ApplyToMessagesInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<OperationStatusData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_apply_to_messages",
            self.apply_to_messages_impl(input)
                .await
                .map(|data| (operation_summary(&data.status, &data.operation.kind), data)),
        )
    }

    #[tool(
        name = "imap_update_message_flags",
        description = "Add, remove, or replace flags on explicit messages"
    )]
    async fn update_message_flags(
        &self,
        Parameters(input): Parameters<UpdateMessageFlagsInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<OperationStatusData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_update_message_flags",
            self.update_message_flags_impl(input)
                .await
                .map(|data| (operation_summary(&data.status, &data.operation.kind), data)),
        )
    }

    #[tool(
        name = "imap_manage_mailbox",
        description = "Create, rename, or delete a mailbox"
    )]
    async fn manage_mailbox(
        &self,
        Parameters(input): Parameters<ManageMailboxInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<OperationStatusData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_manage_mailbox",
            self.manage_mailbox_impl(input)
                .await
                .map(|data| (operation_summary(&data.status, &data.operation.kind), data)),
        )
    }

    #[tool(
        name = "imap_get_operation",
        description = "Get the status of a background IMAP write operation"
    )]
    async fn get_operation(
        &self,
        Parameters(input): Parameters<OperationIdInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<OperationStatusData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_get_operation",
            self.get_operation_impl(input)
                .await
                .map(|data| (operation_summary(&data.status, &data.operation.kind), data)),
        )
    }

    #[tool(
        name = "imap_cancel_operation",
        description = "Cancel a background IMAP write operation"
    )]
    async fn cancel_operation(
        &self,
        Parameters(input): Parameters<OperationIdInput>,
    ) -> Result<Json<crate::models::ToolEnvelope<OperationStatusData>>, ErrorData> {
        let started = Instant::now();
        finalize_tool(
            started,
            "imap_cancel_operation",
            self.cancel_operation_impl(input)
                .await
                .map(|data| (operation_summary(&data.status, &data.operation.kind), data)),
        )
    }

    async fn checkout_read_session(
        &self,
        account_id: &str,
    ) -> crate::errors::AppResult<ReadSessionLease> {
        loop {
            let cached = {
                let mut cache = self.read_sessions.lock().await;
                cache.checkout(account_id, Instant::now())
            };
            if let Some(mut session) = cached {
                if crate::imap::noop_session(&self.config, &mut session)
                    .await
                    .is_ok()
                {
                    return Ok(ReadSessionLease::new(account_id.to_owned(), session));
                }
                let _ = crate::imap::logout_session_best_effort(&self.config, session).await;
                continue;
            }

            let account = self.config.get_account(account_id)?;
            let session = crate::imap::connect_authenticated(&self.config, account).await?;
            return Ok(ReadSessionLease::new(account_id.to_owned(), session));
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MailImapServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Secure IMAP MCP server. Read operations are enabled by default; write tools require MAIL_IMAP_WRITE_ENABLED=true.",
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rmcp::handler::server::tool::schema_for_output;

    use super::*;
    use crate::models::{OperationIdInput, ToolEnvelope, validate_client_safe_input_schema};
    use crate::server::types::{
        GetMessageData, GetMessageRawData, ListAccountsData, ListMailboxesData,
        ManageMailboxOperation, OperationState, OperationStatusData, SearchResultData,
        StoredOperationSpec,
    };

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
    fn all_tools_publish_output_schemas() {
        let server = MailImapServer::new(schema_test_server_config());
        for tool in server.tool_router.list_all() {
            assert!(
                tool.output_schema.is_some(),
                "tool {} missing output_schema",
                tool.name
            );
        }
    }

    #[test]
    fn tool_output_schemas_match_concrete_envelope_types() {
        let server = MailImapServer::new(schema_test_server_config());
        let expected = [
            (
                "imap_list_accounts",
                schema_for_output::<ToolEnvelope<ListAccountsData>>().expect("valid schema"),
            ),
            (
                "imap_list_mailboxes",
                schema_for_output::<ToolEnvelope<ListMailboxesData>>().expect("valid schema"),
            ),
            (
                "imap_search_messages",
                schema_for_output::<ToolEnvelope<SearchResultData>>().expect("valid schema"),
            ),
            (
                "imap_get_message",
                schema_for_output::<ToolEnvelope<GetMessageData>>().expect("valid schema"),
            ),
            (
                "imap_get_message_raw",
                schema_for_output::<ToolEnvelope<GetMessageRawData>>().expect("valid schema"),
            ),
            (
                "imap_apply_to_messages",
                schema_for_output::<ToolEnvelope<OperationStatusData>>().expect("valid schema"),
            ),
            (
                "imap_update_message_flags",
                schema_for_output::<ToolEnvelope<OperationStatusData>>().expect("valid schema"),
            ),
            (
                "imap_manage_mailbox",
                schema_for_output::<ToolEnvelope<OperationStatusData>>().expect("valid schema"),
            ),
            (
                "imap_get_operation",
                schema_for_output::<ToolEnvelope<OperationStatusData>>().expect("valid schema"),
            ),
            (
                "imap_cancel_operation",
                schema_for_output::<ToolEnvelope<OperationStatusData>>().expect("valid schema"),
            ),
        ];

        for (name, schema) in expected {
            let tool = server
                .tool_router
                .list_all()
                .into_iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert_eq!(
                tool.output_schema
                    .as_ref()
                    .expect("output schema missing")
                    .as_ref(),
                schema.as_ref(),
                "tool {name} output schema differs from concrete envelope type"
            );
        }
    }

    pub(super) fn schema_test_server_config() -> ServerConfig {
        ServerConfig {
            accounts: BTreeMap::new(),
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

    #[tokio::test]
    async fn operation_response_includes_next_action_while_running() {
        let server = MailImapServer::new(schema_test_server_config());
        let operation_id = server
            .create_operation(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: types::MailboxAction::Create {
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
        assert_eq!(response.status, "running");
        assert!(!response.operation.done);
        assert_eq!(
            response
                .next_action
                .as_ref()
                .expect("next action should be present")
                .tool,
            "imap_get_operation"
        );
    }

    #[tokio::test]
    async fn cancel_operation_marks_running_operation() {
        let server = MailImapServer::new(schema_test_server_config());
        let operation_id = server
            .create_operation(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: types::MailboxAction::Create {
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
        assert_eq!(response.operation.state, "cancel_requested");
        assert_eq!(response.status, "running");
    }

    #[tokio::test]
    async fn get_operation_restarts_cancel_requested_operation_without_worker() {
        let server = MailImapServer::new(schema_test_server_config());
        let operation_id = server
            .create_operation(StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: types::MailboxAction::Create {
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
        assert!(!initial.operation.done);

        for _ in 0..20 {
            let response = server
                .operation_response(&operation_id)
                .await
                .expect("operation response should be available");
            if response.operation.done {
                assert_eq!(response.status, "canceled");
                assert!(response.next_action.is_none());
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        panic!("operation did not reach a terminal state after restart");
    }
}
