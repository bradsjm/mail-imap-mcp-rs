//! Message parsing and MIME handling
//!
//! Parses RFC822 messages using `mailparse`, extracts body text/HTML,
//! and handles attachments. Sanitizes HTML, derives fallback text from HTML,
//! and supports optional PDF text extraction.

use std::collections::BTreeMap;

use base64::Engine;
use mailparse::body::Body;
use mailparse::{DispositionType, MailHeader, ParsedMail};

use crate::errors::{AppError, AppResult};
use crate::models::{AttachmentInfo, AttachmentMode, BodyMode};

/// Maximum attachments collected during MIME parsing.
pub const MAX_ATTACHMENTS: usize = 50;

/// Default MIME part ceiling used by unit-test fixtures.
#[cfg(test)]
pub const MIME_MAX_PARTS: usize = 250;

/// Resource ceilings for bounded raw-message MIME parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessingLimits {
    pub decode_budget_bytes: usize,
    pub max_depth: usize,
    pub max_parts: usize,
    pub attachment_extract_budget_bytes: usize,
    pub attachment_text_max_chars: usize,
}

/// A resource ceiling reached while producing a partial parsed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingLimit {
    DecodeBudgetBytes,
    MaxDepth,
    MaxParts,
    AttachmentExtractBudgetBytes,
}

/// Parsed message representation
///
/// Contains extracted headers, body content, and attachment metadata.
/// Bodies are truncated by caller to configured limits.
#[derive(Debug, Clone)]
pub struct ParsedMessage {
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
    /// All headers as key-value pairs
    pub headers_all: Vec<(String, String)>,
    /// Plain text body (untruncated)
    pub body_text: Option<String>,
    /// Sanitized HTML body (untruncated)
    pub body_html_sanitized: Option<String>,
    /// Attachment metadata
    pub attachments: Vec<AttachmentInfo>,
    /// Whether attachment collection exceeded `MAX_ATTACHMENTS`
    pub attachments_truncated: bool,
    /// Resource ceilings reached while producing this partial result.
    pub processing_limits: Vec<ProcessingLimit>,
}

struct WalkConfig {
    attachment_mode: AttachmentMode,
    include_html: bool,
    limits: ProcessingLimits,
}

struct WalkState {
    body_text: Option<String>,
    body_html: Option<String>,
    attachments: Vec<AttachmentInfo>,
    attachments_truncated: bool,
    processing_limits: Vec<ProcessingLimit>,
    decoded_bytes: usize,
    attachment_decoded_bytes: usize,
    parts_seen: usize,
}

impl WalkState {
    fn hit(&mut self, limit: ProcessingLimit) {
        if !self.processing_limits.contains(&limit) {
            self.processing_limits.push(limit);
        }
    }
}

