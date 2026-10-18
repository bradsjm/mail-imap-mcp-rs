//! Message parsing and MIME handling
//!
//! Parses RFC822 messages using `mailparse`, extracts body text/HTML,
//! and handles attachments. Sanitizes HTML, derives fallback text from HTML,
//! and supports optional PDF text extraction.

use std::collections::BTreeMap;

use base64::Engine;
use mailparse::body::Body;
use mailparse::{DispositionType, MailHeader, ParsedContentType, ParsedMail};

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

/// Decode one text body part and store it in the walk state.
///
/// Complete parts decode through mailparse. When that fails, for example on a
/// base64 body cut by a prefix fetch, the bounded transfer decoders keep the
/// decodable head of the part instead of dropping the whole body.
fn decode_text_part(
    part: &ParsedMail<'_>,
    is_html: bool,
    state: &mut WalkState,
    config: &WalkConfig,
) -> AppResult<()> {
    let remaining = config
        .limits
        .decode_budget_bytes
        .saturating_sub(state.decoded_bytes);
    let decoded = match part.get_body() {
        Ok(decoded) => decoded,
        Err(_) => match decode_text_head(part, remaining) {
            TextHead::Text(decoded) => decoded,
            TextHead::OverBudget => {
                state.hit(ProcessingLimit::DecodeBudgetBytes);
                return Ok(());
            }
            TextHead::Undecodable => return Ok(()),
        },
    };
    if decoded.len() > remaining {
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

/// Result of the lenient bounded text decode used when mailparse fails.
enum TextHead {
    /// The decodable head of the part, converted to UTF-8.
    Text(String),
    /// The decoded head exceeds the remaining decode budget.
    OverBudget,
    /// The part holds no decodable text, for example invalid base64.
    Undecodable,
}

/// Decode the transfer encoding of a text part within a byte budget.
///
/// Truncated transfer-encoded data keeps its decodable head. The decoder runs
/// with three spare bytes, the size of one base64 chunk, so that a result
/// longer than `remaining` always means the budget was exceeded rather than the
/// data being cut.
fn decode_text_head(part: &ParsedMail<'_>, remaining: usize) -> TextHead {
    let Ok((bytes, _partial)) = decode_parsed_part_bounded(part, remaining.saturating_add(3))
    else {
        return TextHead::Undecodable;
    };
    if bytes.len() > remaining {
        return TextHead::OverBudget;
    }
    match decoded_bytes_to_string(&bytes, &part.ctype) {
        Some(text) => TextHead::Text(text),
        None => TextHead::Undecodable,
    }
}

/// Convert already transfer-decoded bytes to UTF-8 using the part charset.
///
/// Uses mailparse with no transfer encoding so the encoding is never applied
/// twice.
fn decoded_bytes_to_string(bytes: &[u8], ctype: &ParsedContentType) -> Option<String> {
    match Body::new(bytes, ctype, &None) {
        Body::SevenBit(body) | Body::EightBit(body) => body.get_as_string().ok(),
        Body::Binary(body) => body.get_as_string().ok(),
        Body::Base64(_) | Body::QuotedPrintable(_) => None,
    }
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

/// Maximum Message-ID values returned by [`thread_reference_ids`].
const THREAD_REFERENCE_MAX_IDS: usize = 20;

/// Maximum bytes of one header value processed by [`thread_reference_ids`].
const THREAD_REFERENCE_MAX_HEADER_BYTES: usize = 8_192;

/// Maximum bytes of one Message-ID kept by [`thread_reference_ids`].
const THREAD_REFERENCE_MAX_ID_BYTES: usize = 256;

/// Message-ID values that identify one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReferences {
    /// Unique IDs without angle brackets, in first-seen order.
    pub ids: Vec<String>,
    /// True when a header or ID was skipped, so `ids` may be incomplete.
    pub skipped_headers: bool,
}

/// Collect conversation Message-IDs from Message-ID, In-Reply-To, and References.
///
/// Headers are read in that order and every occurrence counts. Values are
/// parsed with `mailparse::msgidparse`; unparseable or over-long headers and
/// IDs are skipped and reported through `skipped_headers`. At most 20 unique
/// IDs are returned, spelled exactly as in the headers.
pub fn thread_reference_ids(headers: &[(String, String)]) -> ThreadReferences {
    let mut ids: Vec<String> = Vec::new();
    let mut skipped_headers = false;
    for name in ["message-id", "in-reply-to", "references"] {
        for (_, value) in headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            if value.len() > THREAD_REFERENCE_MAX_HEADER_BYTES {
                skipped_headers = true;
                continue;
            }
            let Ok(parsed) = mailparse::msgidparse(value) else {
                skipped_headers = true;
                continue;
            };
            for id in parsed.iter() {
                if id.is_empty() || id.len() > THREAD_REFERENCE_MAX_ID_BYTES {
                    skipped_headers = true;
                } else if ids.contains(id) {
                    continue;
                } else if ids.len() >= THREAD_REFERENCE_MAX_IDS {
                    skipped_headers = true;
                } else {
                    ids.push(id.clone());
                }
            }
        }
    }
    ThreadReferences {
        ids,
        skipped_headers,
    }
}

/// Body preview derived from a message prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewOutcome {
    /// Whitespace-normalized preview, absent when no meaningful text exists.
    pub preview: Option<String>,
    /// Resource ceilings reached while parsing the prefix.
    pub processing_limits: Vec<ProcessingLimit>,
}

