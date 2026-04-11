use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::error;

use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::mailbox_codec::normalize_mailbox_name;
use crate::message_id::MessageId;
use crate::models::{
    ApplyToMessagesInput, GetOperationInput, ManageMailboxInput, OperationIdInput,
    UpdateMessageFlagsInput,
};

use super::types::{
    ApplyMessagesOperation, BulkMessageOperationData, FlagUpdateRequest, MailboxAction,
    MailboxManagementResult, MessageActionInput, MessageMutationGroup, MessageMutationResult,
    OperationMetadata, OperationProgress, OperationResultData, OperationState, OperationStatusData,
    OperationStep, OperationStepOutcome, StoredOperation, StoredOperationSpec, ToolIssue,
    UpdateFlagsOperation, canceled_tool_issue, destination_mailbox_for_action, flag_operation_name,
    mailbox_action_display, mailbox_action_stage, message_action_name, next_action_get_operation,
    next_action_get_operation_with_result, next_operation_step, now_utc_string,
    operation_kind_label, operation_total_units, status_from_issue_and_counts,
};
use super::validation::{
    build_flag_update_request, build_mailbox_action, build_message_action, parse_bulk_message_ids,
    require_write_enabled, validate_account_id, validate_flag_update_request, validate_mailbox,
    validate_message_action, validate_operation_id,
};
use super::{MailImapServer, WRITE_INLINE_BUDGET_MS};

#[derive(Default)]
struct OperationExecutionContext {
    account_id: Option<String>,
    session: Option<imap::ImapSession>,
    selected_mailbox: Option<String>,
    selected_readonly: Option<bool>,
    selected_uidvalidity: Option<u32>,
    supports_move: Option<bool>,
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

    pub(super) async fn update_message_flags_impl(
        &self,
        input: UpdateMessageFlagsInput,
    ) -> AppResult<OperationStatusData> {
        require_write_enabled(&self.config)?;
        let request = build_flag_update_request(&input)?;
        validate_flag_update_request(&request)?;
        let (account_id, message_ids) = parse_bulk_message_ids(&input.message_ids)?;
        let spec = self
            .preflight_flag_operation(&account_id, request, message_ids)
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

    async fn preflight_flag_operation(
        &self,
        account_id: &str,
        request: FlagUpdateRequest,
        message_ids: Vec<MessageId>,
    ) -> AppResult<StoredOperationSpec> {
        let groups = group_message_ids(&message_ids);
        let account = self.config.get_account(account_id)?;
        let mut session = imap::connect_authenticated(&self.config, account).await?;
        self.validate_group_uidvalidities(&mut session, &groups, false)
            .await?;
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
            } => {
                let results = match action {
                    MessageActionInput::Move {
                        destination_mailbox,
                    } => {
                        self.execute_move_group(
                            execution_ctx,
                            account_id,
                            group,
                            destination_mailbox,
                        )
                        .await
                    }
                    MessageActionInput::Copy {
                        destination_mailbox,
                    } => {
                        self.execute_copy_group(
                            execution_ctx,
                            account_id,
                            group,
                            destination_mailbox,
                        )
                        .await
                    }
                    MessageActionInput::Delete => {
                        self.execute_delete_group(execution_ctx, account_id, group)
                            .await
                    }
                };
                OperationStepOutcome::MessageResults(results)
            }
            OperationStep::UpdateFlagsGroup {
                account_id,
                request,
                group,
            } => OperationStepOutcome::MessageResults(
                self.execute_flag_update_group(execution_ctx, account_id, group, request)
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
                    Some(super::types::flag_operation_name(&spec.request.operation)),
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
            } => match imap::create_parent_mailboxes(&self.config, session, destination_mailbox)
                .await
            {
                Ok(()) => {
                    imap::rename_mailbox(&self.config, session, mailbox, destination_mailbox).await
                }
                Err(error) => Err(error),
            },
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

    async fn execute_copy_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
        destination_mailbox: &str,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        if let Err(error) = self
            .ensure_selected_group(execution_ctx, account_id, group, false)
            .await
        {
            return failed_group_results(
                group,
                "select_mailbox_readwrite",
                &error,
                Some(destination_mailbox),
                None,
            );
        }
        let Some(session) = execution_ctx.session.as_mut() else {
            return failed_group_results(
                group,
                "connect_authenticated",
                &AppError::Internal("execution session unavailable".to_owned()),
                Some(destination_mailbox),
                None,
            );
        };
        let result =
            imap::uid_copy_sequence(&self.config, session, uid_set.as_str(), destination_mailbox)
                .await;
        let issues = result
            .err()
            .map(|error| group_issues(group, "uid_copy", &error))
            .unwrap_or_default();
        finalize_group_results(group, issues, Some(destination_mailbox), None, true)
    }