/// Parse RFC822 message into structured representation
///
/// Extracts headers, body text/HTML, and attachment info. Sanitizes
/// HTML and optionally extracts text from PDF attachments.
///
/// Resource ceilings are reported in [`ParsedMessage::processing_limits`];
/// reaching one returns the useful partial result rather than failing parsing.
pub fn parse_message(
    raw: &[u8],
    body_max_chars: usize,
    body_mode: BodyMode,
    attachment_mode: AttachmentMode,
    limits: &ProcessingLimits,
) -> AppResult<ParsedMessage> {
    let headers = parse_all_headers(raw)?;
    let preflight_limits = preflight_raw_mime(raw, limits.max_depth, limits.max_parts)?;
    if !preflight_limits.is_empty() {
        return Ok(header_only_message(headers, preflight_limits));
    }

    let parsed = mailparse::parse_mail(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse RFC822 message: {e}")))?;
    let mut state = WalkState {
        body_text: None,
        body_html: None,
        attachments: Vec::new(),
        attachments_truncated: false,
        processing_limits: Vec::new(),
        decoded_bytes: 0,
        attachment_decoded_bytes: 0,
        parts_seen: 0,
    };
    let config = WalkConfig {
        attachment_mode,
        include_html: matches!(body_mode, BodyMode::Html | BodyMode::Both),
        limits: *limits,
    };

    walk_parts(&parsed, "1".to_owned(), 0, &mut state, &config)?;

    let text = if matches!(body_mode, BodyMode::Text | BodyMode::Both) {
        select_body_text(state.body_text, state.body_html.as_deref())
            .map(|text| truncate_chars(text, body_max_chars))
    } else {
        None
    };
    let html = if config.include_html {
        state
            .body_html
            .map(|html| truncate_chars(html, body_max_chars))
    } else {
        None
    };

    let header_map = to_header_map(&headers);
    Ok(ParsedMessage {
        date: header_map.get("date").cloned(),
        from: header_map.get("from").cloned(),
        to: header_map.get("to").cloned(),
        cc: header_map.get("cc").cloned(),
        subject: header_map.get("subject").cloned(),
        headers_all: headers,
        body_text: text,
        body_html_sanitized: html,
        attachments: state.attachments,
        attachments_truncated: state.attachments_truncated,
        processing_limits: state.processing_limits,
    })
}
fn header_only_message(
    headers: Vec<(String, String)>,
    processing_limits: Vec<ProcessingLimit>,
) -> ParsedMessage {
    let header_map = to_header_map(&headers);
    ParsedMessage {
        date: header_map.get("date").cloned(),
        from: header_map.get("from").cloned(),
        to: header_map.get("to").cloned(),
        cc: header_map.get("cc").cloned(),
        subject: header_map.get("subject").cloned(),
        headers_all: headers,
        body_text: None,
        body_html_sanitized: None,
        attachments: Vec::new(),
        attachments_truncated: true,
        processing_limits,
    }
}

fn preflight_raw_mime(
    raw: &[u8],
    max_depth: usize,
    max_parts: usize,
) -> AppResult<Vec<ProcessingLimit>> {
    if max_parts == 0 {
        return Ok(vec![ProcessingLimit::MaxParts]);
    }

    let stack_limit = max_parts.saturating_add(1);
    let mut stack = Vec::with_capacity(stack_limit.min(64));
    stack.push((raw, 0usize));
    let mut parts_seen = 0usize;
    let mut limits = Vec::new();

    while let Some((part, depth)) = stack.pop() {
        if depth > max_depth {
            push_limit(&mut limits, ProcessingLimit::MaxDepth);
            continue;
        }
        if parts_seen >= max_parts {
            push_limit(&mut limits, ProcessingLimit::MaxParts);
            break;
        }
        parts_seen += 1;

        let (headers, body_offset) = mailparse::parse_headers(part)
            .map_err(|e| AppError::Internal(format!("failed to parse message headers: {e}")))?;
        let content_type = headers
            .iter()
            .find(|header| header.get_key_ref().eq_ignore_ascii_case("content-type"))
            .map_or_else(
                || mailparse::parse_content_type("text/plain"),
                |header| mailparse::parse_content_type(&header.get_value()),
            );
        if !content_type
            .mimetype
            .to_ascii_lowercase()
            .starts_with("multipart/")
        {
            continue;
        }
        let Some(boundary) = content_type.params.get("boundary") else {
            continue;
        };

        let child_depth = depth.saturating_add(1);
        if child_depth > max_depth {
            let children = multipart_child_slices(&part[body_offset..], boundary.as_bytes(), 1);
            if !children.parts.is_empty() || children.overflowed {
                push_limit(&mut limits, ProcessingLimit::MaxDepth);
            }
            continue;
        }

        let remaining = max_parts.saturating_sub(parts_seen.saturating_add(stack.len()));
        let children = multipart_child_slices(&part[body_offset..], boundary.as_bytes(), remaining);
        if children.overflowed {
            push_limit(&mut limits, ProcessingLimit::MaxParts);
            return Ok(limits);
        }
        for child in children.parts.into_iter().rev() {
            if stack.len() >= stack_limit {
                push_limit(&mut limits, ProcessingLimit::MaxParts);
                break;
            }
            stack.push((child, child_depth));
        }
    }

    Ok(limits)
}

struct MultipartChildren<'a> {
    parts: Vec<&'a [u8]>,
    overflowed: bool,
}

fn multipart_child_slices<'a>(
    body: &'a [u8],
    boundary: &[u8],
    capacity: usize,
) -> MultipartChildren<'a> {
    let mut marker = Vec::with_capacity(boundary.len().saturating_add(2));
    marker.extend_from_slice(b"--");
    marker.extend_from_slice(boundary);

    let mut parts = Vec::with_capacity(capacity.min(16));
    let mut child_start = None;
    let mut line_start = 0usize;
    let mut overflowed = false;
    let mut closed = false;

    while line_start < body.len() {
        let line_end = body[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(body.len(), |offset| line_start + offset + 1);
        let line = &body[line_start..line_end];
        if line.starts_with(&marker) {
            let terminates_child = child_start.is_some();
            if let Some(start) = child_start.take() {
                let mut end = line_start;
                if end > start && body[end - 1] == b'\n' {
                    end -= 1;
                    if end > start && body[end - 1] == b'\r' {
                        end -= 1;
                    }
                }
                if parts.len() >= capacity {
                    overflowed = true;
                    break;
                }
                parts.push(&body[start..end]);
            }

            if terminates_child && line[marker.len()..].starts_with(b"--") {
                closed = true;
                break;
            }
            if line.ends_with(b"\n") {
                child_start = Some(line_end);
            } else {
                break;
            }
        }
        line_start = line_end;
    }

    if !closed
        && !overflowed
        && let Some(start) = child_start
    {
        if parts.len() >= capacity {
            overflowed = true;
        } else {
            parts.push(&body[start..]);
        }
    }

    MultipartChildren { parts, overflowed }
}