/// Build a whitespace-normalized text preview from a raw message prefix.
///
/// Parsing is bounded by the prefix length, not by `preview_max_chars`, so
/// leading whitespace cannot consume the preview. A truncated multipart,
/// base64, or quoted-printable body yields a best-effort preview or `None`.
pub fn build_preview(
    raw: &[u8],
    preview_max_chars: usize,
    limits: &ProcessingLimits,
) -> AppResult<PreviewOutcome> {
    let parse = |input: &[u8]| {
        parse_message(
            input,
            input.len().max(1),
            BodyMode::Text,
            AttachmentMode::None,
            limits,
        )
    };
    // A prefix can end inside a header line, for example after a lone CR, which
    // mailparse rejects. Retry once with only the complete lines.
    let parsed = match parse(raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            let complete_lines = raw
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(&raw[..0], |end| &raw[..=end]);
            if complete_lines.len() == raw.len() {
                return Err(error);
            }
            parse(complete_lines)?
        }
    };
    let mut preview = String::new();
    let mut chars = 0usize;
    if let Some(body) = parsed.body_text.as_deref() {
        'words: for word in body.split_whitespace() {
            if !preview.is_empty() {
                preview.push(' ');
                chars += 1;
            }
            for ch in word.chars() {
                if chars >= preview_max_chars {
                    break 'words;
                }
                preview.push(ch);
                chars += 1;
            }
        }
    }
    Ok(PreviewOutcome {
        preview: (!preview.is_empty()).then_some(preview),
        processing_limits: parsed.processing_limits,
    })
}

/// Maximum characters in an attachment part ID.
const PART_ID_MAX_CHARS: usize = 64;

/// Requested representation of an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentRequest {
    /// Extracted text limited to `max_chars` Unicode characters.
    Text {
        /// Maximum characters returned.
        max_chars: usize,
    },
    /// A window of decoded bytes.
    Bytes {
        /// First decoded byte returned.
        offset_bytes: usize,
        /// Maximum decoded bytes returned.
        max_bytes: usize,
    },
}

/// Attachment content selected by an [`AttachmentRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentPayload {
    /// Extracted text.
    Text {
        /// Text cut to the requested character limit.
        text: String,
        /// True when text was cut at the limit.
        truncated: bool,
    },
    /// One page of decoded bytes.
    Bytes {
        /// Decoded bytes of this page.
        bytes: Vec<u8>,
        /// Offset of the first returned byte.
        offset_bytes: usize,
        /// True when more decoded bytes follow this page.
        has_more: bool,
    },
}

/// A fully decoded attachment with the requested payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRead {
    /// Attachment filename, when declared.
    pub filename: Option<String>,
    /// Lowercase MIME type of the part.
    pub content_type: String,
    /// Full decoded size in bytes.
    pub size_bytes: usize,
    /// Requested content.
    pub payload: AttachmentPayload,
}

/// Result of reading one attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentOutcome {
    /// The attachment was read completely and the payload is present.
    Content(AttachmentRead),
    /// A resource ceiling prevented reading; no payload is available.
    LimitExceeded(ProcessingLimit),
    /// Text mode cannot represent this attachment; bytes mode still can.
    TextUnavailable {
        /// Attachment filename, when declared.
        filename: Option<String>,
        /// Lowercase MIME type of the part.
        content_type: String,
        /// Full decoded size in bytes.
        size_bytes: usize,
        /// Short explanation for the caller.
        reason: String,
    },
}

/// Parse a part ID into child indices that follow the root part `1`.
///
/// The root is `1` and every later segment is a positive decimal index
/// without sign or leading zero, matching the IDs assigned by `walk_parts`.
pub(crate) fn parse_part_id(part_id: &str) -> AppResult<Vec<usize>> {
    let invalid = || {
        AppError::InvalidInput(
            "part_id must be '1' followed by positive dot-separated child indices".to_owned(),
        )
    };
    if part_id.chars().count() > PART_ID_MAX_CHARS {
        return Err(AppError::InvalidInput(format!(
            "part_id must be at most {PART_ID_MAX_CHARS} characters"
        )));
    }
    let mut segments = part_id.split('.');
    if segments.next() != Some("1") {
        return Err(invalid());
    }
    segments
        .map(|segment| {
            let digits = segment.bytes().all(|byte| byte.is_ascii_digit());
            if segment.is_empty() || !digits || segment.starts_with('0') {
                return Err(invalid());
            }
            segment.parse::<usize>().map_err(|_| invalid())
        })
        .collect()
}