    async fn execute_move_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
        destination_mailbox: &str,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        if let Err(error) = self
            .ensure_selected_group(execution_ctx, account_id, group, false)
            .await
        {
            return failed_group_results(
                group,
                "select_mailbox_readwrite",
                &error,
                Some(destination_mailbox),
                None,
            );
        }
        let supports_move = match self.supports_move(execution_ctx, account_id).await {
            Ok(supports_move) => supports_move,
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
        let Some(session) = execution_ctx.session.as_mut() else {
            return failed_group_results(
                group,
                "connect_authenticated",
                &AppError::Internal("execution session unavailable".to_owned()),
                Some(destination_mailbox),
                None,
            );
        };

        if supports_move {
            let issues = imap::uid_move_sequence(
                &self.config,
                session,
                uid_set.as_str(),
                destination_mailbox,
            )
            .await
            .err()
            .map(|error| group_issues(group, "uid_move", &error))
            .unwrap_or_default();
            return finalize_group_results(group, issues, Some(destination_mailbox), None, true);
        }

        if let Err(error) =
            imap::uid_copy_sequence(&self.config, session, uid_set.as_str(), destination_mailbox)
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
            session,
            uid_set.as_str(),
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        {
            return finalize_group_results(
                group,
                group_issues(group, "uid_store_deleted", &error),
                Some(destination_mailbox),
                None,
                true,
            );
        }

        let issues = imap::uid_expunge_sequence(&self.config, session, uid_set.as_str())
            .await
            .err()
            .map(|error| group_issues(group, "uid_expunge", &error))
            .unwrap_or_default();
        finalize_group_results(group, issues, Some(destination_mailbox), None, true)
    }

    async fn execute_delete_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        if let Err(error) = self
            .ensure_selected_group(execution_ctx, account_id, group, false)
            .await
        {
            return failed_group_results(group, "select_mailbox_readwrite", &error, None, None);
        }
        let Some(session) = execution_ctx.session.as_mut() else {
            return failed_group_results(
                group,
                "connect_authenticated",
                &AppError::Internal("execution session unavailable".to_owned()),
                None,
                None,
            );
        };

        if let Err(error) = imap::uid_store_sequence(
            &self.config,
            session,
            uid_set.as_str(),
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        {
            return finalize_group_results(
                group,
                group_issues(group, "uid_store_deleted", &error),
                None,
                None,
                true,
            );
        }

        let issues = imap::uid_expunge_sequence(&self.config, session, uid_set.as_str())
            .await
            .err()
            .map(|error| group_issues(group, "uid_expunge", &error))
            .unwrap_or_default();
        finalize_group_results(group, issues, None, None, true)
    }

    async fn execute_flag_update_group(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
        group: &MessageMutationGroup,
        request: &FlagUpdateRequest,
    ) -> Vec<MessageMutationResult> {
        let uid_set = build_uid_set(&group.entries);
        if let Err(error) = self
            .ensure_selected_group(execution_ctx, account_id, group, false)
            .await
        {
            return failed_group_results(group, "select_mailbox_readwrite", &error, None, None);
        }
        let Some(session) = execution_ctx.session.as_mut() else {
            return failed_group_results(
                group,
                "connect_authenticated",
                &AppError::Internal("execution session unavailable".to_owned()),
                None,
                None,
            );
        };

        let query = match request.operation {
            super::types::FlagOperation::Add => {
                format!("+FLAGS.SILENT ({})", request.flags.join(" "))
            }
            super::types::FlagOperation::Remove => {
                format!("-FLAGS.SILENT ({})", request.flags.join(" "))
            }
            super::types::FlagOperation::Replace => {
                format!("FLAGS.SILENT ({})", request.flags.join(" "))
            }
        };

        if let Err(error) =
            imap::uid_store_sequence(&self.config, session, uid_set.as_str(), &query).await
        {
            return finalize_group_results(
                group,
                group_issues(group, "uid_store_flags", &error),
                None,
                None,
                false,
            );
        }

        let fetched_flags =
            match imap::fetch_flags_by_uid_set(&self.config, session, uid_set.as_str()).await {
                Ok(flags) => Some(flags),
                Err(error) => {
                    let issues = group_issues(group, "fetch_flags", &error);
                    return finalize_group_results(group, issues, None, None, false);
                }
            };

        let mut results = Vec::with_capacity(group.entries.len());
        for message_id in &group.entries {
            let encoded_message_id = message_id.encode();
            let flags = fetched_flags
                .as_ref()
                .and_then(|by_uid| by_uid.get(&message_id.uid).cloned());
            let mut issues = Vec::new();
            let has_flags = flags.is_some();
            if fetched_flags.is_some() && !has_flags {
                issues.push(ToolIssue {
                    code: "internal".to_owned(),
                    stage: "fetch_flags".to_owned(),
                    message: format!("UID {} missing from fetch_flags response", message_id.uid),
                    retryable: true,
                    uid: Some(message_id.uid),
                    message_id: Some(encoded_message_id.clone()),
                });
            }
            results.push(finalize_message_result(
                message_id,
                encoded_message_id,
                issues,
                None,
                flags,
                has_flags,
            ));
        }
        results
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
        execution_ctx.selected_mailbox = None;
        execution_ctx.selected_readonly = None;
        execution_ctx.selected_uidvalidity = None;
        execution_ctx.supports_move = None;
        Ok(())
    }

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

