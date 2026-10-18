use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_imap::Session;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tracing::error;

use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::mailbox_codec::normalize_mailbox_name;
use crate::message_id::MessageId;
use crate::models::{
    ApplyToMessagesInput, GetOperationInput, ManageMailboxInput, OperationIdInput, SetOperation,
    UpdateMessageFlagsInput, UpdateMessageLabelsInput,
};

use super::provider;
use super::types::{
    ApplyMessagesOperation, BulkMessageOperationData, MailboxAction, MailboxManagementResult,
    MessageActionInput, MessageMutationGroup, MessageMutationResult, OperationMetadata,
    OperationProgress, OperationResultData, OperationState, OperationStatusData, OperationStep,
    OperationStepOutcome, PropertyUpdate, StoredOperation, StoredOperationSpec, ToolIssue,
    UpdatePropertiesOperation, canceled_tool_issue, destination_mailbox_for_action,
    mailbox_action_display, mailbox_action_stage, message_action_name, next_action_get_operation,
    next_action_get_operation_with_result, next_operation_step, now_utc_string,
    operation_kind_label, operation_total_units, set_operation_name, status_from_issue_and_counts,
};
use super::validation::{
    build_flag_update, build_label_update, build_mailbox_action, build_message_action,
    parse_bulk_message_ids, require_write_enabled, validate_account_id, validate_mailbox,
    validate_message_action, validate_operation_id,
};
use super::{MailImapServer, WRITE_INLINE_BUDGET_MS};

/// Capabilities of an execution session, read with one CAPABILITY command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutionCapabilities {
    /// Whether the server advertises native `MOVE`.
    supports_move: bool,
    /// Whether the server advertises `UIDPLUS`, which `UID EXPUNGE` of exact UIDs requires.
    supports_uidplus: bool,
    /// Whether the server advertises Gmail's `X-GM-EXT-1`.
    supports_gmail: bool,
}

/// Message actions need capabilities that only some servers advertise.
///
/// Delete requires UIDPLUS. Move requires native MOVE or UIDPLUS for the copy-based path.
/// Copy needs neither. There is no fallback to plain EXPUNGE.
fn check_message_action_capabilities(
    action: &MessageActionInput,
    capabilities: ExecutionCapabilities,
) -> AppResult<()> {
    let allowed = match action {
        MessageActionInput::Copy { .. } => true,
        MessageActionInput::Move { .. } => {
            capabilities.supports_move || capabilities.supports_uidplus
        }
        MessageActionInput::Delete => capabilities.supports_uidplus,
    };
    if allowed {
        Ok(())
    } else {
        Err(AppError::Conflict(
            "server lacks UIDPLUS; delete and copy-based move are unavailable without safe UID EXPUNGE"
                .to_owned(),
        ))
    }
}

/// Rejects Gmail label updates on servers that do not advertise `X-GM-EXT-1`.
fn check_property_update_capabilities(
    update: &PropertyUpdate,
    capabilities: ExecutionCapabilities,
) -> AppResult<()> {
    if matches!(update, PropertyUpdate::GmailLabels { .. }) && !capabilities.supports_gmail {
        return Err(AppError::InvalidInput(
            "Gmail labels require a server that advertises X-GM-EXT-1".to_owned(),
        ));
    }
    Ok(())
}

/// Connection state shared by the groups of one operation.
#[derive(Default)]
struct OperationExecutionContext {
    account_id: Option<String>,
    session: Option<imap::ImapSession>,
    selected_mailbox: Option<String>,
    selected_readonly: Option<bool>,
    selected_uidvalidity: Option<u32>,
    /// Capabilities of the current session; unset until read and after any session change.
    capabilities: Option<ExecutionCapabilities>,
}

impl OperationExecutionContext {
    /// Forgets the selection and capabilities that belong to a replaced or removed session.
    fn clear_session_cache(&mut self) {
        self.selected_mailbox = None;
        self.selected_readonly = None;
        self.selected_uidvalidity = None;
        self.capabilities = None;
    }

    /// Drops the session after a command error so that no later command uses the connection.
    fn discard_session(&mut self) {
        self.session = None;
        self.account_id = None;
        self.clear_session_cache();
    }

    /// Returns the execution session or an internal error when none is open.
    fn session_mut(&mut self) -> AppResult<&mut imap::ImapSession> {
        self.session
            .as_mut()
            .ok_or_else(|| AppError::Internal("execution session unavailable".to_owned()))
    }
}

impl MailImapServer {
    pub(super) async fn apply_to_messages_impl(
        &self,
        input: ApplyToMessagesInput,
    ) -> AppResult<OperationStatusData> {
        require_write_enabled(&self.config)?;
        let action = build_message_action(&input)?;
        validate_message_action(&action)?;
        let (account_id, message_ids) = parse_bulk_message_ids(&input.message_ids)?;
        let spec = self
            .preflight_apply_message_operation(&account_id, action, message_ids)
            .await?;
        self.start_write_operation(spec).await
    }

    /// Validates a flag update, checks it against the server, and starts its operation.
    pub(super) async fn update_message_flags_impl(
        &self,
        input: UpdateMessageFlagsInput,
    ) -> AppResult<OperationStatusData> {
        require_write_enabled(&self.config)?;
        let update = build_flag_update(&input)?;
        let (account_id, message_ids) = parse_bulk_message_ids(&input.message_ids)?;
        let spec = self
            .preflight_property_update(&account_id, update, message_ids)
            .await?;
        self.start_write_operation(spec).await
    }

    /// Validates a Gmail label update, checks Gmail support, and starts its operation.
    pub(super) async fn update_message_labels_impl(
        &self,
        input: UpdateMessageLabelsInput,
    ) -> AppResult<OperationStatusData> {
        require_write_enabled(&self.config)?;
        let update = build_label_update(&input)?;
        let (account_id, message_ids) = parse_bulk_message_ids(&input.message_ids)?;
        let spec = self
            .preflight_property_update(&account_id, update, message_ids)
            .await?;
        self.start_write_operation(spec).await
    }

    pub(super) async fn manage_mailbox_impl(
        &self,
        input: ManageMailboxInput,
    ) -> AppResult<OperationStatusData> {
        require_write_enabled(&self.config)?;
        validate_account_id(&input.account_id)?;
        let action = build_mailbox_action(&input)?;
        let spec = self
            .preflight_manage_mailbox_operation(&input.account_id, action)
            .await?;
        self.start_write_operation(spec).await
    }

    pub(super) async fn get_operation_impl(
        &self,
        input: GetOperationInput,
    ) -> AppResult<OperationStatusData> {
        validate_operation_id(&input.operation_id)?;
        self.ensure_operation_worker_running(&input.operation_id)
            .await?;
        self.operation_response(&input.operation_id, input.include_result)
            .await
    }

    pub(super) async fn cancel_operation_impl(
        &self,
        input: OperationIdInput,
    ) -> AppResult<OperationStatusData> {
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
        self.operation_response(&input.operation_id, true).await
    }