/// Read one attachment from a raw message with explicit text or byte output.
///
/// The part is located through the child indices of its ID, transfer-decoded
/// once within the decode budget, and never read when a limit applies.
/// Bytes mode never runs text extraction.
pub fn read_attachment(
    raw: &[u8],
    part_id: &str,
    request: &AttachmentRequest,
    limits: &ProcessingLimits,
) -> AppResult<AttachmentOutcome> {
    let path = parse_part_id(part_id)?;
    if let Some(limit) = preflight_raw_mime(raw, limits.max_depth, limits.max_parts)?
        .into_iter()
        .next()
    {
        return Ok(AttachmentOutcome::LimitExceeded(limit));
    }
    let parsed = mailparse::parse_mail(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse RFC822 message: {e}")))?;
    let mut part = &parsed;
    for index in path {
        part = part
            .subparts
            .get(index - 1)
            .ok_or_else(|| AppError::NotFound(format!("attachment part {part_id} not found")))?;
    }
    if !part.subparts.is_empty() {
        return Err(AppError::InvalidInput(format!(
            "part {part_id} is a multipart container, not an attachment"
        )));
    }
    let disposition = part.get_content_disposition();
    let filename = attachment_filename(part, &disposition.params);
    if disposition.disposition != DispositionType::Attachment && filename.is_none() {
        return Err(AppError::InvalidInput(format!(
            "part {part_id} is not an attachment"
        )));
    }

    let (decoded, partial) = decode_parsed_part_bounded(part, limits.decode_budget_bytes)?;
    if partial {
        return Ok(AttachmentOutcome::LimitExceeded(
            ProcessingLimit::DecodeBudgetBytes,
        ));
    }
    let content_type = part.ctype.mimetype.to_ascii_lowercase();
    let size_bytes = decoded.len();
    let payload = match *request {
        AttachmentRequest::Bytes {
            offset_bytes,
            max_bytes,
        } => {
            if offset_bytes > size_bytes {
                return Err(AppError::InvalidInput(format!(
                    "offset_bytes {offset_bytes} exceeds attachment size {size_bytes}"
                )));
            }
            let end = offset_bytes.saturating_add(max_bytes).min(size_bytes);
            AttachmentPayload::Bytes {
                bytes: decoded[offset_bytes..end].to_vec(),
                offset_bytes,
                has_more: end < size_bytes,
            }
        }
        AttachmentRequest::Text { max_chars } => {
            let remaining = limits.decode_budget_bytes.saturating_sub(size_bytes);
            match extract_attachment_text(part, &content_type, &decoded, remaining, limits) {
                TextExtraction::Text(text) => {
                    let truncated = text.chars().nth(max_chars).is_some();
                    AttachmentPayload::Text {
                        text: truncate_chars(text, max_chars),
                        truncated,
                    }
                }
                TextExtraction::Limit(limit) => return Ok(AttachmentOutcome::LimitExceeded(limit)),
                TextExtraction::Unavailable(reason) => {
                    return Ok(AttachmentOutcome::TextUnavailable {
                        filename,
                        content_type,
                        size_bytes,
                        reason,
                    });
                }
            }
        }
    };
    Ok(AttachmentOutcome::Content(AttachmentRead {
        filename,
        content_type,
        size_bytes,
        payload,
    }))
}

/// Result of extracting text from decoded attachment bytes.
enum TextExtraction {
    /// Extracted text, not yet cut to the caller's character limit.
    Text(String),
    /// A resource ceiling prevented extraction.
    Limit(ProcessingLimit),
    /// The type or content cannot be shown as text.
    Unavailable(String),
}

/// Extract text from transfer-decoded attachment bytes by content type.
///
/// `remaining_decode_bytes` bounds charset-converted output.
fn extract_attachment_text(
    part: &ParsedMail<'_>,
    content_type: &str,
    decoded: &[u8],
    remaining_decode_bytes: usize,
    limits: &ProcessingLimits,
) -> TextExtraction {
    if content_type == "application/pdf" {
        if decoded.len() > limits.attachment_extract_budget_bytes {
            return TextExtraction::Limit(ProcessingLimit::AttachmentExtractBudgetBytes);
        }
        return match pdf_extract::extract_text_from_mem(decoded) {
            Ok(text) => TextExtraction::Text(text),
            Err(_) => TextExtraction::Unavailable("PDF text extraction failed".to_owned()),
        };
    }
    let supported = content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/json" | "application/xml" | "application/ics"
        );
    if !supported {
        return TextExtraction::Unavailable(format!(
            "text mode does not support content type {content_type}"
        ));
    }

    // Attachments without a declared charset are read as UTF-8, not mailparse's
    // us-ascii default, which would corrupt UTF-8 JSON, XML, and CSV.
    let charset = if part.ctype.params.contains_key("charset") {
        part.ctype.charset.clone()
    } else {
        "utf-8".to_owned()
    };
    let ctype = ParsedContentType {
        mimetype: part.ctype.mimetype.clone(),
        charset,
        params: part.ctype.params.clone(),
    };
    let Some(text) = decoded_bytes_to_string(decoded, &ctype) else {
        return TextExtraction::Unavailable("attachment charset conversion failed".to_owned());
    };
    if text.len() > remaining_decode_bytes {
        return TextExtraction::Limit(ProcessingLimit::DecodeBudgetBytes);
    }
    if content_type != "text/html" {
        return TextExtraction::Text(text);
    }
    TextExtraction::Text(html_to_text(&ammonia::clean(&text)).unwrap_or_default())
}

