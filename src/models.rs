//! Input/output DTOs and schema-bearing types
//!
//! Defines all data structures used in MCP tool contracts. Each type is
//! annotated with `JsonSchema` for automatic schema generation.

use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Metadata included in all tool responses
///
/// Provides timing information and current UTC timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Meta {
    /// Current UTC timestamp in RFC 3339 format with milliseconds
    pub now_utc: String,
    /// Tool execution duration in milliseconds
    #[schemars(schema_with = "nonnegative_integer_schema")]
    pub duration_ms: u64,
}

impl Meta {
    /// Create metadata populated with current time and elapsed duration
    pub fn now(duration_ms: u64) -> Self {
        Self {
            now_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            duration_ms,
        }
    }
}

fn nonnegative_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 0
    })
}

fn remove_format(schema: &mut schemars::Schema) {
    schema.remove("format");
}

/// Standard response envelope for all tools
///
/// Wraps tool-specific data with human-readable summary and execution metadata.
/// This structure provides consistent response shape across all MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolEnvelope<T>
where
    T: JsonSchema,
{
    /// Human-readable summary of the operation outcome
    pub summary: String,
    /// Tool-specific data payload
    pub data: T,
    /// Execution metadata (timestamp, duration)
    pub meta: Meta,
}

/// Account metadata (no credentials)
///
/// Returned by `imap_list_accounts`. Password is intentionally excluded.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountInfo {
    /// Account identifier
    pub account_id: String,
    /// IMAP server hostname
    pub host: String,
    /// IMAP server port
    pub port: u16,
    /// Whether TLS is enabled (always true in this implementation)
    pub secure: bool,
}

/// Normalized semantic role for a special-use mailbox.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MailboxRole {
    Inbox,
    All,
    Archive,
    Drafts,
    Flagged,
    Important,
    Junk,
    Sent,
    Trash,
}

/// Mailbox/folder metadata
///
/// Returned by `imap_list_mailboxes`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MailboxInfo {
    /// Mailbox name (may contain path separators like `/` or `.`)
    pub name: String,
    /// Hierarchy delimiter if supported by server (e.g., `/`, `.`)
    pub delimiter: Option<String>,
    /// Raw IMAP LIST attributes, including unrecognized extensions.
    pub attributes: Vec<String>,
    /// Normalized special-use role when the server reports one.
    pub role: Option<MailboxRole>,
    /// Whether this mailbox may be selected.
    pub selectable: bool,
    /// Whether the provider manages this mailbox and disallows rename/delete.
    pub provider_managed: bool,
    /// Total messages from a live `STATUS`; present only when `include_counts` was requested and the server reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_messages: Option<u32>,
    /// Messages without `\Seen` from a live `STATUS`; present only when `include_counts` was requested and the server reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread_messages: Option<u32>,
}

/// Message summary for search results
///
/// Lightweight representation returned by `imap_search_messages`. Includes an
/// optional body preview and, on Gmail servers, Gmail labels and identities.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageSummary {
    /// Stable, opaque message identifier
    pub message_id: String,
    /// URI reference to message resource
    pub message_uri: String,
    /// URI reference to raw RFC822 source
    pub message_raw_uri: String,
    /// Mailbox name containing this message
    pub mailbox: String,
    /// Mailbox UIDVALIDITY at time of search
    pub uidvalidity: u32,
    /// Message UID within mailbox
    pub uid: u32,
    /// Parsed Date header
    pub date: Option<String>,
    /// Parsed From header
    pub from: Option<String>,
    /// Parsed Subject header
    pub subject: Option<String>,
    /// IMAP flags (e.g., `\Seen`, `\Flagged`)
    pub flags: Option<Vec<String>>,
    /// Whitespace-normalized body preview, present when `preview_max_chars` was requested and the message has text.
    pub preview: Option<String>,
    /// Gmail labels exactly as observed on Gmail servers; absent when the server did not return them.
    pub gmail_labels: Option<Vec<String>>,
    /// Gmail message identifier (`X-GM-MSGID`) as a decimal string.
    pub gmail_message_id: Option<String>,
    /// Gmail conversation identifier (`X-GM-THRID`) as a decimal string.
    pub gmail_thread_id: Option<String>,
}

/// Attachment metadata
///
/// Returned in message details. Includes optional extracted text for PDFs
/// when `extract_attachment_text=true`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentInfo {
    /// Filename if present in Content-Disposition or Content-Type
    pub filename: Option<String>,
    /// MIME content type (e.g., `application/pdf`, `image/jpeg`)
    pub content_type: String,
    /// Complete decoded payload size in bytes, or unavailable when not fully known
    pub size_bytes: Option<usize>,
    /// Part ID for MIME structure (e.g., `1`, `2`, `3.1`)
    pub part_id: String,
    /// Extracted text from PDF (if enabled and extraction succeeded)
    pub extracted_text: Option<String>,
}

/// Mutation mode shared by flag and Gmail label updates.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetOperation {
    Add,
    Remove,
    Replace,
}

/// Method used to discover the messages of a conversation search.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMethod {
    Gmail,
    Headers,
}

/// Requested body content mode for `imap_get_message`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BodyMode {
    Text,
    Html,
    Both,
}

