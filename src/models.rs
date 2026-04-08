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
    /// Optional subject snippet (if `include_snippet=true`)
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
    /// Include subject snippet in results
    #[serde(default)]
    pub include_snippet: bool,
    /// Maximum snippet length (50..500, requires `include_snippet=true`)
    #[schemars(range(min = 50, max = 500), transform = remove_format)]
    pub snippet_max_chars: Option<usize>,
}

/// Input: get parsed message details
///
/// Used by `imap_get_message`. Supports bounded enrichment (char limits,
/// optional HTML, optional attachment text extraction).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMessageInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Stable message identifier (format: `imap:{account}:{mailbox}:{uidvalidity}:{uid}`)
    pub message_id: String,
    /// Maximum body characters (100..20000, default 2000)
    #[serde(default = "default_body_max_chars")]
    #[schemars(range(min = 100, max = 20_000), transform = remove_format)]
    pub body_max_chars: usize,
    /// Include headers in response
    #[serde(default = "default_true")]
    pub include_headers: bool,
    /// Include all headers (if `true`, overrides curated header list)
    #[serde(default)]
    pub include_all_headers: bool,
    /// Include sanitized HTML body
    #[serde(default)]
    pub include_html: bool,
    /// Extract text from PDF attachments
    #[serde(default)]
    pub extract_attachment_text: bool,
    /// Maximum attachment text length (100..50000, requires `extract_attachment_text=true`)
    #[schemars(range(min = 100, max = 50_000), transform = remove_format)]
    pub attachment_text_max_chars: Option<usize>,
}

/// Input: get raw RFC822 message source
///
/// Used by `imap_get_message_raw`. Returns bounded message bytes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMessageRawInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Stable message identifier
    pub message_id: String,
    /// Maximum message bytes to return (1024..1000000, default 200000)
    #[serde(default = "default_raw_max_bytes")]
    #[schemars(range(min = 1_024, max = 1_000_000), transform = remove_format)]
    pub max_bytes: usize,
}

/// Input: apply a bulk action to selected messages.
///
/// Used by `imap_apply_to_messages`. Message selection may be explicit by
/// `message_ids` or derived from mailbox search criteria.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ApplyToMessagesInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Message selector (either explicit ids or a mailbox search)
    pub selector: MessageSelectorInput,
    /// Action to apply to the selected messages
    #[serde(flatten)]
    pub action: MessageActionInput,
    /// Maximum allowed matched messages
    #[serde(default = "default_max_messages")]
    #[schemars(range(min = 1, max = 1_000), transform = remove_format)]
    pub max_messages: usize,
    /// When true, validate and preview without mutating messages
    #[serde(default)]
    pub dry_run: bool,
}

/// Selects target messages by explicit ids or by mailbox search criteria.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MessageSelectorInput {
    /// Stable message identifiers to target
    #[schemars(length(min = 1, max = 1_000))]
    pub message_ids: Option<Vec<String>>,
    /// Mailbox search criteria
    pub search: Option<SearchSelectorInput>,
}

/// Search criteria used by `imap_apply_to_messages`.
///
/// Mirrors `imap_search_messages` without pagination limit or snippet options.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchSelectorInput {
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
}

/// Action union for `imap_apply_to_messages`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MessageActionInput {
    /// Move messages within the same account
    Move {
        /// Destination mailbox
        #[schemars(length(min = 1, max = 256))]
        destination_mailbox: String,
    },
    /// Copy messages to another mailbox
    Copy {
        /// Destination mailbox
        #[schemars(length(min = 1, max = 256))]
        destination_mailbox: String,
        /// Destination account (if omitted, copies within same account)
        #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
        destination_account_id: Option<String>,
    },
    /// Delete messages from their source mailbox
    Delete,
    /// Add and/or remove flags on messages
    UpdateFlags {
        /// Flags to add (e.g., `\Seen`, `\Flagged`, `Important`)
        add_flags: Option<Vec<String>>,
        /// Flags to remove
        remove_flags: Option<Vec<String>>,
    },
}