fn push_limit(limits: &mut Vec<ProcessingLimit>, limit: ProcessingLimit) {
    if !limits.contains(&limit) {
        limits.push(limit);
    }
}

fn decode_parsed_part_bounded(
    part: &ParsedMail<'_>,
    remaining: usize,
) -> AppResult<(Vec<u8>, bool)> {
    match part.get_body_encoded() {
        Body::Base64(body) => decode_transfer_encoded_bounded(body.get_raw(), "base64", remaining),
        Body::QuotedPrintable(body) => {
            decode_transfer_encoded_bounded(body.get_raw(), "quoted-printable", remaining)
        }
        Body::SevenBit(body) | Body::EightBit(body) => {
            decode_transfer_encoded_bounded(body.get_raw(), "8bit", remaining)
        }
        Body::Binary(body) => decode_transfer_encoded_bounded(body.get_raw(), "binary", remaining),
    }
}

fn decode_transfer_encoded_bounded(
    data: &[u8],
    encoding: &str,
    remaining: usize,
) -> AppResult<(Vec<u8>, bool)> {
    let encoding = encoding.trim();
    if encoding.eq_ignore_ascii_case("base64") {
        return decode_base64_bounded(data, remaining);
    }
    if encoding.eq_ignore_ascii_case("quoted-printable") {
        return decode_quoted_printable_bounded(data, remaining);
    }
    let taken = data.len().min(remaining);
    Ok((data[..taken].to_vec(), taken < data.len()))
}

fn decode_base64_bounded(data: &[u8], remaining: usize) -> AppResult<(Vec<u8>, bool)> {
    let mut encoded = Vec::with_capacity(4);
    let mut decoded = Vec::with_capacity(remaining.min(data.len()));
    let mut partial = false;
    for byte in data
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        if decoded.len() == remaining {
            partial = true;
            break;
        }
        encoded.push(byte);
        if encoded.len() == 4 {
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .map_err(|error| {
                    AppError::Internal(format!("failed decoding base64 body section: {error}"))
                })?;
            if chunk.len() > remaining.saturating_sub(decoded.len()) {
                partial = true;
                break;
            }
            decoded.extend_from_slice(&chunk);
            encoded.clear();
        }
    }
    if !encoded.is_empty() {
        partial = true;
    }
    Ok((decoded, partial))
}