/// Truncate string to maximum characters (Unicode-aware)
///
/// Preserves complete characters, never splitting multi-byte sequences.
pub fn truncate_chars(input: String, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {

    use base64::Engine;

    use super::{
        AttachmentOutcome, AttachmentPayload, AttachmentRead, AttachmentRequest, MAX_ATTACHMENTS,
        MIME_MAX_PARTS, ProcessingLimit, ProcessingLimits, attachment_size_bytes, build_preview,
        curated_headers, parse_message, read_attachment, thread_reference_ids, truncate_chars,
    };
    use crate::errors::AppError;
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

    fn header_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn thread_references_read_root_message_id() {
        let refs = thread_reference_ids(&header_pairs(&[("Message-ID", "<root@example.com>")]));
        assert_eq!(refs.ids, vec!["root@example.com"]);
        assert!(!refs.skipped_headers);
    }

    #[test]
    fn thread_references_order_headers_and_ignore_name_case() {
        let refs = thread_reference_ids(&header_pairs(&[
            ("REFERENCES", "<a@x> <b@x>"),
            ("in-reply-to", "<b@x>"),
            ("message-id", "<reply@x>"),
            ("Message-Id", "<Second@x>"),
            ("Subject", "<ignored@x>"),
        ]));
        assert_eq!(refs.ids, vec!["reply@x", "Second@x", "b@x", "a@x"]);
        assert!(!refs.skipped_headers);
    }

    #[test]
    fn thread_references_preserve_case_and_remove_exact_duplicates() {
        let refs = thread_reference_ids(&header_pairs(&[
            ("Message-ID", "<Mixed@Example.COM>"),
            (
                "References",
                "<mixed@example.com> <Mixed@Example.COM> <Mixed@Example.COM>",
            ),
        ]));
        assert_eq!(refs.ids, vec!["Mixed@Example.COM", "mixed@example.com"]);
        assert!(!refs.skipped_headers);
    }

    #[test]
    fn thread_references_cap_unique_ids_and_report_omissions() {
        let many: String = (0..25).map(|index| format!("<id{index}@x> ")).collect();
        let refs = thread_reference_ids(&header_pairs(&[("References", many.as_str())]));
        assert_eq!(refs.ids.len(), 20);
        assert_eq!(refs.ids[0], "id0@x");
        assert_eq!(refs.ids[19], "id19@x");
        assert!(refs.skipped_headers);

        let exact: String = (0..20).map(|index| format!("<id{index}@x> ")).collect();
        let refs = thread_reference_ids(&header_pairs(&[
            ("References", exact.as_str()),
            ("In-Reply-To", "<id3@x>"),
        ]));
        assert_eq!(refs.ids.len(), 20);
        assert!(!refs.skipped_headers);
    }

    #[test]
    fn thread_references_skip_overlong_header_and_overlong_id() {
        let long_header = format!("<{}@x>", "a".repeat(8_192));
        let refs = thread_reference_ids(&header_pairs(&[
            ("References", long_header.as_str()),
            ("Message-ID", "<ok@x>"),
        ]));
        assert_eq!(refs.ids, vec!["ok@x"]);
        assert!(refs.skipped_headers);

        let long_id = format!("<{}>", "b".repeat(257));
        let refs = thread_reference_ids(&header_pairs(&[
            ("In-Reply-To", long_id.as_str()),
            ("Message-ID", "<ok@x>"),
        ]));
        assert_eq!(refs.ids, vec!["ok@x"]);
        assert!(refs.skipped_headers);

        let boundary_id = format!("<{}>", "c".repeat(256));
        let refs = thread_reference_ids(&header_pairs(&[("Message-ID", boundary_id.as_str())]));
        assert_eq!(refs.ids.len(), 1);
        assert!(!refs.skipped_headers);
    }

    #[test]
    fn thread_references_skip_malformed_and_comment_bearing_headers() {
        for value in [
            "no-brackets@x",
            "<unterminated@x",
            "(comment <fake@x>) <real@x>",
            "<real@x> (trailing <fake@x>)",
        ] {
            let refs = thread_reference_ids(&header_pairs(&[("References", value)]));
            assert!(refs.ids.is_empty(), "{value}");
            assert!(refs.skipped_headers, "{value}");
        }
        let refs = thread_reference_ids(&header_pairs(&[("Message-ID", "<>")]));
        assert!(refs.ids.is_empty());
        assert!(refs.skipped_headers);
        let refs = thread_reference_ids(&header_pairs(&[("References", "")]));
        assert!(refs.ids.is_empty());
        assert!(!refs.skipped_headers);
    }

    #[test]
    fn thread_references_never_panic_on_arbitrary_text() {
        let samples = [
            "<",
            ">",
            "<<>>",
            "<a<b>>",
            "\u{0}<\u{0}>",
            "<é😀@x> <日本語>",
            "\r\n<a@x>\r\n <b@x>",
            "   ",
            "<a@x><b@x>",
        ];
        for sample in samples {
            for name in ["Message-ID", "In-Reply-To", "References"] {
                let _ = thread_reference_ids(&header_pairs(&[(name, sample)]));
            }
        }
        let refs = thread_reference_ids(&header_pairs(&[("References", "<é😀@x> <日本語>")]));
        assert_eq!(refs.ids, vec!["é😀@x", "日本語"]);
        let refs = thread_reference_ids(&header_pairs(&[("References", "<a@x><b@x>")]));
        assert_eq!(refs.ids, vec!["a@x", "b@x"]);
    }

    fn preview_of(raw: &str, max_chars: usize) -> super::PreviewOutcome {
        build_preview(raw.as_bytes(), max_chars, &test_limits(100)).expect("preview builds")
    }

    #[test]
    fn preview_leading_whitespace_does_not_consume_limit() {
        let raw = format!(
            "Subject: s\r\n\r\n{}hello   world\r\n\r\n",
            " \r\n\t".repeat(100)
        );
        let outcome = preview_of(&raw, 50);
        assert_eq!(outcome.preview.as_deref(), Some("hello world"));
        assert!(outcome.processing_limits.is_empty());
    }

    #[test]
    fn preview_normalizes_whitespace_then_truncates_to_characters() {
        let outcome = preview_of("Subject: s\r\n\r\nalpha \r\n\r\n beta\tgamma", 50);
        assert_eq!(outcome.preview.as_deref(), Some("alpha beta gamma"));

        let outcome = preview_of("Subject: s\r\n\r\nab    cd", 3);
        assert_eq!(outcome.preview.as_deref(), Some("ab "));
        let outcome = preview_of("Subject: s\r\n\r\nab cd ef", 4);
        assert_eq!(outcome.preview.as_deref(), Some("ab c"));

        let body = "word ".repeat(200);
        let raw = format!("Subject: s\r\n\r\n{body}");
        assert_eq!(
            preview_of(&raw, 50).preview.map(|p| p.chars().count()),
            Some(50)
        );
        assert_eq!(
            preview_of(&raw, 500).preview.map(|p| p.chars().count()),
            Some(500)
        );
    }

    #[test]
    fn preview_counts_multibyte_characters() {
        let raw = "Subject: s\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n日本語😀テキスト";
        let outcome = preview_of(raw, 4);
        assert_eq!(outcome.preview.as_deref(), Some("日本語😀"));
    }

    #[test]
    fn preview_derives_text_from_html_only_message() {
        let raw = concat!(
            "Subject: s\r\nContent-Type: text/html\r\n\r\n",
            "<html><body><script>evil()</script><p>Hello <b>HTML</b></p></body></html>"
        );
        let outcome = preview_of(raw, 50);
        assert_eq!(outcome.preview.as_deref(), Some("Hello HTML"));
    }

    #[test]
    fn preview_keeps_decodable_head_of_truncated_base64() {
        let raw = concat!(
            "Subject: s\r\nContent-Type: text/plain\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "SGVsbG8gd29ybGQgZnJvbSBiYXNlNjQ"
        );
        let outcome = preview_of(raw, 500);
        let preview = outcome.preview.expect("head is decodable");
        assert!(preview.starts_with("Hello world from base"), "{preview}");

        let raw = concat!(
            "Subject: s\r\nContent-Type: text/plain\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "SGVsbG8gd2"
        );
        assert_eq!(preview_of(raw, 500).preview.as_deref(), Some("Hello"));

        let raw = concat!(
            "Subject: s\r\nContent-Type: text/plain\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "SG"
        );
        assert!(preview_of(raw, 500).preview.is_none());
    }

    #[test]
    fn preview_keeps_decodable_head_of_truncated_quoted_printable() {
        let raw = concat!(
            "Subject: s\r\nContent-Type: text/plain; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
            "caf=C3=A9 au lait =\r\nand more =C3"
        );
        let preview = preview_of(raw, 500).preview.expect("head is decodable");
        assert!(preview.starts_with("café au lait and more"), "{preview}");

        let raw = concat!(
            "Subject: s\r\nContent-Type: text/plain\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
            "cut here ="
        );
        let preview = preview_of(raw, 500).preview.expect("head is decodable");
        assert!(preview.starts_with("cut here"), "{preview}");
    }

    #[test]
    fn preview_handles_truncated_multipart_prefixes() {
        let full = concat!(
            "Subject: s\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n",
            "--b\r\nContent-Type: text/plain\r\n\r\nplain body text\r\n",
            "--b\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n--b--\r\n"
        );
        for cut in 0..full.len() {
            let result = build_preview(&full.as_bytes()[..cut], 50, &test_limits(100));
            assert!(result.is_ok(), "prefix of {cut} bytes failed: {result:?}");
        }
        let cut = full.find("html body").expect("fixture") + 3;
        let outcome = preview_of(&full[..cut], 50);
        assert_eq!(outcome.preview.as_deref(), Some("plain body text"));
        let cut = full.find("plain body").expect("fixture") + 5;
        let outcome = preview_of(&full[..cut], 50);
        assert_eq!(outcome.preview.as_deref(), Some("plain"));
    }

    #[test]
    fn preview_is_absent_without_body_content() {
        assert!(
            preview_of("Subject: only headers\r\n\r\n", 50)
                .preview
                .is_none()
        );
        assert!(preview_of("Subject: only headers", 50).preview.is_none());
        assert!(
            preview_of("Subject: s\r\n\r\n \r\n\t\r\n", 50)
                .preview
                .is_none()
        );
        assert!(
            build_preview(b"", 50, &test_limits(100))
                .expect("empty input")
                .preview
                .is_none()
        );
    }

    #[test]
    fn preview_reports_processing_limits_without_failing() {
        let nested = concat!(
            "Subject: s\r\nContent-Type: multipart/mixed; boundary=a\r\n\r\n",
            "--a\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n",
            "--b\r\nContent-Type: text/plain\r\n\r\ndeep\r\n--b--\r\n--a--\r\n"
        );
        let mut limits = test_limits(100);
        limits.max_depth = 1;
        let outcome = build_preview(nested.as_bytes(), 50, &limits).expect("depth is not an error");
        assert!(outcome.preview.is_none());
        assert_eq!(outcome.processing_limits, vec![ProcessingLimit::MaxDepth]);

        let mut limits = test_limits(100);
        limits.max_parts = 2;
        let outcome = build_preview(nested.as_bytes(), 50, &limits).expect("parts is not an error");
        assert!(outcome.preview.is_none());
        assert_eq!(outcome.processing_limits, vec![ProcessingLimit::MaxParts]);

        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 4;
        let outcome = build_preview(
            b"Subject: s\r\n\r\nthis body is larger than the budget",
            50,
            &limits,
        )
        .expect("decode budget is not an error");
        assert!(outcome.preview.is_none());
        assert_eq!(
            outcome.processing_limits,
            vec![ProcessingLimit::DecodeBudgetBytes]
        );
    }

    #[test]
    fn complete_text_decode_budget_behavior_is_unchanged() {
        let raw = concat!(
            "Subject: s\r\nContent-Type: text/plain\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "SGVsbG8gd29ybGQ="
        );
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 11;
        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &limits,
        )
        .expect("parses");
        assert_eq!(parsed.body_text.as_deref(), Some("Hello world"));
        limits.decode_budget_bytes = 10;
        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &limits,
        )
        .expect("parses");
        assert!(parsed.body_text.is_none());
        assert_eq!(
            parsed.processing_limits,
            vec![ProcessingLimit::DecodeBudgetBytes]
        );
    }

    fn b64(bytes: &[u8]) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        encoded
            .as_bytes()
            .chunks(60)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    /// Message with ids: 1.1 alternative, 1.1.1 plain, 1.1.2 html, 1.2 csv,
    /// 1.3 nested mixed, 1.3.1 json attachment, 1.3.2 binary attachment.
    fn nested_message(binary: &[u8]) -> String {
        format!(
            concat!(
                "Subject: nested\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n",
                "--outer\r\nContent-Type: multipart/alternative; boundary=alt\r\n\r\n",
                "--alt\r\nContent-Type: text/plain\r\n\r\nplain\r\n",
                "--alt\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n--alt--\r\n",
                "--outer\r\nContent-Type: text/csv; name=\"a.csv\"\r\n",
                "Content-Disposition: attachment; filename=\"a.csv\"\r\n\r\nx,y\r\n1,2\r\n",
                "--outer\r\nContent-Type: multipart/mixed; boundary=inner\r\n\r\n",
                "--inner\r\nContent-Type: application/json\r\n",
                "Content-Disposition: attachment; filename=\"d.json\"\r\n\r\n{{\"k\":\"v\"}}\r\n",
                "--inner\r\nContent-Type: application/octet-stream\r\n",
                "Content-Disposition: attachment; filename=\"b.bin\"\r\n",
                "Content-Transfer-Encoding: base64\r\n\r\n{binary}\r\n",
                "--inner--\r\n--outer--\r\n"
            ),
            binary = b64(binary)
        )
    }

    const TEXT_REQUEST: AttachmentRequest = AttachmentRequest::Text { max_chars: 10_000 };

    fn read_ok(raw: &str, part_id: &str, request: &AttachmentRequest) -> AttachmentRead {
        match read_attachment(raw.as_bytes(), part_id, request, &test_limits(100))
            .expect("attachment read succeeds")
        {
            AttachmentOutcome::Content(read) => read,
            other => panic!("expected content, got {other:?}"),
        }
    }

    fn all_bytes(offset_bytes: usize) -> AttachmentRequest {
        AttachmentRequest::Bytes {
            offset_bytes,
            max_bytes: usize::MAX,
        }
    }

    #[test]
    fn attachment_ids_from_parse_message_resolve_to_same_parts() {
        let binary: Vec<u8> = (0..=255u8).collect();
        let raw = nested_message(&binary);
        let parsed = parse_message(
            raw.as_bytes(),
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(100),
        )
        .expect("parses");
        let ids: Vec<&str> = parsed
            .attachments
            .iter()
            .map(|attachment| attachment.part_id.as_str())
            .collect();
        assert_eq!(ids, vec!["1.2", "1.3.1", "1.3.2"]);
        for attachment in &parsed.attachments {
            let read = read_ok(&raw, &attachment.part_id, &all_bytes(0));
            assert_eq!(read.filename, attachment.filename);
            assert_eq!(read.content_type, attachment.content_type);
            assert_eq!(Some(read.size_bytes), attachment.size_bytes);
        }
        let read = read_ok(&raw, "1.3.2", &all_bytes(0));
        assert_eq!(
            read.payload,
            AttachmentPayload::Bytes {
                bytes: binary,
                offset_bytes: 0,
                has_more: false
            }
        );
    }

    #[test]
    fn attachment_text_modes_for_common_types() {
        let raw = nested_message(b"bin");
        let read = read_ok(&raw, "1.2", &TEXT_REQUEST);
        assert_eq!(read.filename.as_deref(), Some("a.csv"));
        assert_eq!(read.size_bytes, 8);
        assert_eq!(
            read.payload,
            AttachmentPayload::Text {
                text: "x,y\r\n1,2".to_owned(),
                truncated: false
            }
        );
        let read = read_ok(&raw, "1.3.1", &TEXT_REQUEST);
        assert_eq!(
            read.payload,
            AttachmentPayload::Text {
                text: "{\"k\":\"v\"}".to_owned(),
                truncated: false
            }
        );
    }

    fn single_attachment(content_type: &str, encoding: &str, body: &str) -> String {
        format!(
            concat!(
                "Subject: one\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n",
                "--m\r\nContent-Type: text/plain\r\n\r\nbody\r\n",
                "--m\r\nContent-Type: {ct}\r\n",
                "Content-Disposition: attachment; filename=\"f\"\r\n",
                "Content-Transfer-Encoding: {enc}\r\n\r\n{body}\r\n--m--\r\n"
            ),
            ct = content_type,
            enc = encoding,
            body = body
        )
    }

    fn text_of(read: &AttachmentRead) -> (&str, bool) {
        match &read.payload {
            AttachmentPayload::Text { text, truncated } => (text.as_str(), *truncated),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn attachment_text_supports_calendar_xml_html_and_utf8_without_charset() {
        let calendar =
            single_attachment("text/calendar", "7bit", "BEGIN:VCALENDAR\r\nEND:VCALENDAR");
        let read = read_ok(&calendar, "1.2", &TEXT_REQUEST);
        assert_eq!(text_of(&read).0, "BEGIN:VCALENDAR\r\nEND:VCALENDAR");
        let ics = single_attachment("application/ics", "7bit", "BEGIN:VEVENT");
        assert_eq!(
            text_of(&read_ok(&ics, "1.2", &TEXT_REQUEST)).0,
            "BEGIN:VEVENT"
        );
        let xml = single_attachment("application/xml", "7bit", "<a>x</a>");
        assert_eq!(text_of(&read_ok(&xml, "1.2", &TEXT_REQUEST)).0, "<a>x</a>");
        let utf8 = single_attachment(
            "application/json",
            "base64",
            &b64("{\"n\":\"é日\"}".as_bytes()),
        );
        assert_eq!(
            text_of(&read_ok(&utf8, "1.2", &TEXT_REQUEST)).0,
            "{\"n\":\"é日\"}"
        );
        let html = single_attachment(
            "text/html",
            "7bit",
            "<html><script>bad()</script><p>Hello <i>there</i></p></html>",
        );
        let read = read_ok(&html, "1.2", &TEXT_REQUEST);
        assert_eq!(text_of(&read).0, "Hello there");
    }

    #[test]
    fn attachment_text_converts_declared_charset_once() {
        let latin1 = single_attachment(
            "text/plain; charset=iso-8859-1",
            "base64",
            &b64(&[0x63, 0x61, 0x66, 0xE9]),
        );
        assert_eq!(text_of(&read_ok(&latin1, "1.2", &TEXT_REQUEST)).0, "café");
        let qp = single_attachment(
            "text/plain; charset=iso-8859-1",
            "quoted-printable",
            "caf=E9",
        );
        let read = read_ok(&qp, "1.2", &TEXT_REQUEST);
        assert_eq!(text_of(&read).0, "café");
        assert_eq!(read.size_bytes, 4);
    }

    #[test]
    fn attachment_text_truncates_to_max_chars() {
        let raw = single_attachment("text/plain; charset=utf-8", "8bit", "日本語テキスト");
        let read = read_ok(&raw, "1.2", &AttachmentRequest::Text { max_chars: 3 });
        assert_eq!(text_of(&read), ("日本語", true));
        let read = read_ok(&raw, "1.2", &AttachmentRequest::Text { max_chars: 7 });
        assert_eq!(text_of(&read), ("日本語テキスト", false));
        assert_eq!(read.size_bytes, 21);
    }

    /// Build a minimal single-page PDF whose content stream shows `text`.
    fn tiny_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 24 Tf 72 700 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn attachment_pdf_text_uses_extraction_and_budget() {
        let pdf = tiny_pdf("Hello PDF");
        let raw = single_attachment("application/pdf", "base64", &b64(&pdf));
        let read = read_ok(&raw, "1.2", &TEXT_REQUEST);
        assert!(
            text_of(&read).0.contains("Hello PDF"),
            "{:?}",
            text_of(&read)
        );
        assert_eq!(read.size_bytes, pdf.len());

        let mut limits = test_limits(100);
        limits.attachment_extract_budget_bytes = pdf.len() - 1;
        let outcome =
            read_attachment(raw.as_bytes(), "1.2", &TEXT_REQUEST, &limits).expect("reads");
        assert_eq!(
            outcome,
            AttachmentOutcome::LimitExceeded(ProcessingLimit::AttachmentExtractBudgetBytes)
        );
    }

    #[test]
    fn attachment_bad_pdf_is_text_unavailable_but_bytes_mode_skips_extraction() {
        let raw = single_attachment(
            "application/pdf",
            "base64",
            &b64(b"%PDF-1.4 not a real pdf"),
        );
        let outcome = read_attachment(raw.as_bytes(), "1.2", &TEXT_REQUEST, &test_limits(100))
            .expect("reads");
        match outcome {
            AttachmentOutcome::TextUnavailable {
                filename,
                content_type,
                size_bytes,
                reason,
            } => {
                assert_eq!(filename.as_deref(), Some("f"));
                assert_eq!(content_type, "application/pdf");
                assert_eq!(size_bytes, 23);
                assert!(!reason.is_empty());
            }
            other => panic!("expected TextUnavailable, got {other:?}"),
        }
        let read = read_ok(&raw, "1.2", &all_bytes(0));
        assert_eq!(read.size_bytes, 23);

        let mut limits = test_limits(100);
        limits.attachment_extract_budget_bytes = 1;
        let outcome =
            read_attachment(raw.as_bytes(), "1.2", &all_bytes(0), &limits).expect("reads");
        assert!(matches!(outcome, AttachmentOutcome::Content(_)));
    }

    #[test]
    fn attachment_unsupported_type_is_text_unavailable() {
        let raw = single_attachment("image/png", "base64", &b64(&[1, 2, 3]));
        let outcome = read_attachment(raw.as_bytes(), "1.2", &TEXT_REQUEST, &test_limits(100))
            .expect("reads");
        match outcome {
            AttachmentOutcome::TextUnavailable {
                content_type,
                size_bytes,
                ..
            } => {
                assert_eq!(content_type, "image/png");
                assert_eq!(size_bytes, 3);
            }
            other => panic!("expected TextUnavailable, got {other:?}"),
        }
        assert!(matches!(
            read_attachment(raw.as_bytes(), "1.2", &all_bytes(0), &test_limits(100)),
            Ok(AttachmentOutcome::Content(_))
        ));
    }

    #[test]
    fn attachment_bytes_round_trip_across_pages() {
        let binary: Vec<u8> = (0..1000u32).map(|n| (n * 7 % 256) as u8).collect();
        let raw = nested_message(&binary);
        for page in [1usize, 7, 64, 333, 1000, 5000] {
            let mut collected = Vec::new();
            let mut offset = 0usize;
            loop {
                let read = read_ok(
                    &raw,
                    "1.3.2",
                    &AttachmentRequest::Bytes {
                        offset_bytes: offset,
                        max_bytes: page,
                    },
                );
                assert_eq!(read.size_bytes, 1000);
                let AttachmentPayload::Bytes {
                    bytes,
                    offset_bytes,
                    has_more,
                } = read.payload
                else {
                    panic!("expected bytes");
                };
                assert_eq!(offset_bytes, offset);
                assert!(bytes.len() <= page);
                offset += bytes.len();
                collected.extend(bytes);
                if !has_more {
                    break;
                }
            }
            assert_eq!(collected, binary, "page size {page}");
        }
    }

    #[test]
    fn attachment_byte_offsets_at_and_beyond_end() {
        let raw = nested_message(&[1, 2, 3, 4]);
        let read = read_ok(
            &raw,
            "1.3.2",
            &AttachmentRequest::Bytes {
                offset_bytes: 4,
                max_bytes: 10,
            },
        );
        assert_eq!(
            read.payload,
            AttachmentPayload::Bytes {
                bytes: Vec::new(),
                offset_bytes: 4,
                has_more: false
            }
        );
        for request in [
            AttachmentRequest::Bytes {
                offset_bytes: 5,
                max_bytes: 10,
            },
            AttachmentRequest::Bytes {
                offset_bytes: usize::MAX,
                max_bytes: 1,
            },
        ] {
            assert!(matches!(
                read_attachment(raw.as_bytes(), "1.3.2", &request, &test_limits(100)),
                Err(AppError::InvalidInput(_))
            ));
        }
        let read = read_ok(
            &raw,
            "1.3.2",
            &AttachmentRequest::Bytes {
                offset_bytes: 3,
                max_bytes: usize::MAX,
            },
        );
        assert_eq!(
            read.payload,
            AttachmentPayload::Bytes {
                bytes: vec![4],
                offset_bytes: 3,
                has_more: false
            }
        );
        let read = read_ok(
            &raw,
            "1.3.2",
            &AttachmentRequest::Bytes {
                offset_bytes: 1,
                max_bytes: 0,
            },
        );
        assert_eq!(
            read.payload,
            AttachmentPayload::Bytes {
                bytes: Vec::new(),
                offset_bytes: 1,
                has_more: true
            }
        );
    }

    #[test]
    fn attachment_partial_decode_is_limit_exceeded_in_both_modes() {
        let raw = nested_message(&[9u8; 100]);
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 99;
        for request in [TEXT_REQUEST, all_bytes(0)] {
            assert_eq!(
                read_attachment(raw.as_bytes(), "1.3.2", &request, &limits).expect("reads"),
                AttachmentOutcome::LimitExceeded(ProcessingLimit::DecodeBudgetBytes)
            );
        }
        limits.decode_budget_bytes = 100;
        assert!(matches!(
            read_attachment(raw.as_bytes(), "1.3.2", &all_bytes(0), &limits),
            Ok(AttachmentOutcome::Content(_))
        ));
    }

    #[test]
    fn attachment_text_charset_output_is_checked_against_remaining_budget() {
        let raw = single_attachment("text/plain; charset=utf-8", "8bit", "0123456789");
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 19;
        assert_eq!(
            read_attachment(raw.as_bytes(), "1.2", &TEXT_REQUEST, &limits).expect("reads"),
            AttachmentOutcome::LimitExceeded(ProcessingLimit::DecodeBudgetBytes)
        );
        limits.decode_budget_bytes = 20;
        assert!(matches!(
            read_attachment(raw.as_bytes(), "1.2", &TEXT_REQUEST, &limits),
            Ok(AttachmentOutcome::Content(_))
        ));
    }

    #[test]
    fn attachment_reports_message_processing_limits_first() {
        let raw = nested_message(b"x");
        let mut limits = test_limits(100);
        limits.max_depth = 1;
        assert_eq!(
            read_attachment(raw.as_bytes(), "1.3.2", &all_bytes(0), &limits).expect("reads"),
            AttachmentOutcome::LimitExceeded(ProcessingLimit::MaxDepth)
        );
        limits.max_depth = 64;
        limits.max_parts = 3;
        assert_eq!(
            read_attachment(raw.as_bytes(), "1.3.2", &all_bytes(0), &limits).expect("reads"),
            AttachmentOutcome::LimitExceeded(ProcessingLimit::MaxParts)
        );
    }

    #[test]
    fn attachment_rejects_bad_part_ids() {
        let raw = nested_message(b"x");
        let long = format!("1{}", ".1".repeat(32));
        assert_eq!(long.chars().count(), 65);
        let overflow = format!("1.{}", "9".repeat(30));
        for id in [
            "",
            "0",
            "1.0",
            "1..2",
            "-1",
            "+1",
            "1.",
            ".1",
            "2",
            "1.+1",
            "1.-1",
            "1.01",
            "01",
            "1.a",
            "1 .2",
            "1.2.",
            long.as_str(),
            overflow.as_str(),
        ] {
            assert!(
                matches!(
                    read_attachment(raw.as_bytes(), id, &all_bytes(0), &test_limits(100)),
                    Err(AppError::InvalidInput(_))
                ),
                "{id:?}"
            );
        }
        let max_len = format!("1{}", ".1".repeat(31));
        assert_eq!(max_len.chars().count(), 63);
        assert!(matches!(
            read_attachment(raw.as_bytes(), &max_len, &all_bytes(0), &test_limits(100)),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn attachment_rejects_multipart_and_body_targets_and_reports_missing() {
        let raw = nested_message(b"x");
        for id in ["1", "1.1", "1.3", "1.1.1", "1.1.2"] {
            assert!(
                matches!(
                    read_attachment(raw.as_bytes(), id, &all_bytes(0), &test_limits(100)),
                    Err(AppError::InvalidInput(_))
                ),
                "{id}"
            );
        }
        for id in ["1.4", "1.3.3", "1.2.1", "1.99999999999999999"] {
            assert!(
                matches!(
                    read_attachment(raw.as_bytes(), id, &all_bytes(0), &test_limits(100)),
                    Err(AppError::NotFound(_))
                ),
                "{id}"
            );
        }
    }

    #[test]
    fn attachment_single_part_message_root_is_addressable() {
        let raw = "Subject: s\r\nContent-Type: text/plain; name=\"n.txt\"\r\n\r\nhello";
        let read = read_ok(raw, "1", &TEXT_REQUEST);
        assert_eq!(text_of(&read), ("hello", false));
        let plain = "Subject: s\r\nContent-Type: text/plain\r\n\r\nhello";
        assert!(matches!(
            read_attachment(plain.as_bytes(), "1", &TEXT_REQUEST, &test_limits(100)),
            Err(AppError::InvalidInput(_))
        ));
    }
}