/// Requested attachment handling mode for `imap_get_message`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentMode {
    None,
    Metadata,
    ExtractText,
}

/// Full message detail
///
/// Rich representation returned by `imap_get_message`. Includes all headers,
/// body content, and attachment metadata.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageDetail {
    /// Stable, opaque message identifier
    pub message_id: String,
    /// URI reference to message resource
    pub message_uri: String,
    /// URI reference to raw RFC822 source
    pub message_raw_uri: String,
    /// Mailbox name containing this message
    pub mailbox: String,
    /// Mailbox UIDVALIDITY
    pub uidvalidity: u32,
    /// Message UID within mailbox
    pub uid: u32,
    /// Parsed Date header
    pub date: Option<String>,
    /// Parsed From header
    pub from: Option<String>,
    /// Parsed To header
    pub to: Option<String>,
    /// Parsed Cc header
    pub cc: Option<String>,
    /// Parsed Subject header
    pub subject: Option<String>,
    /// IMAP flags (e.g., `\Seen`, `\Flagged`)
    pub flags: Option<Vec<String>>,
    /// Gmail labels exactly as observed on Gmail servers; absent when the server did not return them.
    pub gmail_labels: Option<Vec<String>>,
    /// Gmail message identifier (`X-GM-MSGID`) as a decimal string.
    pub gmail_message_id: Option<String>,
    /// Gmail conversation identifier (`X-GM-THRID`) as a decimal string.
    pub gmail_thread_id: Option<String>,
    /// All headers or curated subset (if `include_headers=true`)
    pub headers: Option<Vec<(String, String)>>,
    /// Plain text body (truncated to `body_max_chars`)
    pub body_text: Option<String>,
    /// Sanitized HTML body (if `include_html=true`, truncated)
    pub body_html: Option<String>,
    /// Attachment metadata (up to `MAX_ATTACHMENTS`)
    pub attachments: Option<Vec<AttachmentInfo>>,
}

/// Input: list mailboxes with cursor pagination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListMailboxesInput {
    /// Account identifier (defaults to `"default"`).
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Cursor from a previous mailbox-list response.
    pub cursor: Option<String>,
    /// Maximum mailboxes to return (1..200, or 1..50 when `include_counts` is true; default 50).
    #[serde(default = "default_mailbox_limit")]
    #[schemars(range(min = 1, max = 200), transform = remove_format)]
    pub limit: usize,
    /// Include live message and unread counts for the selectable mailboxes of the returned page (default false).
    #[serde(default)]
    pub include_counts: bool,
}

/// Input: search messages with pagination
///
/// Used by `imap_search_messages`. Supports multiple search criteria,
/// several mailboxes, conversation discovery, and cursor-based pagination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchMessagesInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Distinct mailboxes to search, 1..10 names (e.g., `INBOX`, `Sent`); results are grouped in this order.
    #[schemars(length(min = 1, max = 10), inner(length(min = 1, max = 256)))]
    pub mailboxes: Vec<String>,
    /// Pagination cursor from previous search result
    pub cursor: Option<String>,
    /// Full-text search query
    #[schemars(length(min = 1, max = 256))]
    pub query: Option<String>,
    /// Filter by From header
    #[schemars(length(min = 1, max = 256))]
    pub from: Option<String>,
    /// Filter by To header
    #[schemars(length(min = 1, max = 256))]
    pub to: Option<String>,
    /// Filter by Subject header
    #[schemars(length(min = 1, max = 256))]
    pub subject: Option<String>,
    /// Filter to unread messages only
    pub unread_only: Option<bool>,
    /// Filter by the `\Flagged` flag: `true` keeps flagged messages and `false` keeps unflagged messages.
    pub flagged: Option<bool>,
    /// Filter by the `\Answered` flag: `true` keeps answered messages and `false` keeps unanswered messages.
    pub answered: Option<bool>,
    /// Filter by one custom keyword (1..64 characters, an IMAP atom without a leading backslash).
    #[schemars(length(min = 1, max = 64))]
    pub keyword: Option<String>,
    /// Filter by Message-ID header text (substring match).
    #[schemars(length(min = 1, max = 256))]
    pub message_id_header: Option<String>,
    /// Gmail raw search expression (`X-GM-RAW`), accepted only on servers that advertise `X-GM-EXT-1`.
    #[schemars(length(min = 1, max = 256))]
    pub gmail_query: Option<String>,
    /// Stable message identifier whose conversation is searched; it must belong to the same account.
    pub thread_of_message_id: Option<String>,
    /// Filter to messages from last N days
    #[schemars(range(min = 1, max = 365), transform = remove_format)]
    pub last_days: Option<u16>,
    /// Filter to messages on or after this date (YYYY-MM-DD)
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub start_date: Option<String>,
    /// Filter to messages before this date (YYYY-MM-DD)
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub end_date: Option<String>,
    /// Maximum messages to return (1..100, default 10)
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100), transform = remove_format)]
    pub limit: usize,
    /// Maximum body preview length in characters (50..500). When omitted, previews are not included.
    #[schemars(range(min = 50, max = 500), transform = remove_format)]
    pub preview_max_chars: Option<usize>,
}