/// Input: create, rename, or delete a mailbox.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ManageMailboxInput {
    /// Account identifier (defaults to `"default"`)
    #[serde(default = "default_account_id")]
    #[schemars(length(min = 1, max = 64), pattern(r"^[A-Za-z0-9_-]+$"))]
    pub account_id: String,
    /// Mailbox management action
    #[serde(flatten)]
    pub action: MailboxAction,
}

/// Mailbox management action kind.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MailboxAction {
    /// Create a mailbox
    Create {
        /// Source or target mailbox name, depending on action
        #[schemars(length(min = 1, max = 256))]
        mailbox: String,
    },
    /// Rename or move a mailbox
    Rename {
        /// Existing mailbox name
        #[schemars(length(min = 1, max = 256))]
        mailbox: String,
        /// Destination mailbox name
        #[schemars(length(min = 1, max = 256))]
        destination_mailbox: String,
    },
    /// Delete a mailbox
    Delete {
        /// Mailbox name
        #[schemars(length(min = 1, max = 256))]
        mailbox: String,
    },
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

/// Default value for `max_bytes` in get_message_raw
///
/// Large enough to capture full message headers and body for most messages,
/// but bounded to prevent excessive output. 200KB is a practical limit.
fn default_raw_max_bytes() -> usize {
    200_000
}

/// Default value for maximum messages allowed in bulk selection.
fn default_max_messages() -> usize {
    100
}

#[cfg(test)]
mod tests {
    use rmcp::handler::server::common::schema_for_type;
    use serde_json::{Map, Value};

    use super::{
        AccountOnlyInput, ApplyToMessagesInput, GetMessageInput, GetMessageRawInput,
        ManageMailboxInput, SearchMessagesInput,
    };

    #[test]
    fn input_schemas_do_not_publish_nonstandard_unsigned_formats() {
        for schema in [
            schema_for_type::<AccountOnlyInput>(),
            schema_for_type::<SearchMessagesInput>(),
            schema_for_type::<GetMessageInput>(),
            schema_for_type::<GetMessageRawInput>(),
            schema_for_type::<ApplyToMessagesInput>(),
            schema_for_type::<ManageMailboxInput>(),
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
    fn message_input_schemas_publish_account_and_size_constraints() {
        let message_schema = schema_for_type::<GetMessageInput>();
        let message_props = message_schema["properties"]
            .as_object()
            .expect("get_message schema must expose properties");
        assert_eq!(
            schema_string_property(message_props, "account_id", "pattern"),
            Some("^[A-Za-z0-9_-]+$")
        );
        assert_eq!(
            schema_numeric_property(message_props, "body_max_chars", "minimum"),
            Some(100)
        );
        assert_eq!(
            schema_numeric_property(message_props, "body_max_chars", "maximum"),
            Some(20_000)
        );
        assert_eq!(
            schema_numeric_property(message_props, "attachment_text_max_chars", "minimum"),
            Some(100)
        );
        assert_eq!(
            schema_numeric_property(message_props, "attachment_text_max_chars", "maximum"),
            Some(50_000)
        );

        let raw_schema = schema_for_type::<GetMessageRawInput>();
        let raw_props = raw_schema["properties"]
            .as_object()
            .expect("get_message_raw schema must expose properties");
        assert_eq!(
            schema_numeric_property(raw_props, "max_bytes", "minimum"),
            Some(1_024)
        );
        assert_eq!(
            schema_numeric_property(raw_props, "max_bytes", "maximum"),
            Some(1_000_000)
        );

        let apply_schema = schema_for_type::<ApplyToMessagesInput>();
        let apply_props = apply_schema["properties"]
            .as_object()
            .expect("apply_to_messages schema must expose properties");
        assert_eq!(
            schema_numeric_property(apply_props, "max_messages", "minimum"),
            Some(1)
        );
        assert_eq!(
            schema_numeric_property(apply_props, "max_messages", "maximum"),
            Some(1_000)
        );
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
