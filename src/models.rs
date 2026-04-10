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

/// Mailbox/folder metadata
///
/// Returned by `imap_list_mailboxes`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MailboxInfo {
    /// Mailbox name (may contain path separators like `/` or `.`)
    pub name: String,
    /// Hierarchy delimiter if supported by server (e.g., `/`, `.`)
    pub delimiter: Option<String>,
}

/// Message summary for search results
///
/// Lightweight representation returned by `imap_search_messages`. Includes
/// optional snippet for preview.
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
    /// Optional subject snippet (present when `snippet_max_chars` was requested)
    pub snippet: Option<String>,
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
    /// Attachment size in bytes
    pub size_bytes: usize,
    /// Part ID for MIME structure (e.g., `1`, `2`, `3.1`)
    pub part_id: String,
    /// Extracted text from PDF (if enabled and extraction succeeded)
    pub extracted_text: Option<String>,
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
    /// All headers or curated subset (if `include_headers=true`)
    pub headers: Option<Vec<(String, String)>>,
    /// Plain text body (truncated to `body_max_chars`)
    pub body_text: Option<String>,
    /// Sanitized HTML body (if `include_html=true`, truncated)
    pub body_html: Option<String>,
    /// Attachment metadata (up to `MAX_ATTACHMENTS`)
    pub attachments: Option<Vec<AttachmentInfo>>,
}

/// Input: account_id only
///
/// Used by `imap_list_accounts` and `imap_list_mailboxes`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AccountOnlyInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
}

/// Input: search messages with pagination
///
/// Used by `imap_search_messages`. Supports multiple search criteria and
/// cursor-based pagination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchMessagesInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Mailbox to search (e.g., `INBOX`, `Sent`, `Archive`)
    #[schemars(length(min = 1, max = 256))]
    pub mailbox: String,
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
    /// Filter to messages from last N days
    #[schemars(range(min = 1, max = 365), transform = remove_format)]
    pub last_days: Option<u16>,
    /// Filter to messages on or after this date (YYYY-MM-DD)
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub start_date: Option<String>,
    /// Filter to messages before this date (YYYY-MM-DD)
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub end_date: Option<String>,
    /// Maximum messages to return (1..50, default 10)
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 50), transform = remove_format)]
    pub limit: usize,
    /// Maximum snippet length (50..500). When omitted, snippets are not included.
    #[schemars(range(min = 50, max = 500), transform = remove_format)]
    pub snippet_max_chars: Option<usize>,
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
    /// Flag mutation mode
    #[schemars(schema_with = "flag_operation_schema")]
    pub operation: String,
    /// Standard IMAP system flags or server-defined keywords
    #[schemars(length(min = 1, max = 32))]
    pub flags: Vec<String>,
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

/// Input: fetch or cancel a previously started write operation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OperationIdInput {
    /// Opaque operation identifier returned by a write tool.
    #[schemars(length(min = 1, max = 64))]
    pub operation_id: String,
}

/// Default value for `account_id` field
pub fn default_account_id() -> String {
    "default".to_owned()
}

/// Default value for `bool` fields (true)
fn default_true() -> bool {
    true
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

fn flag_operation_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["add", "remove", "replace"]
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
        AccountOnlyInput, ApplyToMessagesInput, GetMessageInput, GetMessageRawInput,
        ManageMailboxInput, OperationIdInput, SearchMessagesInput, UpdateMessageFlagsInput,
        validate_client_safe_input_schema,
    };

    #[test]
    fn input_schemas_do_not_publish_nonstandard_unsigned_formats() {
        for schema in [
            schema_for_type::<AccountOnlyInput>(),
            schema_for_type::<SearchMessagesInput>(),
            schema_for_type::<GetMessageInput>(),
            schema_for_type::<GetMessageRawInput>(),
            schema_for_type::<ApplyToMessagesInput>(),
            schema_for_type::<UpdateMessageFlagsInput>(),
            schema_for_type::<ManageMailboxInput>(),
            schema_for_type::<OperationIdInput>(),
        ] {
            assert_no_nonstandard_integer_formats(&Value::Object((*schema).clone()));
        }
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
            Some(50)
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
            schema_numeric_property(properties, "snippet_max_chars", "minimum"),
            Some(50)
        );
        assert_eq!(
            schema_numeric_property(properties, "snippet_max_chars", "maximum"),
            Some(500)
        );
        assert_eq!(
            schema_string_property(properties, "start_date", "pattern"),
            Some("^\\d{4}-\\d{2}-\\d{2}$")
        );
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
    }

    #[test]
    fn formerly_broken_write_tool_model_schemas_are_client_safe() {
        for schema in [
            schema_for_type::<ApplyToMessagesInput>(),
            schema_for_type::<UpdateMessageFlagsInput>(),
            schema_for_type::<ManageMailboxInput>(),
            schema_for_type::<OperationIdInput>(),
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