/// Input: get parsed message details
///
/// Used by `imap_get_message`. Supports bounded enrichment (char limits,
/// optional HTML, optional attachment text extraction).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMessageInput {
    /// Stable message identifier (format: `imap:{account}:{mailbox}:{uidvalidity}:{uid}`)
    pub message_id: String,
    /// Maximum body characters (1..16000, default 2000)
    #[serde(default = "default_body_max_chars")]
    #[schemars(range(min = 1, max = 16_000), transform = remove_format)]
    pub body_max_chars: usize,
    /// Requested body content mode
    #[serde(default = "default_body_mode")]
    pub body_mode: BodyMode,
    /// Include headers in response
    #[serde(default = "default_true")]
    pub include_headers: bool,
    /// Include all headers (if `true`, overrides curated header list)
    #[serde(default)]
    pub include_all_headers: bool,
    /// Requested attachment handling mode
    #[serde(default = "default_attachment_mode")]
    pub attachment_mode: AttachmentMode,
    /// Maximum attachment text length (1..64000, requires `attachment_mode=extract_text`)
    #[schemars(range(min = 1, max = 64_000), transform = remove_format)]
    pub attachment_text_max_chars: Option<usize>,
}

/// Input: get raw RFC822 message source
///
/// Used by `imap_get_message_raw`. Returns bounded message bytes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMessageRawInput {
    /// Stable message identifier
    pub message_id: String,
    /// Maximum message bytes to return (1..64000, default 16000)
    #[serde(default = "default_raw_max_bytes")]
    #[schemars(range(min = 1, max = 64_000), transform = remove_format)]
    pub max_bytes: usize,
    /// Starting offset in the raw RFC822 source
    #[serde(default)]
    #[schemars(transform = remove_format)]
    pub offset_bytes: usize,
}

/// Input: apply a bulk action to explicit messages.
///
/// Used by `imap_apply_to_messages`. Message discovery happens via read tools;
/// this command mutates only the provided stable message ids.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ApplyToMessagesInput {
    /// Stable message identifiers to mutate
    #[schemars(length(min = 1, max = 250))]
    pub message_ids: Vec<String>,
    /// Action to apply to the provided messages
    #[schemars(schema_with = "message_action_schema")]
    pub action: String,
    /// Destination mailbox for `move` and `copy`
    #[schemars(length(min = 1, max = 256))]
    pub destination_mailbox: Option<String>,
}

/// Input: update flags on explicit messages.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateMessageFlagsInput {
    /// Stable message identifiers to mutate
    #[schemars(length(min = 1, max = 250))]
    pub message_ids: Vec<String>,
    /// Flag mutation mode: `add`, `remove`, or `replace`
    pub operation: SetOperation,
    /// Standard IMAP system flags or server-defined keywords
    #[schemars(length(min = 1, max = 32))]
    pub flags: Vec<String>,
}

/// Input: add, remove, or replace Gmail labels on explicit messages.
///
/// Requires a server that advertises `X-GM-EXT-1`. Labels are 1..256
/// characters without ASCII control characters. `add` and `remove` need at
/// least one label; `replace` may be empty and then clears all labels.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateMessageLabelsInput {
    /// Stable message identifiers to mutate
    #[schemars(length(min = 1, max = 250))]
    pub message_ids: Vec<String>,
    /// Label mutation mode: `add`, `remove`, or `replace`
    pub operation: SetOperation,
    /// Gmail labels; empty is allowed only for `replace`, which clears all labels
    #[schemars(length(min = 0, max = 32))]
    pub labels: Vec<String>,
}

/// Input: create, rename, or delete a mailbox.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ManageMailboxInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Mailbox management action
    #[schemars(schema_with = "mailbox_action_schema")]
    pub action: String,
    /// Source or target mailbox name, depending on action
    #[schemars(length(min = 1, max = 256))]
    pub mailbox: String,
    /// Destination mailbox name for `rename`
    #[schemars(length(min = 1, max = 256))]
    pub destination_mailbox: Option<String>,
}

/// Input: cancel a previously started write operation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OperationIdInput {
    /// Opaque operation identifier returned by a write tool.
    #[schemars(length(min = 1, max = 64))]
    pub operation_id: String,
}

/// Input: fetch the status of a previously started write operation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetOperationInput {
    /// Opaque operation identifier returned by a write tool.
    #[schemars(length(min = 1, max = 64))]
    pub operation_id: String,
    /// Include the completed result payload when the operation is terminal.
    #[serde(default)]
    pub include_result: bool,
}

/// Representation of attachment content requested from `imap_get_attachment`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentContentMode {
    Text,
    Bytes,
}