    /// Checks move destinations, server capabilities, and UIDVALIDITY before a message action.
    async fn preflight_apply_message_operation(
        &self,
        account_id: &str,
        action: MessageActionInput,
        message_ids: Vec<MessageId>,
    ) -> AppResult<StoredOperationSpec> {
        let groups = group_message_ids(&message_ids);
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
        }
        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        if !matches!(action, MessageActionInput::Copy { .. }) {
            let capabilities = self.read_capabilities(&mut session).await?;
            check_message_action_capabilities(&action, capabilities)?;
        }
        if let Some(destination_mailbox) = destination_mailbox_for_action(&action) {
            imap::select_mailbox_readonly(&self.config, &mut session, destination_mailbox).await?;
        }
        self.validate_group_uidvalidities(&mut session, &groups, false)
            .await?;
        Ok(StoredOperationSpec::ApplyMessages(ApplyMessagesOperation {
            account_id: account_id.to_owned(),
            action,
            message_ids,
            groups,
            next_group_index: 0,
            result_by_id: BTreeMap::new(),
        }))
    }

    /// Checks capabilities and UIDVALIDITY for a flag or Gmail label update.
    async fn preflight_property_update(
        &self,
        account_id: &str,
        update: PropertyUpdate,
        message_ids: Vec<MessageId>,
    ) -> AppResult<StoredOperationSpec> {
        let groups = group_message_ids(&message_ids);
        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        if matches!(update, PropertyUpdate::GmailLabels { .. }) {
            let capabilities = self.read_capabilities(&mut session).await?;
            check_property_update_capabilities(&update, capabilities)?;
        }
        self.validate_group_uidvalidities(&mut session, &groups, false)
            .await?;
        Ok(StoredOperationSpec::UpdateProperties(
            UpdatePropertiesOperation {
                account_id: account_id.to_owned(),
                update,
                message_ids,
                groups,
                next_group_index: 0,
                result_by_id: BTreeMap::new(),
            },
        ))
    }

    async fn preflight_manage_mailbox_operation(
        &self,
        account_id: &str,
        action: MailboxAction,
    ) -> AppResult<StoredOperationSpec> {
        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        match &action {
            MailboxAction::Create { mailbox } => validate_mailbox(mailbox)?,
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
        Ok(StoredOperationSpec::ManageMailbox(
            super::types::ManageMailboxOperation {
                account_id: account_id.to_owned(),
                action,
                completed: false,
                result: None,
            },
        ))
    }

    async fn start_write_operation(
        &self,
        spec: StoredOperationSpec,
    ) -> AppResult<OperationStatusData> {
        let operation_id = self.create_operation(spec).await;
        let deadline = Instant::now() + Duration::from_millis(WRITE_INLINE_BUDGET_MS);
        self.run_operation_until(&operation_id, Some(deadline))
            .await?;
        self.ensure_operation_worker_running(&operation_id).await?;
        self.operation_response(&operation_id, true).await
    }

    pub(super) async fn create_operation(&self, spec: StoredOperationSpec) -> String {
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
        evict_completed_operations(&mut operations, self.config.operation_max_entries);
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
        let mut execution_ctx = OperationExecutionContext::default();
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
            let outcome = self.execute_operation_step(&mut execution_ctx, &step).await;
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

    async fn execute_operation_step(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        step: &OperationStep,
    ) -> OperationStepOutcome {
        match step {
            OperationStep::ApplyMessagesGroup {
                account_id,
                action,
                group,
            } => OperationStepOutcome::MessageResults(
                self.execute_message_action_group(execution_ctx, account_id, group, action)
                    .await,
            ),
            OperationStep::UpdatePropertiesGroup {
                account_id,
                update,
                group,
            } => OperationStepOutcome::MessageResults(
                self.execute_property_update_group(execution_ctx, account_id, group, update)
                    .await,
            ),
            OperationStep::ManageMailbox { account_id, action } => {
                OperationStepOutcome::MailboxResult(
                    self.execute_manage_mailbox_action(execution_ctx, account_id, action)
                        .await,
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
                StoredOperationSpec::UpdateProperties(spec),
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
            StoredOperationSpec::UpdateProperties(spec) => {
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
                    Some(set_operation_name(spec.update.operation())),
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
                OperationResultData::MailboxManagement(spec.result.clone().ok_or_else(|| {
                    AppError::Internal("missing mailbox operation result".to_owned())
                })?)
            }
        };

        operation.state = if was_cancel_requested {
            OperationState::Canceled
        } else {
            match operation_result_status(&result) {
                "ok" => OperationState::Ok,
                "partial" => OperationState::Partial,
                "failed" => OperationState::Failed,
                _ => OperationState::Failed,
            }
        };
        operation.progress.phase = operation.state.state_label().to_owned();
        operation.progress.current_mailbox = None;
        operation.progress.remaining_units = 0;
        operation.finished_at = Some(now_utc_string());
        operation.worker_started = false;
        operation.issues = operation_result_issues(&result).to_vec();
        operation.result = Some(result);
        evict_completed_operations(&mut operations, self.config.operation_max_entries);
        Ok(())
    }

    pub(super) async fn operation_response(
        &self,
        operation_id: &str,
        include_result: bool,
    ) -> AppResult<OperationStatusData> {
        let operation = {
            let operations = self.operations.lock().await;
            operations.get(operation_id).cloned().ok_or_else(|| {
                AppError::NotFound(format!("operation '{operation_id}' not found"))
            })?
        };
        let is_terminal = operation.state.is_terminal();
        let result = if is_terminal && include_result {
            operation.result.clone()
        } else {
            None
        };
        let next_action = if !is_terminal {
            Some(next_action_get_operation(operation_id))
        } else if operation.result.is_some() && !include_result {
            Some(next_action_get_operation_with_result(operation_id))
        } else {
            None
        };

        Ok(OperationStatusData {
            status: operation.state.status_label().to_owned(),
            issues: operation.issues,
            operation: OperationMetadata {
                operation_id: operation.operation_id,
                kind: operation.kind,
                state: operation.state.state_label().to_owned(),
                done: is_terminal,
                cancel_supported: operation.cancel_supported,
                created_at: operation.created_at,
                started_at: operation.started_at,
                finished_at: operation.finished_at,
                progress: operation.progress,
            },
            result,
            next_action,
        })
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

    /// Marks an operation as ended by a worker error and fills results for unfinished groups.
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
            StoredOperationSpec::UpdateProperties(spec) => {
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
                    Some(set_operation_name(spec.update.operation())),
                    ordered,
                )?
            }
            StoredOperationSpec::ManageMailbox(spec) => {
                if !spec.completed {
                    let (action_name, mailbox, destination_mailbox) =
                        mailbox_action_display(&spec.action);
                    spec.result = Some(MailboxManagementResult {
                        status: "failed".to_owned(),
                        issues: vec![failure_issue.clone()],
                        account_id: spec.account_id.clone(),
                        action: action_name.to_owned(),
                        mailbox,
                        destination_mailbox,
                    });
                    spec.completed = true;
                }
                OperationResultData::MailboxManagement(spec.result.clone().ok_or_else(|| {
                    AppError::Internal("missing mailbox operation result".to_owned())
                })?)
            }
        };

        // A partial message keeps the operation partial even when the worker failed later.
        operation.state = if operation_result_status(&result) == "partial" {
            OperationState::Partial
        } else {
            OperationState::Failed
        };
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
        let issues = operation_result_issues(&result);
        operation.issues = if issues.is_empty() {
            vec![failure_issue]
        } else {
            issues.to_vec()
        };
        operation.result = Some(result);
        evict_completed_operations(&mut operations, self.config.operation_max_entries);
        Ok(())
    }

    async fn execute_manage_mailbox_action(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        action: &MailboxAction,
    ) -> MailboxManagementResult {
        let (action_name, mailbox, destination_mailbox) = mailbox_action_display(action);
        if let Err(error) = self
            .ensure_execution_session(execution_ctx, account_id)
            .await
        {
            return MailboxManagementResult {
                status: "failed".to_owned(),
                issues: vec![ToolIssue::from_error("connect_authenticated", &error)],
                account_id: account_id.to_owned(),
                action: action_name.to_owned(),
                mailbox,
                destination_mailbox,
            };
        }
        let Some(session) = execution_ctx.session.as_mut() else {
            return MailboxManagementResult {
                status: "failed".to_owned(),
                issues: vec![ToolIssue::from_error(
                    "connect_authenticated",
                    &AppError::Internal("execution session unavailable".to_owned()),
                )],
                account_id: account_id.to_owned(),
                action: action_name.to_owned(),
                mailbox,
                destination_mailbox,
            };
        };

        let mut issues = Vec::new();
        let operation = match action {
            MailboxAction::Create { mailbox } => {
                imap::create_mailbox_path(&self.config, session, mailbox).await
            }
            MailboxAction::Rename {
                mailbox,
                destination_mailbox,
            } => imap::rename_mailbox(&self.config, session, mailbox, destination_mailbox).await,
            MailboxAction::Delete { mailbox } => {
                imap::delete_mailbox(&self.config, session, mailbox).await
            }
        };
        if let Err(error) = operation {
            issues.push(ToolIssue::from_error(mailbox_action_stage(action), &error));
        } else {
            execution_ctx.selected_mailbox = None;
            execution_ctx.selected_readonly = None;
            execution_ctx.selected_uidvalidity = None;
        }

        let status = status_from_issue_and_counts(&issues, issues.is_empty()).to_owned();
        MailboxManagementResult {
            status,
            issues,
            account_id: account_id.to_owned(),
            action: action_name.to_owned(),
            mailbox,
            destination_mailbox,
        }
    }

    /// Runs one copy, move, or delete group on the execution session.
    ///
    /// Capabilities are read on the execution session before the first mutation, and
    /// any command error discards the session so that no later command uses it.
    async fn execute_message_action_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
        action: &MessageActionInput,
    ) -> Vec<MessageMutationResult> {
        let destination_mailbox = destination_mailbox_for_action(action);
        if let Err(error) = self
            .ensure_selected_group(execution_ctx, account_id, group, false)
            .await
        {
            return failed_group_results(
                group,
                "select_mailbox_readwrite",
                &error,
                destination_mailbox,
            );
        }
        // Whether the server can move natively; copy needs no capability check.
        let native_move = match action {
            MessageActionInput::Copy { .. } => false,
            MessageActionInput::Move { .. } | MessageActionInput::Delete => {
                let capabilities = match self.ensure_capabilities(execution_ctx, account_id).await {
                    Ok(capabilities) => capabilities,
                    Err(error) => {
                        return failed_group_results(
                            group,
                            "capabilities",
                            &error,
                            destination_mailbox,
                        );
                    }
                };
                if let Err(error) = check_message_action_capabilities(action, capabilities) {
                    return failed_group_results(
                        group,
                        "capabilities",
                        &error,
                        destination_mailbox,
                    );
                }
                capabilities.supports_move
            }
        };
        let session = match execution_ctx.session_mut() {
            Ok(session) => session,
            Err(error) => {
                return failed_group_results(
                    group,
                    "connect_authenticated",
                    &error,
                    destination_mailbox,
                );
            }
        };
        let uids = group_uids(group);
        let outcome = run_message_action(&self.config, session, action, &uids, native_move).await;
        if outcome.failure.is_some() {
            execution_ctx.discard_session();
        }
        message_group_results(account_id, group, destination_mailbox, &outcome)
    }

    /// Runs one flag or Gmail label update group on the execution session.
    ///
    /// Label updates re-check Gmail support on the execution session before any STORE.
    /// Any command error discards the session; no mutation command is ever repeated.
    async fn execute_property_update_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
        update: &PropertyUpdate,
    ) -> Vec<MessageMutationResult> {
        if let Err(error) = self
            .ensure_selected_group(execution_ctx, account_id, group, false)
            .await
        {
            return failed_group_results(group, "select_mailbox_readwrite", &error, None);
        }
        if matches!(update, PropertyUpdate::GmailLabels { .. }) {
            let capabilities = match self.ensure_capabilities(execution_ctx, account_id).await {
                Ok(capabilities) => capabilities,
                Err(error) => return failed_group_results(group, "capabilities", &error, None),
            };
            if let Err(error) = check_property_update_capabilities(update, capabilities) {
                return failed_group_results(group, "capabilities", &error, None);
            }
        }
        let session = match execution_ctx.session_mut() {
            Ok(session) => session,
            Err(error) => {
                return failed_group_results(group, "connect_authenticated", &error, None);
            }
        };
        let uids = group_uids(group);
        let outcome = run_property_update(&self.config, session, update, &uids).await;
        if outcome.failure.is_some() || outcome.observed.is_err() {
            execution_ctx.discard_session();
        }
        property_results(group, update, &outcome)
    }

    async fn validate_group_uidvalidities(
        &self,
        session: &mut imap::ImapSession,
        groups: &[MessageMutationGroup],
        readonly: bool,
    ) -> AppResult<()> {
        let mut selected_mailbox: Option<String> = None;
        let mut selected_uidvalidity: Option<u32> = None;
        for group in groups {
            let current_uidvalidity = if selected_mailbox.as_deref() == Some(group.mailbox.as_str())
            {
                cached_uidvalidity(&selected_uidvalidity)?
            } else if readonly {
                let uidvalidity =
                    imap::select_mailbox_readonly(&self.config, session, &group.mailbox).await?;
                selected_mailbox = Some(group.mailbox.clone());
                selected_uidvalidity = Some(uidvalidity);
                uidvalidity
            } else {
                let uidvalidity =
                    imap::select_mailbox_readwrite(&self.config, session, &group.mailbox).await?;
                selected_mailbox = Some(group.mailbox.clone());
                selected_uidvalidity = Some(uidvalidity);
                uidvalidity
            };
            if current_uidvalidity != group.uidvalidity {
                return Err(AppError::Conflict(
                    "message uidvalidity no longer matches mailbox".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Opens the execution session for the account, clearing the cache of any replaced session.
    async fn ensure_execution_session(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
    ) -> AppResult<()> {
        if execution_ctx.account_id.as_deref() == Some(account_id)
            && execution_ctx.session.is_some()
        {
            return Ok(());
        }

        let account = self.config.get_account(account_id)?;
        let session = imap::connect_authenticated(&self.config, account).await?;
        execution_ctx.account_id = Some(account_id.to_owned());
        execution_ctx.session = Some(session);
        execution_ctx.clear_session_cache();
        Ok(())
    }

    /// Selects the group's mailbox on the execution session and checks its UIDVALIDITY.
    ///
    /// A SELECT error discards the session; the cached selection always matches the server.
    async fn ensure_selected_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
        readonly: bool,
    ) -> AppResult<()> {
        self.ensure_execution_session(execution_ctx, account_id)
            .await?;
        if execution_ctx.selected_mailbox.as_deref() == Some(group.mailbox.as_str())
            && execution_ctx.selected_readonly == Some(readonly)
            && execution_ctx.selected_uidvalidity == Some(group.uidvalidity)
        {
            return Ok(());
        }

        // The previous selection is unknown while a SELECT is in flight.
        execution_ctx.selected_mailbox = None;
        execution_ctx.selected_readonly = None;
        execution_ctx.selected_uidvalidity = None;
        let session = execution_ctx.session_mut()?;
        let selected = if readonly {
            imap::select_mailbox_readonly(&self.config, session, &group.mailbox).await
        } else {
            imap::select_mailbox_readwrite(&self.config, session, &group.mailbox).await
        };
        let current_uidvalidity = match selected {
            Ok(uidvalidity) => uidvalidity,
            Err(error) => {
                execution_ctx.discard_session();
                return Err(error);
            }
        };
        execution_ctx.selected_mailbox = Some(group.mailbox.clone());
        execution_ctx.selected_readonly = Some(readonly);
        execution_ctx.selected_uidvalidity = Some(current_uidvalidity);
        if current_uidvalidity != group.uidvalidity {
            return Err(AppError::Conflict(
                "message uidvalidity no longer matches mailbox".to_owned(),
            ));
        }
        Ok(())
    }

    /// Returns the capabilities of the execution session, reading them once per session.
    ///
    /// A CAPABILITY command error discards the session.
    async fn ensure_capabilities(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
    ) -> AppResult<ExecutionCapabilities> {
        self.ensure_execution_session(execution_ctx, account_id)
            .await?;
        if let Some(capabilities) = execution_ctx.capabilities {
            return Ok(capabilities);
        }
        let read = self.read_capabilities(execution_ctx.session_mut()?).await;
        match read {
            Ok(capabilities) => {
                execution_ctx.capabilities = Some(capabilities);
                Ok(capabilities)
            }
            Err(error) => {
                execution_ctx.discard_session();
                Err(error)
            }
        }
    }

    /// Reads the write-relevant capabilities of a session with one CAPABILITY command.
    async fn read_capabilities(
        &self,
        session: &mut imap::ImapSession,
    ) -> AppResult<ExecutionCapabilities> {
        let capabilities = imap::capabilities(&self.config, session).await?;
        Ok(ExecutionCapabilities {
            supports_move: capabilities.has_str("MOVE"),
            supports_uidplus: capabilities.has_str("UIDPLUS"),
            supports_gmail: provider::is_gmail_capable(&capabilities),
        })
    }
}

fn cached_uidvalidity(selected_uidvalidity: &Option<u32>) -> AppResult<u32> {
    selected_uidvalidity.ok_or_else(|| {
        AppError::Internal(
            "selected mailbox uidvalidity cache missing during validation".to_owned(),
        )
    })
}

fn evict_completed_operations(
    operations: &mut BTreeMap<String, StoredOperation>,
    max_completed_entries: usize,
) {
    let completed_ids = operations
        .iter()
        .filter(|(_, operation)| operation.state.is_terminal())
        .map(|(operation_id, operation)| {
            (
                operation_id.clone(),
                operation
                    .finished_at
                    .clone()
                    .unwrap_or_else(|| operation.created_at.clone()),
            )
        })
        .collect::<Vec<_>>();

    if completed_ids.len() <= max_completed_entries {
        return;
    }

    let overflow = completed_ids.len() - max_completed_entries;
    let mut completed_ids = completed_ids;
    completed_ids.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    for (operation_id, _) in completed_ids.into_iter().take(overflow) {
        operations.remove(&operation_id);
    }
}

fn group_message_ids(message_ids: &[MessageId]) -> Vec<MessageMutationGroup> {
    let mut grouped = BTreeMap::<(String, u32), Vec<MessageId>>::new();
    for message_id in message_ids {
        grouped
            .entry((message_id.mailbox.clone(), message_id.uidvalidity))
            .or_default()
            .push(message_id.clone());
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

/// Returns the UIDs of a group in input order.
fn group_uids(group: &MessageMutationGroup) -> Vec<u32> {
    group.entries.iter().map(|entry| entry.uid).collect()
}

/// Attaches the UID and encoded message id of a message to an issue.
fn message_issue(issue: ToolIssue, message_id: &MessageId) -> ToolIssue {
    issue
        .with_uid(message_id.uid)
        .with_message_id(&message_id.encode())
}

/// Builds one message result with an explicit status and no property values.
fn message_result(
    message_id: &MessageId,
    status: &str,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<&str>,
) -> MessageMutationResult {
    MessageMutationResult {
        message_id: message_id.encode(),
        status: status.to_owned(),
        issues,
        source_mailbox: message_id.mailbox.clone(),
        destination_mailbox: destination_mailbox.map(ToOwned::to_owned),
        flags: None,
        labels: None,
        new_message_id: None,
    }
}

/// Builds a result for a message whose write was not applied.
fn failed_message_result(
    message_id: &MessageId,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<&str>,
) -> MessageMutationResult {
    message_result(message_id, "failed", issues, destination_mailbox)
}

/// Summarizes per-message results into the bulk operation payload.
///
/// `succeeded`, `partial`, and `failed` count the explicit per-message statuses.
/// The aggregate is `ok` without issues, `partial` when any message was applied
/// or partially applied, and `failed` otherwise.
fn build_bulk_message_response(
    account_id: String,
    action: Option<&str>,
    operation: Option<&str>,
    results: Vec<MessageMutationResult>,
) -> AppResult<OperationResultData> {
    let matched = results.len();
    let mut issues = Vec::new();
    // Number of messages with status `ok`.
    let mut succeeded = 0usize;
    // Number of messages with status `partial`.
    let mut partial = 0usize;
    for result in &results {
        match result.status.as_str() {
            "ok" => succeeded += 1,
            "partial" => partial += 1,
            _ => {}
        }
        issues.extend(result.issues.iter().cloned());
    }
    let failed = matched.saturating_sub(succeeded + partial);
    let status = status_from_issue_and_counts(&issues, succeeded > 0 || partial > 0).to_owned();
    Ok(OperationResultData::BulkMessage(BulkMessageOperationData {
        status,
        issues,
        account_id,
        action: action.map(ToOwned::to_owned),
        operation: operation.map(ToOwned::to_owned),
        matched,
        attempted: matched,
        succeeded,
        partial,
        failed,
        results,
    }))
}

fn operation_result_status(result: &OperationResultData) -> &str {
    match result {
        OperationResultData::BulkMessage(result) => result.status.as_str(),
        OperationResultData::MailboxManagement(result) => result.status.as_str(),
    }
}

fn operation_result_issues(result: &OperationResultData) -> &[ToolIssue] {
    match result {
        OperationResultData::BulkMessage(result) => &result.issues,
        OperationResultData::MailboxManagement(result) => &result.issues,
    }
}

fn append_canceled_message_results(
    result_by_id: &mut BTreeMap<String, MessageMutationResult>,
    remaining_groups: &[MessageMutationGroup],
    destination_mailbox: Option<&str>,
) {
    for group in remaining_groups {
        for message_id in &group.entries {
            let encoded = message_id.encode();
            result_by_id.insert(
                encoded.clone(),
                failed_message_result(
                    message_id,
                    vec![canceled_tool_issue(Some(message_id.uid), Some(encoded))],
                    destination_mailbox,
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
        for message_id in &group.entries {
            result_by_id.insert(
                message_id.encode(),
                failed_message_result(
                    message_id,
                    vec![message_issue(issue.clone(), message_id)],
                    destination_mailbox,
                ),
            );
        }
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

/// Builds `failed` results for every message of a group from one error.
fn failed_group_results(
    group: &MessageMutationGroup,
    stage: &str,
    error: &AppError,
    destination_mailbox: Option<&str>,
) -> Vec<MessageMutationResult> {
    group
        .entries
        .iter()
        .map(|message_id| {
            failed_message_result(
                message_id,
                vec![message_issue(
                    ToolIssue::from_error(stage, error),
                    message_id,
                )],
                destination_mailbox,
            )
        })
        .collect()
}

/// Effect text when only the copy phase of a move was applied.
const EFFECT_COPIED: &str = "the messages were copied to the destination mailbox";
/// Effect text when the copy and the `\Deleted` flag of a move were applied.
const EFFECT_COPIED_AND_FLAGGED: &str = "the messages were copied to the destination mailbox and flagged \\Deleted in the source mailbox";
/// Effect text when a delete flagged the messages but did not expunge them.
const EFFECT_FLAGGED: &str = "the messages were flagged \\Deleted in the source mailbox";

/// How a failed command phase relates to the phases that ran before it.
#[derive(Debug)]
enum FailureKind {
    /// The server refused the first write command with a tagged NO or BAD.
    Rejected,
    /// The write command was sent and its outcome is unknown.
    Unknown,
    /// An earlier phase was applied; the text describes its effect.
    AfterApplied(&'static str),
}

/// A failed command phase of one write group.
#[derive(Debug)]
struct PhaseFailure {
    /// Stage name reported in the issues.
    stage: &'static str,
    /// Error of the failed command.
    error: AppError,
    /// Relation of the failure to the earlier phases.
    kind: FailureKind,
}

impl PhaseFailure {
    /// Classifies the failure of the first write command of a group.
    fn first(stage: &'static str, failure: imap::CommandFailure) -> Self {
        let kind = match failure {
            imap::CommandFailure::Rejected(_) => FailureKind::Rejected,
            imap::CommandFailure::Uncertain(_) => FailureKind::Unknown,
        };
        Self {
            stage,
            error: failure.into(),
            kind,
        }
    }

    /// Classifies a failure of a later phase; the earlier phase stays applied.
    fn after(stage: &'static str, error: impl Into<AppError>, effect: &'static str) -> Self {
        Self {
            stage,
            error: error.into(),
            kind: FailureKind::AfterApplied(effect),
        }
    }

    /// Returns the message status: only an unmapped, refused first command is `failed`.
    fn status(&self, has_new_id: bool) -> &'static str {
        match self.kind {
            FailureKind::Rejected if !has_new_id => "failed",
            _ => "partial",
        }
    }

    /// Builds the issue; every outcome that may have changed the mailbox is not retryable.
    fn issue(&self, has_new_id: bool) -> ToolIssue {
        let issue = ToolIssue::from_error(self.stage, &self.error);
        match self.kind {
            FailureKind::Rejected if has_new_id => issue.non_retryable(),
            FailureKind::Rejected => issue,
            FailureKind::Unknown => issue.non_retryable().with_detail(
                "the write was sent and its outcome is unknown; check the message state before another write",
            ),
            FailureKind::AfterApplied(effect) => issue.non_retryable().with_detail(&format!(
                "{effect}, but {} did not complete; check the message state before another write",
                self.stage
            )),
        }
    }
}

/// Result of the commands of one copy, move, or delete group.
#[derive(Debug)]
struct GroupCommandOutcome {
    /// Destination identities reported by the server.
    mapping: Option<imap::CopyUidMapping>,
    /// Problem with the reported destination identities.
    mapping_issue: Option<AppError>,
    /// First failed phase, if any.
    failure: Option<PhaseFailure>,
}

impl GroupCommandOutcome {
    /// Converts a COPY or MOVE outcome, naming `stage` as the failed phase.
    fn from_copy_move(stage: &'static str, outcome: imap::CopyMoveOutcome) -> Self {
        Self {
            mapping: outcome.mapping,
            mapping_issue: outcome.mapping_issue,
            failure: outcome
                .completion
                .err()
                .map(|failure| PhaseFailure::first(stage, failure)),
        }
    }
}

/// Flags the source messages `\Deleted` and expunges exactly those UIDs.
///
/// `copied` is true when a COPY already succeeded, so any failure leaves a destination copy.
async fn mark_deleted_and_expunge<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
    copied: bool,
) -> Option<PhaseFailure>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    if let Err(failure) =
        imap::uid_store_sequence(server, session, uids, "+FLAGS.SILENT (\\Deleted)").await
    {
        return Some(if copied {
            PhaseFailure::after("uid_store_deleted", failure, EFFECT_COPIED)
        } else {
            PhaseFailure::first("uid_store_deleted", failure)
        });
    }
    let effect = if copied {
        EFFECT_COPIED_AND_FLAGGED
    } else {
        EFFECT_FLAGGED
    };
    imap::uid_expunge_sequence(server, session, uids)
        .await
        .err()
        .map(|error| PhaseFailure::after("uid_expunge", error, effect))
}

/// Sends the commands of one copy, move, or delete group and stops at the first failure.
///
/// Nothing is retried and no alternate mutation command is sent after an error.
async fn run_message_action<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    action: &MessageActionInput,
    uids: &[u32],
    native_move: bool,
) -> GroupCommandOutcome
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    match action {
        MessageActionInput::Copy {
            destination_mailbox,
        } => GroupCommandOutcome::from_copy_move(
            "uid_copy",
            imap::uid_copy_sequence(server, session, uids, destination_mailbox).await,
        ),
        MessageActionInput::Move {
            destination_mailbox,
        } if native_move => GroupCommandOutcome::from_copy_move(
            "uid_move",
            imap::uid_move_sequence(server, session, uids, destination_mailbox).await,
        ),
        MessageActionInput::Move {
            destination_mailbox,
        } => {
            let mut outcome = GroupCommandOutcome::from_copy_move(
                "uid_copy",
                imap::uid_copy_sequence(server, session, uids, destination_mailbox).await,
            );
            if outcome.failure.is_none() {
                outcome.failure = mark_deleted_and_expunge(server, session, uids, true).await;
            }
            outcome
        }
        MessageActionInput::Delete => GroupCommandOutcome {
            mapping: None,
            mapping_issue: None,
            failure: mark_deleted_and_expunge(server, session, uids, false).await,
        },
    }
}

/// Assembles per-message results of a copy, move, or delete group.
///
/// New message ids use the destination mailbox, the reported destination UIDVALIDITY,
/// and the mapped destination UID. A mapping problem makes unmapped messages `partial`.
fn message_group_results(
    account_id: &str,
    group: &MessageMutationGroup,
    destination_mailbox: Option<&str>,
    outcome: &GroupCommandOutcome,
) -> Vec<MessageMutationResult> {
    // Destination name as it appears inside new message ids.
    let destination_name = destination_mailbox.map(normalize_mailbox_name);
    group
        .entries
        .iter()
        .map(|message_id| {
            let new_message_id = outcome
                .mapping
                .as_ref()
                .zip(destination_name.as_deref())
                .and_then(|(mapping, mailbox)| {
                    let uid = *mapping.source_to_destination.get(&message_id.uid)?;
                    Some(
                        MessageId {
                            account_id: account_id.to_owned(),
                            mailbox: mailbox.to_owned(),
                            uidvalidity: mapping.uidvalidity,
                            uid,
                        }
                        .encode(),
                    )
                });
            let has_new_id = new_message_id.is_some();
            // Status and issues of this message, starting from a clean `ok`.
            let mut status = "ok";
            let mut issues = Vec::new();
            if let Some(failure) = &outcome.failure {
                status = failure.status(has_new_id);
                issues.push(message_issue(failure.issue(has_new_id), message_id));
            }
            if !has_new_id && let Some(error) = &outcome.mapping_issue {
                issues.push(message_issue(
                    ToolIssue::from_error("copyuid_mapping", error).non_retryable(),
                    message_id,
                ));
                if status == "ok" {
                    status = "partial";
                }
            }
            let mut result = message_result(message_id, status, issues, destination_mailbox);
            result.new_message_id = new_message_id;
            result
        })
        .collect()
}

/// Builds the `STORE` query of a flag update.
fn flag_store_query(operation: SetOperation, flags: &[String]) -> String {
    let sign = match operation {
        SetOperation::Add => "+",
        SetOperation::Remove => "-",
        SetOperation::Replace => "",
    };
    format!("{sign}FLAGS.SILENT ({})", flags.join(" "))
}

/// Result of the commands of one flag or label update group.
struct PropertyOutcome {
    /// Failed STORE, if any.
    failure: Option<PhaseFailure>,
    /// Values observed after the STORE by UID, or the verification error.
    observed: AppResult<HashMap<u32, Vec<String>>>,
}

/// Sends the STORE of one property update and reads back the stored values.
///
/// Flags are verified with one `State` FETCH; labels come from the STORE response and
/// no follow-up FETCH is sent. Nothing is retried after any error.
async fn run_property_update<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    update: &PropertyUpdate,
    uids: &[u32],
) -> PropertyOutcome
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    match update {
        PropertyUpdate::Flags { operation, flags } => {
            let query = flag_store_query(*operation, flags);
            if let Err(failure) = imap::uid_store_sequence(server, session, uids, &query).await {
                return PropertyOutcome {
                    failure: Some(PhaseFailure::first("uid_store_flags", failure)),
                    observed: Ok(HashMap::new()),
                };
            }
            let observed = imap::fetch_message_metadata(
                server,
                session,
                uids,
                imap::MetadataProjection::State,
                false,
            )
            .await
            .map(|metadata| {
                metadata
                    .into_iter()
                    .filter_map(|(uid, metadata)| metadata.flags.map(|flags| (uid, flags)))
                    .collect()
            });
            PropertyOutcome {
                failure: None,
                observed,
            }
        }
        PropertyUpdate::GmailLabels { operation, labels } => {
            let outcome =
                imap::store_message_labels(server, session, uids, *operation, labels).await;
            match outcome.completion {
                Err(failure) => PropertyOutcome {
                    failure: Some(PhaseFailure::first("uid_store_labels", failure)),
                    observed: Ok(HashMap::new()),
                },
                Ok(()) => PropertyOutcome {
                    failure: None,
                    observed: Ok(outcome
                        .metadata
                        .into_iter()
                        .filter_map(|(uid, metadata)| {
                            metadata.gmail_labels.map(|labels| (uid, labels))
                        })
                        .collect()),
                },
            }
        }
    }
}

/// Assembles per-message results of a flag or label update group.
///
/// A completed STORE without observed values is `partial` and not retryable.
fn property_results(
    group: &MessageMutationGroup,
    update: &PropertyUpdate,
    outcome: &PropertyOutcome,
) -> Vec<MessageMutationResult> {
    if let Some(failure) = &outcome.failure {
        return group
            .entries
            .iter()
            .map(|message_id| {
                message_result(
                    message_id,
                    failure.status(false),
                    vec![message_issue(failure.issue(false), message_id)],
                    None,
                )
            })
            .collect();
    }
    let is_labels = matches!(update, PropertyUpdate::GmailLabels { .. });
    let (stage, noun) = if is_labels {
        ("store_labels", "labels")
    } else {
        ("fetch_flags", "flags")
    };
    group
        .entries
        .iter()
        .map(|message_id| {
            let observed = outcome
                .observed
                .as_ref()
                .ok()
                .and_then(|observed| observed.get(&message_id.uid));
            let mut result = message_result(message_id, "ok", Vec::new(), None);
            if let Some(values) = observed {
                if is_labels {
                    result.labels = Some(values.clone());
                } else {
                    result.flags = Some(values.clone());
                }
                return result;
            }
            let issue = match &outcome.observed {
                Err(error) => ToolIssue::from_error(stage, error).with_detail(&format!(
                    "the STORE completed but the {noun} could not be verified"
                )),
                Ok(_) => ToolIssue::from_error(
                    stage,
                    &AppError::Internal(format!("UID {} returned no {noun}", message_id.uid)),
                )
                .with_detail("the STORE completed"),
            };
            result.status = "partial".to_owned();
            result.issues = vec![message_issue(issue.non_retryable(), message_id)];
            result
        })
        .collect()
}

fn order_group_results(
    message_ids: &[MessageId],
    mut result_by_id: BTreeMap<String, MessageMutationResult>,
) -> Vec<MessageMutationResult> {
    message_ids
        .iter()
        .map(|message_id| {
            let encoded = message_id.encode();
            result_by_id.remove(&encoded).unwrap_or_else(|| {
                failed_message_result(
                    message_id,
                    vec![ToolIssue {
                        code: "internal".to_owned(),
                        stage: "result_assembly".to_owned(),
                        message: "missing mutation result for message".to_owned(),
                        retryable: true,
                        uid: Some(message_id.uid),
                        message_id: Some(encoded),
                    }],
                    None,
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use async_imap::{Client, Session};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    use tokio::task::JoinHandle;

    use super::{
        ExecutionCapabilities, FailureKind, GroupCommandOutcome, OperationExecutionContext,
        PhaseFailure, PropertyOutcome, build_bulk_message_response, cached_uidvalidity,
        check_message_action_capabilities, check_property_update_capabilities,
        evict_completed_operations, flag_store_query, message_group_results, message_result,
        property_results, run_message_action, run_property_update,
    };
    use crate::config::ServerConfig;
    use crate::errors::AppError;
    use crate::imap::CopyUidMapping;
    use crate::message_id::MessageId;
    use crate::models::{SetOperation, UpdateMessageLabelsInput};
    use crate::server::MailImapServer;
    use crate::server::tests::schema_test_server_config;
    use crate::server::types::{
        MailboxAction, MailboxManagementResult, ManageMailboxOperation, MessageActionInput,
        MessageMutationGroup, OperationResultData, OperationState, PropertyUpdate, StoredOperation,
        StoredOperationSpec, ToolIssue, UpdatePropertiesOperation,
    };

    fn completed_operation(operation_id: &str, finished_at: &str) -> StoredOperation {
        StoredOperation {
            operation_id: operation_id.to_owned(),
            kind: "imap_manage_mailbox".to_owned(),
            state: OperationState::Ok,
            created_at: finished_at.to_owned(),
            started_at: None,
            finished_at: Some(finished_at.to_owned()),
            cancel_supported: true,
            worker_started: false,
            progress: crate::server::types::OperationProgress::new(1, "ok"),
            issues: Vec::new(),
            result: Some(OperationResultData::MailboxManagement(
                MailboxManagementResult {
                    status: "ok".to_owned(),
                    issues: Vec::new(),
                    account_id: "default".to_owned(),
                    action: "create".to_owned(),
                    mailbox: "Archive".to_owned(),
                    destination_mailbox: None,
                },
            )),
            spec: StoredOperationSpec::ManageMailbox(ManageMailboxOperation {
                account_id: "default".to_owned(),
                action: MailboxAction::Create {
                    mailbox: "Archive".to_owned(),
                },
                completed: true,
                result: None,
            }),
        }
    }

    #[test]
    fn evicts_oldest_completed_operations_only() {
        let mut operations = BTreeMap::new();
        operations.insert(
            "done-old".to_owned(),
            completed_operation("done-old", "2026-01-01T00:00:00Z"),
        );
        operations.insert(
            "done-new".to_owned(),
            completed_operation("done-new", "2026-01-02T00:00:00Z"),
        );
        let mut running = completed_operation("running", "2026-01-03T00:00:00Z");
        running.state = OperationState::Running;
        running.finished_at = None;
        operations.insert("running".to_owned(), running);

        evict_completed_operations(&mut operations, 1);

        assert!(!operations.contains_key("done-old"));
        assert!(operations.contains_key("done-new"));
        assert!(operations.contains_key("running"));
    }

    #[test]
    fn schema_test_server_config_sets_operation_limit() {
        let config = ServerConfig {
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
            message_fetch_budget_bytes: 8_388_608,
            message_decode_budget_bytes: 16_777_216,
            mime_max_depth: 32,
            mime_max_parts: 250,
            attachment_extract_budget_bytes: 10_485_760,
        };
        assert_eq!(config.operation_max_entries, 256);
    }

    #[test]
    fn cached_uidvalidity_requires_server_selected_value() {
        let error = cached_uidvalidity(&None).expect_err("missing cached uidvalidity must fail");
        assert!(matches!(error, AppError::Internal(_)));
    }

    /// Builds a message id in the default account.
    fn message_id(mailbox: &str, uidvalidity: u32, uid: u32) -> MessageId {
        MessageId {
            account_id: "default".to_owned(),
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
        }
    }

    /// Builds an INBOX group with UIDVALIDITY 7 for the given UIDs.
    fn group_of(uids: &[u32]) -> MessageMutationGroup {
        MessageMutationGroup {
            mailbox: "INBOX".to_owned(),
            uidvalidity: 7,
            entries: uids
                .iter()
                .map(|uid| message_id("INBOX", 7, *uid))
                .collect(),
        }
    }

    /// Builds the mapping 4 to 41, 5 to 42, and 9 to 99 in UIDVALIDITY 777.
    fn sample_mapping() -> CopyUidMapping {
        CopyUidMapping {
            uidvalidity: 777,
            source_to_destination: HashMap::from([(4, 41), (5, 42), (9, 99)]),
        }
    }

    /// Builds a command outcome from its parts.
    fn outcome(
        mapping: Option<CopyUidMapping>,
        mapping_issue: Option<AppError>,
        failure: Option<PhaseFailure>,
    ) -> GroupCommandOutcome {
        GroupCommandOutcome {
            mapping,
            mapping_issue,
            failure,
        }
    }

    /// Builds a rejected first-phase failure.
    fn rejected(stage: &'static str) -> PhaseFailure {
        PhaseFailure {
            stage,
            error: AppError::Internal("NO refused".to_owned()),
            kind: FailureKind::Rejected,
        }
    }

    /// Server capabilities for guard tests.
    fn caps(
        supports_move: bool,
        supports_uidplus: bool,
        supports_gmail: bool,
    ) -> ExecutionCapabilities {
        ExecutionCapabilities {
            supports_move,
            supports_uidplus,
            supports_gmail,
        }
    }

    fn move_action() -> MessageActionInput {
        MessageActionInput::Move {
            destination_mailbox: "Archive".to_owned(),
        }
    }

    fn copy_action() -> MessageActionInput {
        MessageActionInput::Copy {
            destination_mailbox: "Archive".to_owned(),
        }
    }

    #[test]
    fn capability_guard_rejects_delete_and_fallback_move_without_uidplus() {
        let expected = "server lacks UIDPLUS; delete and copy-based move are unavailable without safe UID EXPUNGE";
        for action in [MessageActionInput::Delete, move_action()] {
            let error = check_message_action_capabilities(&action, caps(false, false, false))
                .expect_err("no UIDPLUS and no MOVE must reject");
            assert!(matches!(&error, AppError::Conflict(message) if message == expected));
        }
        let error = check_message_action_capabilities(
            &MessageActionInput::Delete,
            caps(true, false, false),
        )
        .expect_err("native MOVE does not make delete safe");
        assert!(matches!(error, AppError::Conflict(_)));
    }

    #[test]
    fn capability_guard_allows_native_move_without_uidplus_and_copy_always() {
        check_message_action_capabilities(&move_action(), caps(true, false, false))
            .expect("native MOVE without UIDPLUS is allowed");
        check_message_action_capabilities(&move_action(), caps(false, true, false))
            .expect("copy-based move with UIDPLUS is allowed");
        check_message_action_capabilities(&MessageActionInput::Delete, caps(false, true, false))
            .expect("delete with UIDPLUS is allowed");
        for capabilities in [caps(false, false, false), caps(true, true, true)] {
            check_message_action_capabilities(&copy_action(), capabilities)
                .expect("copy needs neither MOVE nor UIDPLUS");
        }
    }

    #[test]
    fn gmail_label_guard_returns_exact_invalid_input() {
        let update = PropertyUpdate::GmailLabels {
            operation: SetOperation::Add,
            labels: vec!["Work".to_owned()],
        };
        let error = check_property_update_capabilities(&update, caps(true, true, false))
            .expect_err("labels need X-GM-EXT-1");
        assert!(matches!(
            &error,
            AppError::InvalidInput(message)
                if message == "Gmail labels require a server that advertises X-GM-EXT-1"
        ));
        check_property_update_capabilities(&update, caps(false, false, true))
            .expect("Gmail server accepts labels");
        let flags = PropertyUpdate::Flags {
            operation: SetOperation::Add,
            flags: vec!["\\Seen".to_owned()],
        };
        check_property_update_capabilities(&flags, caps(false, false, false))
            .expect("flags do not need Gmail");
    }

    #[test]
    fn replacing_execution_session_resets_cached_state() {
        let mut context = OperationExecutionContext {
            account_id: Some("default".to_owned()),
            selected_mailbox: Some("INBOX".to_owned()),
            selected_readonly: Some(false),
            selected_uidvalidity: Some(7),
            capabilities: Some(caps(true, true, true)),
            ..OperationExecutionContext::default()
        };
        context.clear_session_cache();
        assert!(context.capabilities.is_none());
        assert!(context.selected_mailbox.is_none());
        assert!(context.selected_readonly.is_none());
        assert!(context.selected_uidvalidity.is_none());
        assert_eq!(context.account_id.as_deref(), Some("default"));

        context.capabilities = Some(caps(false, false, false));
        context.selected_mailbox = Some("INBOX".to_owned());
        context.discard_session();
        assert!(context.capabilities.is_none());
        assert!(context.selected_mailbox.is_none());
        assert!(context.account_id.is_none());
        assert!(context.session.is_none());
    }

    #[test]
    fn copy_results_carry_destination_ids_from_mapping() {
        let group = group_of(&[4, 5, 9]);
        let results = message_group_results(
            "default",
            &group,
            Some("Archive/2024"),
            &outcome(Some(sample_mapping()), None, None),
        );
        let ids = results
            .iter()
            .map(|result| result.new_message_id.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                Some("imap:default:Archive/2024:777:41"),
                Some("imap:default:Archive/2024:777:42"),
                Some("imap:default:Archive/2024:777:99"),
            ]
        );
        assert!(results.iter().all(|result| result.status == "ok"));
        assert!(results.iter().all(|result| result.issues.is_empty()));
    }

    #[test]
    fn destination_inbox_is_canonical_in_new_ids() {
        let results = message_group_results(
            "default",
            &group_of(&[4]),
            Some("inbox"),
            &outcome(Some(sample_mapping()), None, None),
        );
        assert_eq!(
            results[0].new_message_id.as_deref(),
            Some("imap:default:INBOX:777:41")
        );
    }

    #[test]
    fn missing_mapping_without_issue_is_ok_without_new_id() {
        let results = message_group_results(
            "default",
            &group_of(&[4]),
            Some("Archive"),
            &outcome(None, None, None),
        );
        assert_eq!(results[0].status, "ok");
        assert!(results[0].new_message_id.is_none());
        assert!(results[0].issues.is_empty());
    }

    #[test]
    fn mapping_issue_makes_applied_messages_partial() {
        let results = message_group_results(
            "default",
            &group_of(&[4, 5]),
            Some("Archive"),
            &outcome(
                None,
                Some(AppError::Internal("malformed COPYUID".to_owned())),
                None,
            ),
        );
        for result in &results {
            assert_eq!(result.status, "partial");
            assert!(result.new_message_id.is_none());
            assert_eq!(result.issues.len(), 1);
            assert_eq!(result.issues[0].stage, "copyuid_mapping");
            assert!(!result.issues[0].retryable);
        }
    }

    #[test]
    fn rejected_copy_after_mapping_keeps_mapped_ids_partial() {
        let group = group_of(&[4, 5, 6]);
        let mapping = CopyUidMapping {
            uidvalidity: 777,
            source_to_destination: HashMap::from([(4, 41), (5, 42)]),
        };
        let results = message_group_results(
            "default",
            &group,
            Some("Archive"),
            &outcome(Some(mapping), None, Some(rejected("uid_copy"))),
        );
        assert_eq!(results[0].status, "partial");
        assert_eq!(
            results[0].new_message_id.as_deref(),
            Some("imap:default:Archive:777:41")
        );
        assert!(!results[0].issues[0].retryable);
        assert_eq!(results[1].status, "partial");
        assert_eq!(results[2].status, "failed");
        assert!(results[2].new_message_id.is_none());
        assert_eq!(results[2].issues[0].stage, "uid_copy");
    }

    #[test]
    fn rejected_first_command_without_mapping_is_failed() {
        let results = message_group_results(
            "default",
            &group_of(&[4]),
            None,
            &outcome(None, None, Some(rejected("uid_store_deleted"))),
        );
        assert_eq!(results[0].status, "failed");
    }

    #[test]
    fn uncertain_copy_is_partial_not_retryable_and_keeps_ids() {
        let failure = PhaseFailure {
            stage: "uid_move",
            error: AppError::Timeout("UID MOVE timed out".to_owned()),
            kind: FailureKind::Unknown,
        };
        let results = message_group_results(
            "default",
            &group_of(&[4, 5, 9]),
            Some("Archive"),
            &outcome(Some(sample_mapping()), None, Some(failure)),
        );
        for result in &results {
            assert_eq!(result.status, "partial");
            assert!(result.new_message_id.is_some());
            let issue = &result.issues[0];
            assert!(!issue.retryable);
            assert_eq!(issue.code, "timeout");
            assert!(issue.message.contains("unknown"), "{}", issue.message);
            assert!(issue.message.contains("before another write"));
        }
    }

    #[test]
    fn later_phase_failure_keeps_ids_and_names_the_phase() {
        let failure = PhaseFailure::after(
            "uid_expunge",
            AppError::Internal("UID EXPUNGE was rejected".to_owned()),
            super::EFFECT_COPIED_AND_FLAGGED,
        );
        let results = message_group_results(
            "default",
            &group_of(&[4, 5]),
            Some("Archive"),
            &outcome(Some(sample_mapping()), None, Some(failure)),
        );
        for result in &results {
            assert_eq!(result.status, "partial", "expunge rejection is never ok");
            assert!(result.new_message_id.is_some());
            let issue = &result.issues[0];
            assert_eq!(issue.stage, "uid_expunge");
            assert!(!issue.retryable);
            assert!(issue.message.contains("copied to the destination"));
            assert!(issue.message.contains("uid_expunge did not complete"));
        }
    }

    #[test]
    fn flag_store_queries_use_silent_forms() {
        let flags = vec!["\\Seen".to_owned(), "Work".to_owned()];
        assert_eq!(
            flag_store_query(SetOperation::Add, &flags),
            "+FLAGS.SILENT (\\Seen Work)"
        );
        assert_eq!(
            flag_store_query(SetOperation::Remove, &flags),
            "-FLAGS.SILENT (\\Seen Work)"
        );
        assert_eq!(
            flag_store_query(SetOperation::Replace, &flags),
            "FLAGS.SILENT (\\Seen Work)"
        );
    }

    /// Builds a property outcome with observed values.
    fn observed(values: &[(u32, &[&str])]) -> PropertyOutcome {
        PropertyOutcome {
            failure: None,
            observed: Ok(values
                .iter()
                .map(|(uid, items)| (*uid, items.iter().map(|item| (*item).to_owned()).collect()))
                .collect()),
        }
    }

    fn flag_update() -> PropertyUpdate {
        PropertyUpdate::Flags {
            operation: SetOperation::Add,
            flags: vec!["\\Seen".to_owned()],
        }
    }

    fn label_update() -> PropertyUpdate {
        PropertyUpdate::GmailLabels {
            operation: SetOperation::Add,
            labels: vec!["Work".to_owned()],
        }
    }

    #[test]
    fn verified_flags_are_ok_and_missing_flags_are_partial() {
        let results = property_results(
            &group_of(&[4, 5]),
            &flag_update(),
            &observed(&[(4, &["\\Seen"])]),
        );
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            results[0].flags.as_deref(),
            Some(&["\\Seen".to_owned()][..])
        );
        assert!(results[0].labels.is_none());
        assert_eq!(results[1].status, "partial");
        assert!(results[1].flags.is_none());
        assert!(!results[1].issues[0].retryable);
        assert!(results[1].issues[0].message.contains("STORE completed"));
    }

    #[test]
    fn failed_flag_verification_is_partial_and_not_retryable() {
        let outcome = PropertyOutcome {
            failure: None,
            observed: Err(AppError::Internal("UID FETCH was rejected".to_owned())),
        };
        let results = property_results(&group_of(&[4]), &flag_update(), &outcome);
        assert_eq!(results[0].status, "partial");
        assert_eq!(results[0].issues[0].stage, "fetch_flags");
        assert!(!results[0].issues[0].retryable);
        assert!(results[0].issues[0].message.contains("STORE completed"));
    }

    #[test]
    fn label_results_use_returned_labels_and_flag_missing_ones_partial() {
        let results = property_results(
            &group_of(&[4, 5]),
            &label_update(),
            &observed(&[(4, &["\\Inbox", "Work"])]),
        );
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            results[0].labels.as_deref(),
            Some(&["\\Inbox".to_owned(), "Work".to_owned()][..])
        );
        assert!(results[0].flags.is_none());
        assert_eq!(results[1].status, "partial");
        assert!(results[1].labels.is_none());
        assert!(!results[1].issues[0].retryable);
        assert!(results[1].issues[0].message.contains("STORE completed"));
    }

    #[test]
    fn observed_empty_label_set_is_ok() {
        let results = property_results(&group_of(&[4]), &label_update(), &observed(&[(4, &[])]));
        assert_eq!(results[0].status, "ok");
        assert_eq!(results[0].labels.as_deref(), Some(&[][..]));
    }

    #[test]
    fn rejected_and_uncertain_stores_map_to_failed_and_partial() {
        let rejected_outcome = PropertyOutcome {
            failure: Some(rejected("uid_store_flags")),
            observed: Ok(HashMap::new()),
        };
        let results = property_results(&group_of(&[4]), &flag_update(), &rejected_outcome);
        assert_eq!(results[0].status, "failed");
        assert!(results[0].flags.is_none());

        let uncertain_outcome = PropertyOutcome {
            failure: Some(PhaseFailure {
                stage: "uid_store_labels",
                error: AppError::Timeout("UID STORE timed out".to_owned()),
                kind: FailureKind::Unknown,
            }),
            observed: Ok(HashMap::new()),
        };
        let results = property_results(&group_of(&[4]), &label_update(), &uncertain_outcome);
        assert_eq!(results[0].status, "partial");
        assert!(!results[0].issues[0].retryable);
    }

    /// Builds a result with the given status and one issue when the status is not `ok`.
    fn status_result(uid: u32, status: &str) -> super::MessageMutationResult {
        let id = message_id("INBOX", 7, uid);
        let issues = if status == "ok" {
            Vec::new()
        } else {
            vec![ToolIssue::from_error(
                "uid_store_flags",
                &AppError::Internal("problem".to_owned()),
            )]
        };
        message_result(&id, status, issues, None)
    }

    fn bulk(results: Vec<super::MessageMutationResult>) -> super::BulkMessageOperationData {
        let OperationResultData::BulkMessage(data) =
            build_bulk_message_response("default".to_owned(), None, Some("add"), results)
                .expect("response must build")
        else {
            panic!("expected a bulk result");
        };
        data
    }

    #[test]
    fn bulk_response_counts_ok_partial_and_failed_messages() {
        let data = bulk(vec![
            status_result(1, "ok"),
            status_result(2, "partial"),
            status_result(3, "failed"),
            status_result(4, "failed"),
        ]);
        assert_eq!(
            (data.matched, data.succeeded, data.partial, data.failed),
            (4, 1, 1, 2)
        );
        assert_eq!(data.status, "partial");
    }

    #[test]
    fn bulk_response_aggregate_status_follows_counts() {
        assert_eq!(bulk(vec![status_result(1, "ok")]).status, "ok");
        assert_eq!(bulk(vec![status_result(1, "partial")]).status, "partial");
        assert_eq!(
            bulk(vec![status_result(1, "failed"), status_result(2, "failed")]).status,
            "failed"
        );
        assert_eq!(
            bulk(vec![
                status_result(1, "partial"),
                status_result(2, "failed")
            ])
            .status,
            "partial"
        );
    }

    #[tokio::test]
    async fn finalize_operation_keeps_partial_results_partial() {
        let server = MailImapServer::new(schema_test_server_config());
        let id = message_id("INBOX", 7, 4);
        let mut result_by_id = BTreeMap::new();
        result_by_id.insert(id.encode(), status_result(4, "partial"));
        let operation_id = server
            .create_operation(StoredOperationSpec::UpdateProperties(
                UpdatePropertiesOperation {
                    account_id: "default".to_owned(),
                    update: label_update(),
                    message_ids: vec![id],
                    groups: vec![group_of(&[4])],
                    next_group_index: 1,
                    result_by_id,
                },
            ))
            .await;

        server
            .finalize_operation(&operation_id)
            .await
            .expect("finalize must succeed");
        let response = server
            .operation_response(&operation_id, true)
            .await
            .expect("operation response must be available");

        assert_eq!(response.operation.kind, "imap_update_message_labels");
        assert_eq!(response.status, "partial");
        assert_eq!(response.operation.state, "partial");
        let Some(OperationResultData::BulkMessage(data)) = response.result else {
            panic!("expected a bulk result");
        };
        assert_eq!((data.succeeded, data.partial, data.failed), (0, 1, 0));
    }

    #[tokio::test]
    async fn label_update_is_rejected_when_writes_are_disabled() {
        let server = MailImapServer::new(schema_test_server_config());
        let error = server
            .update_message_labels_impl(UpdateMessageLabelsInput {
                message_ids: vec!["imap:default:INBOX:7:4".to_owned()],
                operation: SetOperation::Add,
                labels: vec!["Work".to_owned()],
            })
            .await
            .expect_err("disabled writes must be rejected");
        assert!(matches!(&error, AppError::InvalidInput(message)
            if message == "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true"));
    }

    #[tokio::test]
    async fn label_update_rejects_cross_account_ids_before_network_use() {
        let server = MailImapServer::new(ServerConfig {
            write_enabled: true,
            ..schema_test_server_config()
        });
        let error = server
            .update_message_labels_impl(UpdateMessageLabelsInput {
                message_ids: vec![
                    "imap:default:INBOX:7:4".to_owned(),
                    "imap:other:INBOX:7:5".to_owned(),
                ],
                operation: SetOperation::Add,
                labels: vec!["Work".to_owned()],
            })
            .await
            .expect_err("mixed accounts must be rejected");
        assert!(matches!(&error, AppError::InvalidInput(message)
            if message == "message_ids must all belong to the same account"));
    }

    #[tokio::test]
    async fn label_update_validates_labels_before_network_use() {
        let server = MailImapServer::new(ServerConfig {
            write_enabled: true,
            ..schema_test_server_config()
        });
        let error = server
            .update_message_labels_impl(UpdateMessageLabelsInput {
                message_ids: vec!["imap:default:INBOX:7:4".to_owned()],
                operation: SetOperation::Add,
                labels: vec!["bad\nlabel".to_owned()],
            })
            .await
            .expect_err("control characters must be rejected");
        assert!(error.to_string().contains("control characters"));
    }

    /// Starts a scripted IMAP peer; a `None` step closes the connection instead of replying.
    ///
    /// Returns an authenticated session and a task that yields every command the peer saw.
    async fn scripted_session(
        script: Vec<Option<String>>,
    ) -> (Session<DuplexStream>, JoinHandle<Vec<String>>) {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let peer = tokio::spawn(async move {
            let (read_half, mut writer) = tokio::io::split(server_io);
            let mut reader = BufReader::new(read_half);
            writer.write_all(b"* OK ready\r\n").await.expect("greeting");
            let mut commands = Vec::new();
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("login line");
            let tag = line.split(' ').next().unwrap_or_default().to_owned();
            writer
                .write_all(format!("{tag} OK logged in\r\n").as_bytes())
                .await
                .expect("login reply");
            for step in script {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return commands;
                }
                commands.push(line.trim_end().to_owned());
                let tag = line.split(' ').next().unwrap_or_default().to_owned();
                let Some(text) = step else {
                    return commands;
                };
                writer
                    .write_all(text.replace("{tag}", &tag).as_bytes())
                    .await
                    .expect("scripted reply");
            }
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return commands,
                    Ok(_) => commands.push(line.trim_end().to_owned()),
                }
            }
        });
        let mut client = Client::new(client_io);
        client.read_response().await.expect("greeting read");
        let session = client
            .login("user", "pass")
            .await
            .map_err(|(error, _)| error)
            .expect("login");
        (session, peer)
    }

    /// Joins reply lines with CRLF terminators as one scripted reply.
    fn reply(lines: &[&str]) -> Option<String> {
        Some(lines.iter().map(|line| format!("{line}\r\n")).collect())
    }

    /// Server configuration whose socket timeout makes an unexpected extra command fail fast.
    fn fast_server() -> ServerConfig {
        ServerConfig {
            socket_timeout_ms: 2_000,
            ..schema_test_server_config()
        }
    }

    #[tokio::test]
    async fn delete_expunge_rejection_is_partial_after_exactly_two_commands() {
        let (mut session, peer) = scripted_session(vec![
            reply(&["{tag} OK stored"]),
            reply(&["{tag} NO expunge refused"]),
        ])
        .await;
        let outcome = run_message_action(
            &fast_server(),
            &mut session,
            &MessageActionInput::Delete,
            &[4, 5],
            false,
        )
        .await;
        drop(session);
        let commands = peer.await.expect("peer task");

        assert_eq!(commands.len(), 2, "{commands:?}");
        assert!(commands[0].ends_with("UID STORE 4:5 +FLAGS.SILENT (\\Deleted)"));
        assert!(commands[1].ends_with("UID EXPUNGE 4:5"));
        let results = message_group_results("default", &group_of(&[4, 5]), None, &outcome);
        for result in &results {
            assert_eq!(result.status, "partial");
            assert_eq!(result.issues[0].stage, "uid_expunge");
            assert!(!result.issues[0].retryable);
        }
    }

    #[tokio::test]
    async fn delete_store_rejection_is_failed_and_sends_no_expunge() {
        let (mut session, peer) = scripted_session(vec![reply(&["{tag} NO read-only"])]).await;
        let outcome = run_message_action(
            &fast_server(),
            &mut session,
            &MessageActionInput::Delete,
            &[4],
            false,
        )
        .await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
        let results = message_group_results("default", &group_of(&[4]), None, &outcome);
        assert_eq!(results[0].status, "failed");
    }

    #[tokio::test]
    async fn copy_based_move_keeps_mapping_when_expunge_is_rejected() {
        let (mut session, peer) = scripted_session(vec![
            reply(&["{tag} OK [COPYUID 777 4:5 41:42] copied"]),
            reply(&["{tag} OK stored"]),
            reply(&["{tag} BAD expunge refused"]),
        ])
        .await;
        let outcome =
            run_message_action(&fast_server(), &mut session, &move_action(), &[4, 5], false).await;
        drop(session);
        let commands = peer.await.expect("peer task");

        assert_eq!(commands.len(), 3, "{commands:?}");
        assert!(commands[0].contains("UID COPY 4:5"));
        assert!(commands[1].contains("UID STORE 4:5"));
        assert!(commands[2].contains("UID EXPUNGE 4:5"));
        let results =
            message_group_results("default", &group_of(&[4, 5]), Some("Archive"), &outcome);
        assert_eq!(results[0].status, "partial");
        assert_eq!(
            results[0].new_message_id.as_deref(),
            Some("imap:default:Archive:777:41")
        );
        assert_eq!(
            results[1].new_message_id.as_deref(),
            Some("imap:default:Archive:777:42")
        );
        assert_eq!(results[0].issues[0].stage, "uid_expunge");
    }

    #[tokio::test]
    async fn copy_based_move_store_failure_after_copy_is_partial() {
        let (mut session, peer) = scripted_session(vec![
            reply(&["{tag} OK [COPYUID 777 4 41] copied"]),
            reply(&["{tag} NO store refused"]),
        ])
        .await;
        let outcome =
            run_message_action(&fast_server(), &mut session, &move_action(), &[4], false).await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 2);
        let results = message_group_results("default", &group_of(&[4]), Some("Archive"), &outcome);
        assert_eq!(results[0].status, "partial");
        assert!(results[0].new_message_id.is_some());
        assert_eq!(results[0].issues[0].stage, "uid_store_deleted");
    }

    #[tokio::test]
    async fn uncertain_copy_is_partial_and_never_replayed() {
        let (mut session, peer) = scripted_session(vec![None]).await;
        let outcome =
            run_message_action(&fast_server(), &mut session, &move_action(), &[4], false).await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
        let results = message_group_results("default", &group_of(&[4]), Some("Archive"), &outcome);
        assert_eq!(results[0].status, "partial");
        assert!(!results[0].issues[0].retryable);
        assert!(results[0].issues[0].message.contains("unknown"));
    }

    #[tokio::test]
    async fn native_move_uses_one_command_and_reports_mapping() {
        let (mut session, peer) = scripted_session(vec![reply(&[
            "* OK [COPYUID 5 4 40] ",
            "* 1 EXPUNGE",
            "{tag} OK moved",
        ])])
        .await;
        let outcome =
            run_message_action(&fast_server(), &mut session, &move_action(), &[4], true).await;
        drop(session);
        let commands = peer.await.expect("peer task");
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains("UID MOVE 4"));
        let results = message_group_results("default", &group_of(&[4]), Some("Archive"), &outcome);
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            results[0].new_message_id.as_deref(),
            Some("imap:default:Archive:5:40")
        );
    }

    #[tokio::test]
    async fn flag_store_followed_by_rejected_verification_is_partial_without_replay() {
        let (mut session, peer) = scripted_session(vec![
            reply(&["{tag} OK stored"]),
            reply(&["{tag} NO fetch refused"]),
        ])
        .await;
        let outcome = run_property_update(&fast_server(), &mut session, &flag_update(), &[4]).await;
        drop(session);
        let commands = peer.await.expect("peer task");

        assert_eq!(commands.len(), 2, "{commands:?}");
        assert!(commands[0].ends_with("UID STORE 4 +FLAGS.SILENT (\\Seen)"));
        assert!(commands[1].contains("UID FETCH 4 (UID FLAGS)"));
        assert!(outcome.failure.is_none());
        let results = property_results(&group_of(&[4]), &flag_update(), &outcome);
        assert_eq!(results[0].status, "partial");
        assert!(!results[0].issues[0].retryable);
    }

    #[tokio::test]
    async fn flag_store_with_verification_returns_flags() {
        let (mut session, peer) = scripted_session(vec![
            reply(&["{tag} OK stored"]),
            reply(&[
                "* 1 FETCH (UID 4 FLAGS (\\Seen Work))",
                "* 2 FETCH (UID 99 FLAGS (\\Deleted))",
                "{tag} OK done",
            ]),
        ])
        .await;
        let outcome =
            run_property_update(&fast_server(), &mut session, &flag_update(), &[4, 5]).await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 2);
        let results = property_results(&group_of(&[4, 5]), &flag_update(), &outcome);
        assert_eq!(results[0].status, "ok");
        assert_eq!(
            results[0].flags.as_deref(),
            Some(&["\\Seen".to_owned(), "Work".to_owned()][..])
        );
        assert_eq!(results[1].status, "partial");
    }

    #[tokio::test]
    async fn flag_store_rejection_is_failed_without_verification() {
        let (mut session, peer) = scripted_session(vec![reply(&["{tag} NO refused"])]).await;
        let outcome = run_property_update(&fast_server(), &mut session, &flag_update(), &[4]).await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
        let results = property_results(&group_of(&[4]), &flag_update(), &outcome);
        assert_eq!(results[0].status, "failed");
    }

    #[tokio::test]
    async fn label_store_uses_store_response_without_follow_up_fetch() {
        let (mut session, peer) = scripted_session(vec![reply(&[
            "* 1 FETCH (UID 7 X-GM-LABELS (\\Inbox Work))",
            "{tag} OK done",
        ])])
        .await;
        let outcome =
            run_property_update(&fast_server(), &mut session, &label_update(), &[7, 8]).await;
        drop(session);
        let commands = peer.await.expect("peer task");

        assert_eq!(commands.len(), 1, "no FETCH may follow a label STORE");
        assert!(commands[0].contains("UID STORE 7:8 +X-GM-LABELS (\"Work\")"));
        let results = property_results(&group_of(&[7, 8]), &label_update(), &outcome);
        assert_eq!(results[0].status, "ok");
        assert!(
            results[0]
                .labels
                .as_ref()
                .is_some_and(|labels| labels.iter().any(|label| label == "Work"))
        );
        assert_eq!(results[1].status, "partial");
        assert!(!results[1].issues[0].retryable);
    }

    #[tokio::test]
    async fn uncertain_label_store_is_partial_and_not_retried() {
        let (mut session, peer) = scripted_session(vec![None]).await;
        let outcome =
            run_property_update(&fast_server(), &mut session, &label_update(), &[7]).await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
        let results = property_results(&group_of(&[7]), &label_update(), &outcome);
        assert_eq!(results[0].status, "partial");
        assert!(!results[0].issues[0].retryable);
    }
}