fn decode_quoted_printable_bounded(data: &[u8], remaining: usize) -> AppResult<(Vec<u8>, bool)> {
    let mut decoded = Vec::with_capacity(remaining.min(data.len()));
    let mut index = 0;
    while index < data.len() {
        if data[index] != b'=' {
            if decoded.len() == remaining {
                return Ok((decoded, true));
            }
            decoded.push(data[index]);
            index += 1;
            continue;
        }
        match data.get(index + 1..) {
            Some([b'\r', b'\n', ..]) => index += 3,
            Some([b'\n', ..]) => index += 2,
            Some([high, low, ..]) => {
                if decoded.len() == remaining {
                    return Ok((decoded, true));
                }
                let high = hex_value(*high).ok_or_else(|| {
                    AppError::Internal("invalid quoted-printable body section".to_owned())
                })?;
                let low = hex_value(*low).ok_or_else(|| {
                    AppError::Internal("invalid quoted-printable body section".to_owned())
                })?;
                decoded.push(high << 4 | low);
                index += 3;
            }
            _ => return Ok((decoded, true)),
        }
    }
    Ok((decoded, false))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Choose the message text body, preferring a meaningful text/plain part.
///
/// Falls back to text derived from sanitized HTML when plain text is missing
/// or only contains whitespace.
fn select_body_text(body_text: Option<String>, body_html: Option<&str>) -> Option<String> {
    if let Some(text) = body_text.filter(|text| has_meaningful_content(text)) {
        return Some(text);
    }

    body_html
        .and_then(html_to_text)
        .filter(|text| has_meaningful_content(text))
}

/// Convert sanitized HTML to plain text without artificial wrapping.
fn html_to_text(html: &str) -> Option<String> {
    let text = html2text::config::plain_no_decorate()
        .string_from_read(html.as_bytes(), usize::MAX)
        .ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Return true when the body contains non-whitespace characters.
fn has_meaningful_content(body: &str) -> bool {
    body.chars().any(|ch| !ch.is_whitespace())
}

/// Walk MIME part tree recursively
///
/// Traverses all MIME parts to extract text/plain, text/html bodies,
/// and attachment metadata. Handles multipart structures correctly.
fn walk_parts(
    part: &ParsedMail<'_>,
    part_id: String,
    depth: usize,
    state: &mut WalkState,
    config: &WalkConfig,
) -> AppResult<()> {
    if state.parts_seen >= config.limits.max_parts {
        state.hit(ProcessingLimit::MaxParts);
        return Ok(());
    }
    if depth > config.limits.max_depth {
        state.hit(ProcessingLimit::MaxDepth);
        return Ok(());
    }
    state.parts_seen += 1;

    if part.subparts.is_empty() {
        let ctype = part.ctype.mimetype.to_ascii_lowercase();
        let disp = part.get_content_disposition();
        let filename = attachment_filename(part, &disp.params);
        let explicit_attachment = disp.disposition == DispositionType::Attachment;
        let is_attachment = explicit_attachment || filename.is_some();
        let body_candidate = !is_attachment && matches!(ctype.as_str(), "text/plain" | "text/html");

        if body_candidate {
            if ctype == "text/plain" && state.body_text.is_none() {
                decode_text_part(part, false, state, config)?;
            }
            if ctype == "text/html" && state.body_html.is_none() {
                decode_text_part(part, true, state, config)?;
            }
        }

        if is_attachment && config.attachment_mode != AttachmentMode::None {
            if state.attachments.len() >= MAX_ATTACHMENTS {
                state.attachments_truncated = true;
                return Ok(());
            }
            let decode_remaining = config
                .limits
                .decode_budget_bytes
                .saturating_sub(state.decoded_bytes);
            let (decoded, partial) = decode_parsed_part_bounded(part, decode_remaining)?;
            state.decoded_bytes += decoded.len();
            let mut size_bytes = None;
            let mut extracted_text = None;
            if partial {
                state.hit(ProcessingLimit::DecodeBudgetBytes);
            } else {
                size_bytes = Some(decoded.len());
                let extract_pdf = config.attachment_mode == AttachmentMode::ExtractText
                    && ctype == "application/pdf";
                if extract_pdf {
                    let attachment_remaining = config
                        .limits
                        .attachment_extract_budget_bytes
                        .saturating_sub(state.attachment_decoded_bytes);
                    if decoded.len() > attachment_remaining {
                        state.hit(ProcessingLimit::AttachmentExtractBudgetBytes);
                    } else {
                        state.attachment_decoded_bytes += decoded.len();
                        if let Ok(text) = pdf_extract::extract_text_from_mem(&decoded) {
                            extracted_text = Some(truncate_chars(
                                text,
                                config.limits.attachment_text_max_chars,
                            ));
                        }
                    }
                }
            }
            state.attachments.push(AttachmentInfo {
                filename,
                content_type: ctype,
                size_bytes,
                part_id,
                extracted_text,
            });
        }
        return Ok(());
    }

    for (idx, sub) in part.subparts.iter().enumerate() {
        let next_id = format!("{part_id}.{}", idx + 1);
        walk_parts(sub, next_id, depth.saturating_add(1), state, config)?;
    }
    Ok(())
}

fn decode_text_part(
    part: &ParsedMail<'_>,
    is_html: bool,
    state: &mut WalkState,
    config: &WalkConfig,
) -> AppResult<()> {
    let Ok(decoded) = part.get_body() else {
        return Ok(());
    };
    if decoded.len()
        > config
            .limits
            .decode_budget_bytes
            .saturating_sub(state.decoded_bytes)
    {
        state.hit(ProcessingLimit::DecodeBudgetBytes);
        return Ok(());
    }
    state.decoded_bytes += decoded.len();
    if is_html {
        state.body_html = Some(ammonia::clean(&decoded));
    } else {
        state.body_text = Some(decoded);
    }
    Ok(())
}

#[cfg(test)]
fn attachment_size_bytes(part: &ParsedMail<'_>) -> usize {
    match part.get_body_encoded() {
        Body::Base64(body) | Body::QuotedPrintable(body) => body
            .get_decoded()
            .map_or_else(|_| body.get_raw().len(), |decoded| decoded.len()),
        Body::SevenBit(body) | Body::EightBit(body) => body.get_raw().len(),
        Body::Binary(body) => body.get_raw().len(),
    }
}

/// Extract attachment filename from part
///
/// Checks Content-Disposition parameter first, falls back to Content-Type
/// name parameter.
fn attachment_filename(
    part: &ParsedMail<'_>,
    disp_params: &BTreeMap<String, String>,
) -> Option<String> {
    disp_params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned())
}

/// Return headers, either curated or all
///
/// If `include_all=true`, returns all headers. Otherwise, returns only
/// a safe subset (Date, From, To, Cc, Subject, Message-ID).
pub fn curated_headers(headers: &[(String, String)], include_all: bool) -> Vec<(String, String)> {
    if include_all {
        return headers.to_vec();
    }

    let allowed = ["date", "from", "to", "cc", "subject", "message-id"];
    headers
        .iter()
        .filter(|(k, _)| allowed.contains(&k.to_ascii_lowercase().as_str()))
        .cloned()
        .collect()
}

/// Parse header bytes into key-value pairs
pub fn parse_header_bytes(header_bytes: &[u8]) -> AppResult<Vec<(String, String)>> {
    let (headers, _) = mailparse::parse_headers(header_bytes)
        .map_err(|e| AppError::Internal(format!("failed to parse message headers: {e}")))?;
    Ok(to_tuples(headers))
}

/// Parse all headers from raw message
fn parse_all_headers(raw: &[u8]) -> AppResult<Vec<(String, String)>> {
    let (headers, _) = mailparse::parse_headers(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse message headers: {e}")))?;
    Ok(to_tuples(headers))
}

/// Convert mailparse headers to key-value tuples
///
/// Extracts header keys and values using mailparse's `get_key()` and `get_value()`
/// methods, which handle encoding and whitespace normalization.
fn to_tuples(headers: Vec<MailHeader<'_>>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|h| (h.get_key(), h.get_value()))
        .collect()
}

/// Convert header tuples to case-insensitive map
///
/// Returns the first value for each header key (case-insensitive). If a header
/// appears multiple times, only the first value is retained. Keys are normalized
/// to lowercase for case-insensitive lookup.
fn to_header_map(headers: &[(String, String)]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (k, v) in headers {
        let key = k.to_ascii_lowercase();
        map.entry(key).or_insert_with(|| v.clone());
    }
    map
}

/// Truncate string to maximum characters (Unicode-aware)
///
/// Preserves complete characters, never splitting multi-byte sequences.
pub fn truncate_chars(input: String, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {

    use super::{
        MAX_ATTACHMENTS, MIME_MAX_PARTS, ProcessingLimit, ProcessingLimits, attachment_size_bytes,
        curated_headers, parse_message, truncate_chars,
    };
    use crate::models::{AttachmentMode, BodyMode};

    fn test_limits(attachment_text_max_chars: usize) -> ProcessingLimits {
        ProcessingLimits {
            decode_budget_bytes: 10_000_000,
            max_depth: 64,
            max_parts: MIME_MAX_PARTS,
            attachment_extract_budget_bytes: 5_000_000,
            attachment_text_max_chars,
        }
    }

    /// Tests that Unicode strings are truncated by character, not byte.
    #[test]
    fn truncates_unicode_by_character() {
        let input = "a😀b😀c".to_owned();
        let out = truncate_chars(input, 4);
        assert_eq!(out, "a😀b😀");
    }

    /// Tests that `curated_headers` filters headers unless `include_all` is true.
    #[test]
    fn curated_headers_filters_unless_include_all() {
        let headers = vec![
            (
                "Date".to_owned(),
                "Wed, 1 Jan 2025 00:00:00 +0000".to_owned(),
            ),
            ("From".to_owned(), "sender@example.com".to_owned()),
            ("X-Custom".to_owned(), "value".to_owned()),
        ];

        let curated = curated_headers(&headers, false);
        assert_eq!(curated.len(), 2);
        assert!(curated.iter().any(|(k, _)| k.eq_ignore_ascii_case("date")));
        assert!(curated.iter().any(|(k, _)| k.eq_ignore_ascii_case("from")));

        let all = curated_headers(&headers, true);
        assert_eq!(all.len(), 3);
    }

    /// Tests parsing of a simple plain text message and verifies header and body extraction.
    #[test]
    fn parses_simple_plain_text_message() {
        let raw = b"From: sender@example.com\r\nTo: user@example.com\r\nSubject: Hi\r\nDate: Wed, 1 Jan 2025 00:00:00 +0000\r\n\r\nHello there";
        let parsed = parse_message(
            raw,
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(10000),
        )
        .expect("parse should succeed");

        assert_eq!(parsed.subject.as_deref(), Some("Hi"));
        assert_eq!(parsed.from.as_deref(), Some("sender@example.com"));
        assert_eq!(parsed.to.as_deref(), Some("user@example.com"));
        assert_eq!(parsed.body_text.as_deref(), Some("Hello there"));
        assert!(parsed.attachments.is_empty());
        assert!(!parsed.attachments_truncated);
    }

    #[test]
    fn derives_body_text_from_html_only_message() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: user@example.com\r\n",
            "Subject: HTML\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Hello <b>there</b></p></body></html>"
        )
        .as_bytes();

        let parsed = parse_message(
            raw,
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(10000),
        )
        .expect("parse should succeed");

        assert_eq!(parsed.body_text.as_deref(), Some("Hello there"));
        assert_eq!(parsed.body_html_sanitized, None);
    }

    #[test]
    fn includes_html_body_when_requested_for_html_only_message() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: user@example.com\r\n",
            "Subject: HTML\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Hello <b>there</b></p></body></html>"
        )
        .as_bytes();

        let parsed = parse_message(
            raw,
            2000,
            BodyMode::Both,
            AttachmentMode::Metadata,
            &test_limits(10000),
        )
        .expect("parse should succeed");

        assert_eq!(parsed.body_text.as_deref(), Some("Hello there"));
        assert_eq!(
            parsed.body_html_sanitized.as_deref(),
            Some("<p>Hello <b>there</b></p>")
        );
    }

    #[test]
    fn prefers_meaningful_plain_text_over_html() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: user@example.com\r\n",
            "Subject: Alt\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=\"alt\"\r\n",
            "\r\n",
            "--alt\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Hello from plain text\r\n",
            "--alt\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Hello from <b>HTML</b></p></body></html>\r\n",
            "--alt--\r\n"
        )
        .as_bytes();

        let parsed = parse_message(
            raw,
            2000,
            BodyMode::Both,
            AttachmentMode::Metadata,
            &test_limits(10000),
        )
        .expect("parse should succeed");

        assert_eq!(parsed.body_text.as_deref(), Some("Hello from plain text"));
        assert_eq!(
            parsed.body_html_sanitized.as_deref(),
            Some("<p>Hello from <b>HTML</b></p>")
        );
    }

    #[test]
    fn falls_back_to_html_when_plain_text_is_whitespace_only() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: user@example.com\r\n",
            "Subject: Alt\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=\"alt\"\r\n",
            "\r\n",
            "--alt\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "  \r\n\t\r\n",
            "--alt\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>Hello from <b>HTML</b></p></body></html>\r\n",
            "--alt--\r\n"
        )
        .as_bytes();

        let parsed = parse_message(
            raw,
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(10000),
        )
        .expect("parse should succeed");

        assert_eq!(parsed.body_text.as_deref(), Some("Hello from HTML"));
        assert_eq!(parsed.body_html_sanitized, None);
    }

    #[test]
    fn truncates_attachment_collection_at_limit() {
        let mut raw = concat!(
            "From: sender@example.com\r\n",
            "To: user@example.com\r\n",
            "Subject: Many Attachments\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"mix\"\r\n",
            "\r\n",
            "--mix\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "hello\r\n",
        )
        .to_owned();

        for idx in 0..55 {
            raw.push_str(&format!(
                "--mix\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"f{idx}.bin\"\r\n\r\npayload-{idx}\r\n"
            ));
        }
        raw.push_str("--mix--\r\n");

        let parsed = parse_message(
            raw.as_bytes(),
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(10000),
        )
        .expect("parse should succeed");
        assert_eq!(parsed.body_text.as_deref(), Some("hello"));
        assert_eq!(parsed.attachments.len(), MAX_ATTACHMENTS);
        assert!(parsed.attachments_truncated);
    }

    #[test]
    fn attachment_size_uses_decoded_bytes_for_base64_parts() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=\"mix\"\r\n",
            "MIME-Version: 1.0\r\n",
            "\r\n",
            "--mix\r\n",
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"payload.bin\"\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--mix--\r\n"
        );
        let parsed_mail = mailparse::parse_mail(raw.as_bytes()).expect("mail must parse");
        let attachment = &parsed_mail.subparts[0];

        assert_eq!(attachment_size_bytes(attachment), 5);

        let mut limits = test_limits(10_000);
        limits.decode_budget_bytes = 5;
        let parsed = parse_message(
            raw.as_bytes(),
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("exact decoded-byte budget should accept the attachment");
        assert_eq!(parsed.attachments[0].size_bytes, Some(5));
        assert!(
            !parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }

    #[test]
    fn full_attachment_size_is_unavailable_when_decode_budget_prevents_decoding() {
        let raw = concat!(
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"payload.bin\"\r\n\r\n",
            "aGVsbG8="
        );
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 4;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("budget-limited attachment should return metadata");

        assert_eq!(parsed.attachments[0].size_bytes, None);
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }

    #[test]
    fn full_attachment_decode_stops_at_budget_before_invalid_suffix() {
        let raw = concat!(
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"payload.bin\"\r\n\r\n",
            "aGVsbG8=!!!!"
        );
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 5;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("decoding should stop once the configured budget is exhausted");

        assert_eq!(parsed.attachments[0].size_bytes, None);
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }

    #[test]
    fn full_attachments_share_decode_budget() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=\"mix\"\r\n",
            "MIME-Version: 1.0\r\n",
            "\r\n",
            "--mix\r\n",
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"one.bin\"\r\n",
            "\r\n",
            "b25l\r\n",
            "--mix\r\n",
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"two.bin\"\r\n",
            "\r\n",
            "dHdv\r\n",
            "--mix--\r\n",
        );
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 5;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("budget-limited attachments should return metadata");

        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].size_bytes, Some(3));
        assert_eq!(parsed.attachments[1].size_bytes, None);
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }

    #[test]
    fn partial_full_attachment_decode_consumes_the_shared_budget() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=\"mix\"\r\n",
            "MIME-Version: 1.0\r\n",
            "\r\n",
            "--mix\r\n",
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"oversized.bin\"\r\n",
            "\r\n",
            "YWJjZGVm\r\n",
            "--mix\r\n",
            "Content-Type: application/octet-stream\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename=\"three.bin\"\r\n",
            "\r\n",
            "eHl6\r\n",
            "--mix--\r\n",
        );
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 5;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("partial attachment decodes should return metadata");

        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].size_bytes, None);
        assert_eq!(parsed.attachments[1].size_bytes, None);
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }

    #[test]
    fn raw_parse_reports_part_limit() {
        let raw = concat!(
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n",
            "--x--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_parts = 1;
        limits.max_depth = 10;
        limits.decode_budget_bytes = 4;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &limits,
        )
        .expect("limits return a partial message");

        assert!(parsed.body_text.is_none());
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::MaxParts)
        );
    }

    #[test]
    fn raw_parse_reports_depth_limit() {
        let raw = concat!(
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n",
            "--x--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_depth = 0;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &limits,
        )
        .expect("depth limit returns a partial message");

        assert!(parsed.body_text.is_none());
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::MaxDepth)
        );
    }
    #[test]
    fn depth_limit_takes_precedence_over_remaining_part_budget() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n",
            "--b\r\nContent-Type: text/plain\r\n\r\none\r\n",
            "--b\r\nContent-Type: text/plain\r\n\r\ntwo\r\n",
            "--b--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_depth = 0;
        limits.max_parts = 2;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &limits,
        )
        .expect("depth overflow returns a partial message");

        assert!(parsed.body_text.is_none());
        assert_eq!(parsed.processing_limits, vec![ProcessingLimit::MaxDepth]);
    }

    #[test]
    fn initial_closing_delimiter_still_preflights_mailparse_child() {
        let raw = concat!(
            "Subject: Initial close\r\n",
            "Content-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x--\r\n",
            "Content-Type: multipart/mixed; boundary=y\r\n\r\n",
            "--y\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "hidden\r\n",
            "--y--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_depth = 0;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &limits,
        )
        .expect("mailparse-compatible child must be rejected before recursive parsing");

        assert_eq!(parsed.subject.as_deref(), Some("Initial close"));
        assert!(parsed.body_text.is_none());
        assert_eq!(parsed.processing_limits, vec![ProcessingLimit::MaxDepth]);
    }

    #[test]
    fn metadata_and_non_pdf_attachments_do_not_charge_extraction_budget() {
        let raw = concat!(
            "Content-Type: application/octet-stream; name=\"fallback.bin\"\r\n",
            "Content-Disposition: inline\r\n\r\n",
            "payload"
        );
        let mut limits = test_limits(100);
        limits.attachment_extract_budget_bytes = 0;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("metadata parse must succeed");

        assert_eq!(
            parsed.attachments[0].filename.as_deref(),
            Some("fallback.bin")
        );
        assert_eq!(parsed.attachments[0].size_bytes, Some(7));
        assert!(
            !parsed
                .processing_limits
                .contains(&ProcessingLimit::AttachmentExtractBudgetBytes)
        );
    }

    #[test]
    fn depth_overflow_returns_header_only_without_recursive_parse() {
        let raw = concat!(
            "Subject: Deep message\r\n",
            "From: sender@example.com\r\n",
            "X-Preserved: yes\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
            "--outer\r\n",
            "Content-Type: multipart/mixed; boundary=inner\r\n\r\n",
            "--inner\r\n",
            "Malformed Header\r\n\r\n",
            "unreachable\r\n",
            "--inner--\r\n",
            "--outer--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_depth = 0;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Both,
            AttachmentMode::ExtractText,
            &limits,
        )
        .expect("depth overflow returns top-level metadata");

        assert_eq!(parsed.subject.as_deref(), Some("Deep message"));
        assert_eq!(parsed.from.as_deref(), Some("sender@example.com"));
        assert!(
            parsed.headers_all.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("x-preserved") && value == "yes"
            })
        );
        assert!(parsed.body_text.is_none());
        assert!(parsed.body_html_sanitized.is_none());
        assert!(parsed.attachments.is_empty());
        assert!(parsed.attachments_truncated);
        assert_eq!(parsed.processing_limits, vec![ProcessingLimit::MaxDepth]);
    }

    #[test]
    fn part_overflow_returns_header_only_without_recursive_parse() {
        let raw = concat!(
            "Subject: Wide message\r\n",
            "To: recipient@example.com\r\n",
            "X-Preserved: yes\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
            "--outer\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "first\r\n",
            "--outer\r\n",
            "Malformed Header\r\n\r\n",
            "unreachable\r\n",
            "--outer--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_parts = 2;

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Both,
            AttachmentMode::ExtractText,
            &limits,
        )
        .expect("part overflow returns top-level metadata");

        assert_eq!(parsed.subject.as_deref(), Some("Wide message"));
        assert_eq!(parsed.to.as_deref(), Some("recipient@example.com"));
        assert!(
            parsed.headers_all.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("x-preserved") && value == "yes"
            })
        );
        assert!(parsed.body_text.is_none());
        assert!(parsed.body_html_sanitized.is_none());
        assert!(parsed.attachments.is_empty());
        assert!(parsed.attachments_truncated);
        assert_eq!(parsed.processing_limits, vec![ProcessingLimit::MaxParts]);
    }

    #[test]
    fn filename_only_text_attachment_is_not_selected_as_body() {
        let raw = concat!(
            "Subject: Attachment only\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
            "--outer\r\n",
            "Content-Type: text/plain; name=\"notes.txt\"\r\n\r\n",
            "attachment contents\r\n",
            "--outer--\r\n"
        );

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(100),
        )
        .expect("filename-only attachment parses");

        assert_eq!(parsed.subject.as_deref(), Some("Attachment only"));
        assert!(parsed.body_text.is_none());
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename.as_deref(), Some("notes.txt"));
        assert_eq!(parsed.attachments[0].content_type, "text/plain");
        assert!(parsed.processing_limits.is_empty());
    }

    #[test]
    fn malformed_text_part_does_not_abort_valid_siblings() {
        let raw = concat!(
            "Subject: Recover siblings\r\n",
            "X-Preserved: yes\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
            "--outer\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "%%%invalid%%%\r\n",
            "--outer\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "valid sibling\r\n",
            "--outer\r\n",
            "Content-Type: application/octet-stream; name=\"data.bin\"\r\n",
            "Content-Disposition: attachment; filename=\"data.bin\"\r\n\r\n",
            "payload\r\n",
            "--outer--\r\n"
        );

        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(100),
        )
        .expect("a malformed text part is a local omission");

        assert_eq!(parsed.subject.as_deref(), Some("Recover siblings"));
        assert!(
            parsed.headers_all.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("x-preserved") && value == "yes"
            })
        );
        assert_eq!(parsed.body_text.as_deref(), Some("valid sibling"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename.as_deref(), Some("data.bin"));
        assert!(parsed.processing_limits.is_empty());
    }
}