/// Input: read one attachment of a message as text or base64 bytes.
///
/// Text mode accepts only `max_chars`; bytes mode accepts only `offset_bytes`
/// and `max_bytes`. Fields that do not apply to the chosen mode are rejected.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetAttachmentInput {
    /// Stable message identifier
    pub message_id: String,
    /// Part ID from `imap_get_message` attachments: `1` followed by dot-separated child indices (max 64 characters)
    #[schemars(length(min = 1, max = 64))]
    pub part_id: String,
    /// Content representation: `text` (extracted text, default) or `bytes` (base64 of decoded bytes)
    #[serde(default = "default_attachment_content_mode")]
    pub mode: AttachmentContentMode,
    /// Text mode only: maximum characters (1..64000, default 10000)
    #[schemars(range(min = 1, max = 64_000), transform = remove_format)]
    pub max_chars: Option<usize>,
    /// Bytes mode only: first decoded byte to return (default 0)
    #[schemars(transform = remove_format)]
    pub offset_bytes: Option<usize>,
    /// Bytes mode only: maximum decoded bytes (1..64000, default 16000)
    #[schemars(range(min = 1, max = 64_000), transform = remove_format)]
    pub max_bytes: Option<usize>,
}

/// Attachment content in the representation that was requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum AttachmentContent {
    /// Extracted text cut to the requested character limit.
    Text {
        /// Extracted text.
        text: String,
        /// True when the text was cut at `max_chars`; request a larger `max_chars` for more.
        truncated: bool,
    },
    /// One page of decoded bytes as standard base64.
    Base64 {
        /// Base64 of the returned decoded bytes.
        content_base64: String,
        /// Decoded offset of the first returned byte.
        offset_bytes: usize,
        /// Number of decoded bytes returned in this page.
        returned_bytes: usize,
        /// True when more decoded bytes follow this page.
        has_more: bool,
    },
}

/// One completely decoded attachment with the requested content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentResult {
    /// Filename if present in Content-Disposition or Content-Type
    pub filename: Option<String>,
    /// Lowercase MIME content type
    pub content_type: String,
    /// Complete decoded size in bytes
    pub size_bytes: usize,
    /// Requested content
    pub content: AttachmentContent,
}

/// Default value for `account_id` field
pub fn default_account_id() -> String {
    "default".to_owned()
}

/// Default value for `bool` fields (true)
fn default_true() -> bool {
    true
}

/// Default value for mailbox-list page size.
fn default_mailbox_limit() -> usize {
    50
}

/// Default value for `limit` in search
///
/// Chosen as a reasonable balance between response size and pagination overhead.
/// Most users need to see only the first few relevant messages.
fn default_limit() -> usize {
    10
}

/// Default value for `body_max_chars` in get_message
///
/// Provides enough context for most use cases without overwhelming output.
/// 2,000 characters is typically sufficient to understand message content.
fn default_body_max_chars() -> usize {
    2_000
}

fn default_body_mode() -> BodyMode {
    BodyMode::Text
}

/// Default content representation of `imap_get_attachment`.
fn default_attachment_content_mode() -> AttachmentContentMode {
    AttachmentContentMode::Text
}

fn default_attachment_mode() -> AttachmentMode {
    AttachmentMode::Metadata
}

/// Default value for `max_bytes` in get_message_raw
///
/// Large enough to capture common diagnostic slices without overwhelming
/// context. 16KB is a practical default for raw message inspection.
fn default_raw_max_bytes() -> usize {
    16_000
}

fn message_action_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["move", "copy", "delete"]
    })
}

fn mailbox_action_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["create", "rename", "delete"]
    })
}

#[cfg(test)]
pub(crate) fn validate_client_safe_input_schema(schema: &serde_json::Value) -> Result<(), String> {
    let type_is_object = schema
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|ty| ty == "object");
    if !type_is_object {
        return Err("root schema must have type=object".to_owned());
    }
    if !schema
        .get("properties")
        .is_some_and(serde_json::Value::is_object)
    {
        return Err("root schema must expose explicit properties".to_owned());
    }
    validate_client_safe_schema_node(schema, "$")
}