        let session = execution_ctx
            .session
            .as_mut()
            .ok_or_else(|| AppError::Internal("execution session unavailable".to_owned()))?;
        let current_uidvalidity = if readonly {
            imap::select_mailbox_readonly(&self.config, session, &group.mailbox).await?
        } else {
            imap::select_mailbox_readwrite(&self.config, session, &group.mailbox).await?
        };
        if current_uidvalidity != group.uidvalidity {
            return Err(AppError::Conflict(
                "message uidvalidity no longer matches mailbox".to_owned(),
            ));
        }
        execution_ctx.selected_mailbox = Some(group.mailbox.clone());
        execution_ctx.selected_readonly = Some(readonly);
        execution_ctx.selected_uidvalidity = Some(current_uidvalidity);
        Ok(())
    }

    async fn supports_move(
        &self,
        execution_ctx: &mut OperationExecutionContext,
        account_id: &str,
    ) -> AppResult<bool> {
        self.ensure_execution_session(execution_ctx, account_id)
            .await?;
        if let Some(supports_move) = execution_ctx.supports_move {
            return Ok(supports_move);
        }
        let session = execution_ctx
            .session
            .as_mut()
            .ok_or_else(|| AppError::Internal("execution session unavailable".to_owned()))?;
        let supports_move = imap::capabilities(&self.config, session)
            .await?
            .has_str("MOVE");
        execution_ctx.supports_move = Some(supports_move);
        Ok(supports_move)
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
        .map(|message_id| {
            ToolIssue::from_error(stage, error)
                .with_uid(message_id.uid)
                .with_message_id(&message_id.encode())
        })
        .collect()
}

fn failed_message_result(
    message_id: &MessageId,
    encoded_message_id: String,
    issues: Vec<ToolIssue>,
    destination_mailbox: Option<String>,
    flags: Option<Vec<String>>,
) -> MessageMutationResult {
    MessageMutationResult {
        message_id: encoded_message_id,
        status: "failed".to_owned(),
        issues,
        source_mailbox: message_id.mailbox.clone(),
        destination_mailbox,
        flags,
        new_message_id: None,
    }
}

fn finalize_message_result(
    message_id: &MessageId,
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
        source_mailbox: message_id.mailbox.clone(),
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
) -> AppResult<OperationResultData> {
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
    Ok(OperationResultData::BulkMessage(BulkMessageOperationData {
        status,
        issues,
        account_id,
        action: action.map(ToOwned::to_owned),
        operation: operation.map(ToOwned::to_owned),
        matched,
        attempted: matched,
        succeeded,
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
                    encoded.clone(),
                    vec![canceled_tool_issue(Some(message_id.uid), Some(encoded))],
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
        for message_id in &group.entries {
            let encoded = message_id.encode();
            result_by_id.insert(
                encoded.clone(),
                failed_message_result(
                    message_id,
                    encoded,
                    vec![
                        issue
                            .clone()
                            .with_uid(message_id.uid)
                            .with_message_id(&message_id.encode()),
                    ],
                    destination_mailbox.map(ToOwned::to_owned),
                    None,
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
        .map(|message_id| {
            failed_message_result(
                message_id,
                message_id.encode(),
                vec![
                    ToolIssue::from_error(stage, error)
                        .with_uid(message_id.uid)
                        .with_message_id(&message_id.encode()),
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
        .map(|message_id| {
            let encoded = message_id.encode();
            let message_issues = issues
                .iter()
                .filter(|issue| issue.message_id.as_deref() == Some(encoded.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            finalize_message_result(
                message_id,
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
        .map(|message_id| {
            let encoded = message_id.encode();
            result_by_id.remove(&encoded).unwrap_or_else(|| {
                failed_message_result(
                    message_id,
                    encoded,
                    vec![ToolIssue {
                        code: "internal".to_owned(),
                        stage: "result_assembly".to_owned(),
                        message: "missing mutation result for message".to_owned(),
                        retryable: true,
                        uid: Some(message_id.uid),
                        message_id: Some(message_id.encode()),
                    }],
                    None,
                    None,
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{cached_uidvalidity, evict_completed_operations};
    use crate::config::ServerConfig;
    use crate::errors::AppError;
    use crate::server::types::{
        MailboxAction, MailboxManagementResult, ManageMailboxOperation, OperationResultData,
        OperationState, StoredOperation, StoredOperationSpec,
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
        };
        assert_eq!(config.operation_max_entries, 256);
    }

    #[test]
    fn cached_uidvalidity_requires_server_selected_value() {
        let error = cached_uidvalidity(&None).expect_err("missing cached uidvalidity must fail");
        assert!(matches!(error, AppError::Internal(_)));
    }
}