#[cfg(test)]
fn validate_client_safe_schema_node(value: &serde_json::Value, path: &str) -> Result<(), String> {
    match value {
        serde_json::Value::Object(object) => {
            for keyword in [
                "oneOf",
                "anyOf",
                "allOf",
                "not",
                "if",
                "then",
                "else",
                "dependentSchemas",
                "dependentRequired",
                "patternProperties",
                "unevaluatedProperties",
                "propertyNames",
            ] {
                if object.contains_key(keyword) {
                    return Err(format!(
                        "client-unsafe schema keyword '{keyword}' found at {path}"
                    ));
                }
            }

            if let Some(additional_properties) = object.get("additionalProperties") {
                match additional_properties {
                    serde_json::Value::Bool(false) => {}
                    _ => {
                        return Err(format!(
                            "client-unsafe additionalProperties found at {path}"
                        ));
                    }
                }
            }

            for (key, nested) in object {
                let nested_path = format!("{path}.{key}");
                validate_client_safe_schema_node(nested, &nested_path)?;
            }
            Ok(())
        }
        serde_json::Value::Array(values) => {
            for (index, nested) in values.iter().enumerate() {
                let nested_path = format!("{path}[{index}]");
                validate_client_safe_schema_node(nested, &nested_path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use rmcp::handler::server::common::schema_for_type;
    use serde_json::{Map, Value};

    use super::{
        ApplyToMessagesInput, AttachmentContent, AttachmentContentMode, AttachmentResult,
        GetAttachmentInput, GetMessageInput, GetMessageRawInput, GetOperationInput,
        ListMailboxesInput, MailboxInfo, ManageMailboxInput, MessageDetail, MessageSummary,
        OperationIdInput, SearchMessagesInput, UpdateMessageFlagsInput, UpdateMessageLabelsInput,
        validate_client_safe_input_schema,
    };

    #[test]
    fn input_schemas_do_not_publish_nonstandard_unsigned_formats() {
        for schema in [
            schema_for_type::<ListMailboxesInput>(),
            schema_for_type::<SearchMessagesInput>(),
            schema_for_type::<GetMessageInput>(),
            schema_for_type::<GetMessageRawInput>(),
            schema_for_type::<ApplyToMessagesInput>(),
            schema_for_type::<UpdateMessageFlagsInput>(),
            schema_for_type::<UpdateMessageLabelsInput>(),
            schema_for_type::<ManageMailboxInput>(),
            schema_for_type::<GetOperationInput>(),
            schema_for_type::<OperationIdInput>(),
            schema_for_type::<GetAttachmentInput>(),
        ] {
            assert_no_nonstandard_integer_formats(&Value::Object((*schema).clone()));
        }
    }

    #[test]
    fn list_mailboxes_schema_matches_runtime_bounds() {
        let schema = schema_for_type::<ListMailboxesInput>();
        let properties = schema["properties"]
            .as_object()
            .expect("list mailboxes schema must expose properties");

        assert_eq!(
            schema_string_property(properties, "account_id", "pattern"),
            Some("^[A-Za-z0-9_-]+$")
        );
        assert_schema_nullable_string(&schema, properties, "cursor");
        assert!(
            !schema_required_properties(&schema).contains(&"cursor"),
            "cursor must remain optional"
        );
        assert_eq!(
            schema_numeric_property(properties, "limit", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(properties, "limit", "maximum"),
            Some(200)
        );
        assert_eq!(
            schema_variant_for(properties, "limit")
                .and_then(|value| value.get("default"))
                .and_then(Value::as_u64),
            Some(50)
        );

        let mailbox_schema = schema_for_type::<MailboxInfo>();
        let mailbox_properties = mailbox_schema["properties"]
            .as_object()
            .expect("mailbox info schema must expose properties");
        let mut property_names = mailbox_properties
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        property_names.sort_unstable();
        assert_eq!(
            property_names,
            [
                "attributes",
                "delimiter",
                "name",
                "provider_managed",
                "role",
                "selectable",
                "total_messages",
                "unread_messages",
            ]
        );
        assert_eq!(
            schema_string_property(mailbox_properties, "name", "type"),
            Some("string")
        );
        assert_schema_nullable_string(&mailbox_schema, mailbox_properties, "delimiter");
        assert_eq!(
            schema_string_property(mailbox_properties, "attributes", "type"),
            Some("array")
        );
        assert_eq!(
            schema_variant_for(mailbox_properties, "attributes")
                .and_then(|value| value.get("items"))
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str),
            Some("string")
        );
        assert_schema_nullable_string_enum(
            &mailbox_schema,
            mailbox_properties,
            "role",
            [
                "inbox",
                "all",
                "archive",
                "drafts",
                "flagged",
                "important",
                "junk",
                "sent",
                "trash",
            ],
        );
        assert_eq!(
            schema_string_property(mailbox_properties, "selectable", "type"),
            Some("boolean")
        );
        assert_eq!(
            schema_string_property(mailbox_properties, "provider_managed", "type"),
            Some("boolean")
        );

        let mut required = schema_required_properties(&mailbox_schema);
        required.sort_unstable();
        assert_eq!(
            required,
            ["attributes", "name", "provider_managed", "selectable"]
        );
    }
    fn schema_required_properties(schema: &Map<String, Value>) -> Vec<&str> {
        schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|value| {
                value
                    .as_str()
                    .expect("required property names must be strings")
            })
            .collect()
    }

    #[test]
    fn list_mailboxes_input_defaults_page_size() {
        let input: ListMailboxesInput =
            serde_json::from_str(r#"{"account_id":"default"}"#).expect("input must deserialize");
        assert_eq!(input.limit, 50);
        assert!(!input.include_counts);
        assert!(input.cursor.is_none());
    }

    #[test]
    fn search_messages_schema_matches_runtime_bounds() {
        let schema = schema_for_type::<SearchMessagesInput>();
        let properties = schema["properties"]
            .as_object()
            .expect("search schema must expose properties");

        assert_eq!(
            schema_string_property(properties, "account_id", "pattern"),
            Some("^[A-Za-z0-9_-]+$")
        );
        assert_eq!(
            schema_numeric_property(properties, "limit", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(properties, "limit", "maximum"),
            Some(100)
        );
        assert_eq!(
            schema_numeric_property(properties, "last_days", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(properties, "last_days", "maximum"),
            Some(365)
        );
        assert_eq!(
            schema_numeric_property(properties, "preview_max_chars", "minimum"),
            Some(50)
        );
        assert_eq!(
            schema_numeric_property(properties, "preview_max_chars", "maximum"),
            Some(500)
        );
        assert_eq!(
            schema_string_property(properties, "start_date", "pattern"),
            Some("^\\d{4}-\\d{2}-\\d{2}$")
        );
        assert!(!properties.contains_key("mailbox"));
        assert!(!properties.contains_key("snippet_max_chars"));
        assert!(schema_required_properties(&schema).contains(&"mailboxes"));

        let mailboxes = schema_variant_for(properties, "mailboxes").expect("mailboxes schema");
        assert_eq!(mailboxes["minItems"], 1);
        assert_eq!(mailboxes["maxItems"], 10);
        assert_eq!(mailboxes["items"]["minLength"], 1);
        assert_eq!(mailboxes["items"]["maxLength"], 256);
        for property in ["message_id_header", "gmail_query"] {
            assert_eq!(
                schema_numeric_property(properties, property, "maxLength"),
                Some(256),
                "{property} bound"
            );
        }
        assert_eq!(
            schema_numeric_property(properties, "keyword", "maxLength"),
            Some(64)
        );
        for property in ["flagged", "answered", "thread_of_message_id"] {
            assert!(properties.contains_key(property), "{property} missing");
            assert!(
                !schema_required_properties(&schema).contains(&property),
                "{property} must stay optional"
            );
        }
    }

    #[test]
    fn search_input_requires_mailboxes_and_rejects_the_old_mailbox_field() {
        let input: SearchMessagesInput = serde_json::from_str(r#"{"mailboxes":["INBOX","Sent"]}"#)
            .expect("mailboxes input must deserialize");
        assert_eq!(input.mailboxes, ["INBOX", "Sent"]);
        assert_eq!(input.limit, 10);
        assert!(input.preview_max_chars.is_none());
        serde_json::from_str::<SearchMessagesInput>(r#"{"mailbox":"INBOX"}"#)
            .expect_err("the single mailbox field was replaced");
    }

    #[test]
    fn message_outputs_publish_decimal_string_gmail_ids_and_preview() {
        for (schema, expect_preview) in [
            (schema_for_type::<MessageSummary>(), true),
            (schema_for_type::<MessageDetail>(), false),
        ] {
            let properties = schema["properties"]
                .as_object()
                .expect("output schema must expose properties");
            assert_eq!(properties.contains_key("preview"), expect_preview);
            assert!(!properties.contains_key("snippet"));
            for property in ["gmail_message_id", "gmail_thread_id"] {
                assert_eq!(
                    schema_string_property(properties, property, "type"),
                    None,
                    "{property} is a nullable type array"
                );
                assert!(
                    properties[property]["type"]
                        .as_array()
                        .is_some_and(|types| types.iter().any(|ty| ty == "string")),
                    "{property} must be a nullable string"
                );
            }
            assert!(properties.contains_key("gmail_labels"));
        }
    }

    #[test]
    fn gmail_ids_above_two_pow_53_serialize_as_exact_decimal_strings() {
        let id = (1u64 << 53) + 1;
        let summary = MessageSummary {
            message_id: "imap:default:INBOX:1:2".to_owned(),
            message_uri: String::new(),
            message_raw_uri: String::new(),
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 2,
            date: None,
            from: None,
            subject: None,
            flags: None,
            preview: None,
            gmail_labels: None,
            gmail_message_id: Some(id.to_string()),
            gmail_thread_id: Some(u64::MAX.to_string()),
        };
        let value = serde_json::to_value(&summary).expect("serialize summary");
        assert_eq!(value["gmail_message_id"], "9007199254740993");
        assert_eq!(value["gmail_thread_id"], "18446744073709551615");
    }

    #[test]
    fn message_input_schemas_publish_size_and_batch_constraints() {
        let message_schema = schema_for_type::<GetMessageInput>();
        let message_props = message_schema["properties"]
            .as_object()
            .expect("get_message schema must expose properties");
        assert_eq!(
            schema_numeric_property(message_props, "body_max_chars", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(message_props, "body_max_chars", "maximum"),
            Some(16_000)
        );
        assert_eq!(
            schema_numeric_property(message_props, "attachment_text_max_chars", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(message_props, "attachment_text_max_chars", "maximum"),
            Some(64_000)
        );
        assert!(message_props.contains_key("body_mode"));
        assert!(message_props.contains_key("attachment_mode"));

        let raw_schema = schema_for_type::<GetMessageRawInput>();
        let raw_props = raw_schema["properties"]
            .as_object()
            .expect("get_message_raw schema must expose properties");
        assert_eq!(
            schema_numeric_property(raw_props, "max_bytes", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(raw_props, "max_bytes", "maximum"),
            Some(64_000)
        );
        assert_eq!(
            schema_variant_for(raw_props, "offset_bytes")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str),
            Some("integer")
        );

        let apply_schema = schema_for_type::<ApplyToMessagesInput>();
        let apply_props = apply_schema["properties"]
            .as_object()
            .expect("apply_to_messages schema must expose properties");
        assert_eq!(
            schema_numeric_property(apply_props, "message_ids", "minItems"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(apply_props, "message_ids", "maxItems"),
            Some(250)
        );
        assert!(
            !apply_props.contains_key("dry_run"),
            "apply_to_messages schema must not publish dry_run"
        );

        let flag_schema = schema_for_type::<UpdateMessageFlagsInput>();
        let flag_props = flag_schema["properties"]
            .as_object()
            .expect("update_message_flags schema must expose properties");
        assert_eq!(
            schema_numeric_property(flag_props, "message_ids", "maxItems"),
            Some(250)
        );
        assert_eq!(
            schema_numeric_property(flag_props, "flags", "minItems"),
            Some(1)
        );
        assert!(
            !flag_props.contains_key("dry_run"),
            "update_message_flags schema must not publish dry_run"
        );

        let label_schema = schema_for_type::<UpdateMessageLabelsInput>();
        let label_props = label_schema["properties"]
            .as_object()
            .expect("update_message_labels schema must expose properties");
        assert_eq!(
            schema_numeric_property(label_props, "message_ids", "minItems"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(label_props, "message_ids", "maxItems"),
            Some(250)
        );
        assert_eq!(
            schema_numeric_property(label_props, "labels", "minItems"),
            Some(0)
        );
        assert_eq!(
            schema_numeric_property(label_props, "labels", "maxItems"),
            Some(32)
        );

        let get_operation_schema = schema_for_type::<GetOperationInput>();
        let get_operation_props = get_operation_schema["properties"]
            .as_object()
            .expect("get_operation schema must expose properties");
        assert_eq!(
            schema_string_property(get_operation_props, "include_result", "type"),
            Some("boolean")
        );
        assert!(get_operation_props.contains_key("operation_id"));
    }

    #[test]
    fn list_mailboxes_input_publishes_include_counts_and_count_fields() {
        let schema = schema_for_type::<ListMailboxesInput>();
        let properties = schema["properties"].as_object().expect("input properties");
        assert_eq!(
            schema_string_property(properties, "include_counts", "type"),
            Some("boolean")
        );
        assert!(
            !schema_required_properties(&schema).contains(&"include_counts"),
            "include_counts must remain optional"
        );
        let input: ListMailboxesInput =
            serde_json::from_str(r#"{"include_counts":true}"#).expect("input must deserialize");
        assert!(input.include_counts);
        assert_eq!(input.limit, 50);
    }

    #[test]
    fn mailbox_counts_are_skipped_when_unknown_and_kept_when_zero() {
        let mailbox = |total_messages, unread_messages| MailboxInfo {
            name: "INBOX".to_owned(),
            delimiter: None,
            attributes: Vec::new(),
            role: None,
            selectable: true,
            provider_managed: false,
            total_messages,
            unread_messages,
        };
        let unknown = serde_json::to_value(mailbox(None, None)).expect("serialize");
        assert!(unknown.get("total_messages").is_none());
        assert!(unknown.get("unread_messages").is_none());
        let zero = serde_json::to_value(mailbox(Some(0), Some(0))).expect("serialize");
        assert_eq!(zero["total_messages"], 0);
        assert_eq!(zero["unread_messages"], 0);
    }

    #[test]
    fn get_attachment_input_schema_publishes_modes_and_bounds() {
        let schema = schema_for_type::<GetAttachmentInput>();
        let properties = schema["properties"].as_object().expect("input properties");
        for field in [
            "message_id",
            "part_id",
            "mode",
            "max_chars",
            "offset_bytes",
            "max_bytes",
        ] {
            assert!(properties.contains_key(field), "{field} missing");
        }
        let mut required = schema_required_properties(&schema);
        required.sort_unstable();
        assert_eq!(required, ["message_id", "part_id"]);
        assert_eq!(
            schema_numeric_property(properties, "part_id", "maxLength"),
            Some(64)
        );
        assert_eq!(
            schema_numeric_property(properties, "max_chars", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(properties, "max_chars", "maximum"),
            Some(64_000)
        );
        assert_eq!(
            schema_numeric_property(properties, "max_bytes", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(properties, "max_bytes", "maximum"),
            Some(64_000)
        );
        let mode: GetAttachmentInput = serde_json::from_str(r#"{"message_id":"m","part_id":"1"}"#)
            .expect("input must deserialize");
        assert_eq!(mode.mode, AttachmentContentMode::Text);
        let bytes: GetAttachmentInput =
            serde_json::from_str(r#"{"message_id":"m","part_id":"1.2","mode":"bytes"}"#)
                .expect("bytes mode must deserialize");
        assert_eq!(bytes.mode, AttachmentContentMode::Bytes);
    }

    #[test]
    fn attachment_content_serializes_with_a_format_tag() {
        let text = AttachmentResult {
            filename: Some("a.csv".to_owned()),
            content_type: "text/csv".to_owned(),
            size_bytes: 3,
            content: AttachmentContent::Text {
                text: "a,b".to_owned(),
                truncated: false,
            },
        };
        let value = serde_json::to_value(&text).expect("serialize");
        assert_eq!(value["content"]["format"], "text");
        assert_eq!(value["content"]["text"], "a,b");
        let base64 = AttachmentContent::Base64 {
            content_base64: "AAE=".to_owned(),
            offset_bytes: 4,
            returned_bytes: 2,
            has_more: true,
        };
        let value = serde_json::to_value(&base64).expect("serialize");
        assert_eq!(value["format"], "base64");
        assert_eq!(value["offset_bytes"], 4);
        assert_eq!(value["returned_bytes"], 2);
        assert_eq!(value["has_more"], true);
    }

    #[test]
    fn formerly_broken_write_tool_model_schemas_are_client_safe() {
        for schema in [
            schema_for_type::<ApplyToMessagesInput>(),
            schema_for_type::<UpdateMessageFlagsInput>(),
            schema_for_type::<UpdateMessageLabelsInput>(),
            schema_for_type::<ManageMailboxInput>(),
            schema_for_type::<GetOperationInput>(),
            schema_for_type::<OperationIdInput>(),
            schema_for_type::<GetAttachmentInput>(),
        ] {
            validate_client_safe_input_schema(&Value::Object((*schema).clone()))
                .expect("write tool input schema must be client-safe");
        }
    }

    fn assert_no_nonstandard_integer_formats(value: &Value) {
        match value {
            Value::Object(object) => {
                if let Some(format) = object.get("format").and_then(Value::as_str) {
                    assert!(
                        !matches!(
                            format,
                            "uint"
                                | "uint8"
                                | "uint16"
                                | "uint32"
                                | "uint64"
                                | "uint128"
                                | "int8"
                                | "int16"
                                | "int32"
                                | "int64"
                                | "int128"
                                | "int"
                        ),
                        "published schema contains nonstandard numeric format {format}"
                    );
                }
                for value in object.values() {
                    assert_no_nonstandard_integer_formats(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    assert_no_nonstandard_integer_formats(value);
                }
            }
            _ => {}
        }
    }

    fn assert_schema_nullable_string(
        root: &Map<String, Value>,
        properties: &Map<String, Value>,
        key: &str,
    ) {
        let schema = properties
            .get(key)
            .unwrap_or_else(|| panic!("{key} schema must exist"));
        let mut types = Vec::new();
        collect_schema_types(root, schema, &mut types);
        types.sort_unstable();
        types.dedup();
        assert_eq!(
            types,
            ["null", "string"],
            "{key} must allow only string and null values"
        );
    }
    fn assert_schema_nullable_string_enum<const N: usize>(
        root: &Map<String, Value>,
        properties: &Map<String, Value>,
        key: &str,
        expected_values: [&str; N],
    ) {
        assert_schema_nullable_string(root, properties, key);
        let schema = properties
            .get(key)
            .unwrap_or_else(|| panic!("{key} schema must exist"));
        let mut values = Vec::new();
        collect_schema_string_enum_values(root, schema, &mut values);
        values.sort_unstable();
        values.dedup();
        let mut expected_values = expected_values;
        expected_values.sort_unstable();
        assert_eq!(values, expected_values, "{key} enum values must match");
    }
    fn collect_schema_types<'a>(
        root: &'a Map<String, Value>,
        schema: &'a Value,
        types: &mut Vec<&'a str>,
    ) {
        if let Some(resolved) = resolve_local_schema_ref(root, schema) {
            collect_schema_types(root, resolved, types);
            return;
        }
        match schema.get("type") {
            Some(Value::String(ty)) => types.push(ty),
            Some(Value::Array(schema_types)) => {
                types.extend(schema_types.iter().filter_map(Value::as_str));
            }
            _ => {}
        }
        if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
            for variant in variants {
                collect_schema_types(root, variant, types);
            }
        }
    }
    fn collect_schema_string_enum_values<'a>(
        root: &'a Map<String, Value>,
        schema: &'a Value,
        values: &mut Vec<&'a str>,
    ) {
        if let Some(resolved) = resolve_local_schema_ref(root, schema) {
            collect_schema_string_enum_values(root, resolved, values);
            return;
        }
        if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
            values.extend(enum_values.iter().filter_map(Value::as_str));
        }
        if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
            for variant in variants {
                collect_schema_string_enum_values(root, variant, values);
            }
        }
    }
    fn resolve_local_schema_ref<'a>(
        root: &'a Map<String, Value>,
        schema: &Value,
    ) -> Option<&'a Value> {
        let reference = schema.get("$ref")?.as_str()?.strip_prefix("#/")?;
        let mut segments = reference.split('/');
        let first = segments.next()?.replace("~1", "/").replace("~0", "~");
        let mut resolved = root.get(&first)?;
        for segment in segments {
            let segment = segment.replace("~1", "/").replace("~0", "~");
            resolved = resolved.get(&segment)?;
        }
        Some(resolved)
    }

    fn schema_numeric_property(
        properties: &Map<String, Value>,
        key: &str,
        field: &str,
    ) -> Option<u64> {
        schema_variant_for(properties, key)?.get(field)?.as_u64()
    }

    fn schema_string_property<'a>(
        properties: &'a Map<String, Value>,
        key: &str,
        field: &str,
    ) -> Option<&'a str> {
        schema_variant_for(properties, key)?.get(field)?.as_str()
    }

    fn schema_variant_for<'a>(properties: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
        let schema = properties.get(key)?;
        if schema.get("type").is_some_and(is_non_null_type) {
            return Some(schema);
        }
        schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|variant| variant.get("type").is_some_and(is_non_null_type))
    }

    fn is_non_null_type(value: &Value) -> bool {
        match value {
            Value::String(ty) => ty != "null",
            Value::Array(types) => types
                .iter()
                .filter_map(Value::as_str)
                .any(|ty| ty != "null"),
            _ => false,
        }
    }
}
