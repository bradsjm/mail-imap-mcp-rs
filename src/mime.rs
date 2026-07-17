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

/// Resource ceilings shared by full-message and section-based MIME parsing.
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

/// A fetched attachment section and the metadata needed to process it.
#[derive(Debug, Clone)]
pub struct FetchedPart {
    pub plan: PartPlan,
    pub bytes: Vec<u8>,
    /// Whether the fetched bytes contain the complete transfer-encoded payload.
    pub complete: bool,
}

/// Bounded message sections fetched before synchronous MIME processing.
#[derive(Debug, Clone, Copy)]
pub struct MessageSections<'a> {
    pub header_bytes: &'a [u8],
    pub text_bytes: Option<&'a [u8]>,
    pub html_bytes: Option<&'a [u8]>,
    pub attachment_sections: &'a [FetchedPart],
}

/// The role a MIME part plays in a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Text,
    Html,
    Attachment,
    Inline,
    MultipartWrapper,
}

/// A fetchable MIME section and the metadata needed to process it safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartPlan {
    /// IMAP section path, such as `1`, `1.2`, or `2`.
    pub path: String,
    pub kind: PartKind,
    pub content_type: String,
    pub encoding: String,
    /// MIME charset parameter used to decode text parts.
    pub charset: Option<String>,
    pub declared_octets: Option<u32>,
    pub filename: Option<String>,
    pub disposition: Option<String>,
}

/// The bounded set of MIME sections selected for a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFetchPlan {
    pub parts: Vec<PartPlan>,
    pub text_part: Option<String>,
    pub html_part: Option<String>,
    pub attachment_parts: Vec<PartPlan>,
    pub has_bodystructure: bool,
    /// Whether planning stopped at the caller's part ceiling.
    pub parts_truncated: bool,
    /// Whether planning stopped at the caller's nesting-depth ceiling.
    pub depth_truncated: bool,
}

/// Build a fetch plan from an IMAP BODYSTRUCTURE response.
///
/// Multipart containers are recorded as wrappers while their children receive
/// the numbered IMAP section paths used for content fetches.
pub fn plan_from_bodystructure(
    bs: &async_imap::imap_proto::BodyStructure<'_>,
    max_depth: usize,
    max_parts: usize,
) -> MessageFetchPlan {
    let mut plan = MessageFetchPlan {
        parts: Vec::new(),
        text_part: None,
        html_part: None,
        attachment_parts: Vec::new(),
        has_bodystructure: true,
        parts_truncated: false,
        depth_truncated: false,
    };
    walk_bodystructure(bs, None, 0, max_depth, max_parts, &mut plan);
    plan
}

#[cfg(test)]
/// Build the conservative fallback plan used when BODYSTRUCTURE is absent.
///
/// An empty path denotes `BODY.PEEK[]`; callers must use their configured
/// partial-fetch budget rather than requesting an unbounded message body.
pub fn plan_from_raw_headers(raw: &[u8]) -> MessageFetchPlan {
    let headers = parse_header_bytes(raw).unwrap_or_default();
    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map_or_else(
            || "text/plain".to_owned(),
            |(_, value)| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("text/plain")
                    .trim()
                    .to_ascii_lowercase()
            },
        );
    let kind = part_kind(&content_type, None, None);
    let part = PartPlan {
        path: String::new(),
        kind,
        content_type,
        encoding: "unknown".to_owned(),
        charset: content_type_parameter(&headers, "charset"),
        declared_octets: None,
        filename: None,
        disposition: None,
    };
    MessageFetchPlan {
        text_part: if part.kind == PartKind::Text {
            Some(part.path.clone())
        } else {
            None
        },
        html_part: if part.kind == PartKind::Html {
            Some(part.path.clone())
        } else {
            None
        },
        attachment_parts: if part.kind == PartKind::Attachment {
            vec![part.clone()]
        } else {
            Vec::new()
        },
        parts: vec![part],
        has_bodystructure: false,
        parts_truncated: false,
        depth_truncated: false,
    }
}

#[cfg(test)]
fn content_type_parameter(headers: &[(String, String)], wanted: &str) -> Option<String> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))?
        .1
        .as_str();
    value.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case(wanted)
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn walk_bodystructure(
    body: &async_imap::imap_proto::BodyStructure<'_>,
    path: Option<String>,
    depth: usize,
    max_depth: usize,
    max_parts: usize,
    plan: &mut MessageFetchPlan,
) {
    if depth > max_depth {
        plan.depth_truncated = true;
        return;
    }
    if plan.parts.len() >= max_parts {
        plan.parts_truncated = true;
        return;
    }

    match body {
        async_imap::imap_proto::BodyStructure::Multipart { common, bodies, .. } => {
            if let Some(path) = &path {
                push_wrapper(plan, path.clone(), common);
                if plan.parts.len() >= max_parts && !bodies.is_empty() {
                    plan.parts_truncated = true;
                    return;
                }
            }
            for (index, child) in bodies.iter().enumerate() {
                if plan.parts.len() >= max_parts {
                    plan.parts_truncated = true;
                    break;
                }
                let child_path = match &path {
                    Some(parent) => format!("{parent}.{}", index + 1),
                    None => (index + 1).to_string(),
                };
                walk_bodystructure(
                    child,
                    Some(child_path),
                    depth.saturating_add(1),
                    max_depth,
                    max_parts,
                    plan,
                );
            }
        }
        async_imap::imap_proto::BodyStructure::Message {
            common,
            other,
            body,
            ..
        } => {
            let path = path.unwrap_or_else(|| "1".to_owned());
            push_wrapper(plan, path.clone(), common);
            if plan.parts.len() < max_parts {
                walk_bodystructure(
                    body,
                    Some(format!("{path}.1")),
                    depth.saturating_add(1),
                    max_depth,
                    max_parts,
                    plan,
                );
            } else {
                plan.parts_truncated = true;
            }
            let _ = other;
        }
        async_imap::imap_proto::BodyStructure::Basic { common, other, .. }
        | async_imap::imap_proto::BodyStructure::Text { common, other, .. } => {
            let path = path.unwrap_or_else(|| "1".to_owned());
            let content_type = content_type(common);
            let disposition = common
                .disposition
                .as_ref()
                .map(|value| value.ty.to_string());
            let filename = filename(common);
            let kind = part_kind(&content_type, disposition.as_deref(), filename.as_deref());
            let part = PartPlan {
                path: path.clone(),
                kind,
                content_type,
                encoding: encoding_name(&other.transfer_encoding),
                charset: content_type_param(common, "charset"),
                declared_octets: Some(other.octets),
                filename,
                disposition,
            };
            if kind == PartKind::Text && plan.text_part.is_none() {
                plan.text_part = Some(path);
            } else if kind == PartKind::Html && plan.html_part.is_none() {
                plan.html_part = Some(path);
            }
            if kind == PartKind::Attachment
                || part.disposition.as_deref().is_some_and(|value| {
                    value.eq_ignore_ascii_case("inline") && part.filename.is_some()
                })
            {
                plan.attachment_parts.push(part.clone());
            }
            plan.parts.push(part);
        }
    }
}

fn push_wrapper(
    plan: &mut MessageFetchPlan,
    path: String,
    common: &async_imap::imap_proto::BodyContentCommon<'_>,
) {
    plan.parts.push(PartPlan {
        path,
        kind: PartKind::MultipartWrapper,
        content_type: content_type(common),
        encoding: "multipart".to_owned(),
        charset: content_type_param(common, "charset"),
        declared_octets: None,
        filename: filename(common),
        disposition: common
            .disposition
            .as_ref()
            .map(|value| value.ty.to_string()),
    });
}

fn content_type(common: &async_imap::imap_proto::BodyContentCommon<'_>) -> String {
    format!("{}/{}", common.ty.ty, common.ty.subtype).to_ascii_lowercase()
}

fn content_type_param(
    common: &async_imap::imap_proto::BodyContentCommon<'_>,
    wanted: &str,
) -> Option<String> {
    common.ty.params.as_ref().and_then(|params| {
        params
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.to_string())
    })
}

fn filename(common: &async_imap::imap_proto::BodyContentCommon<'_>) -> Option<String> {
    common
        .disposition
        .as_ref()
        .and_then(|disposition| disposition.params.as_ref())
        .and_then(|params| {
            params
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("filename"))
                .map(|(_, value)| value.to_string())
        })
        .or_else(|| content_type_param(common, "name"))
}

fn part_kind(content_type: &str, disposition: Option<&str>, filename: Option<&str>) -> PartKind {
    let inline = disposition.is_some_and(|value| value.eq_ignore_ascii_case("inline"));
    if !inline
        && (disposition.is_some_and(|value| value.eq_ignore_ascii_case("attachment"))
            || filename.is_some())
    {
        PartKind::Attachment
    } else if content_type.eq_ignore_ascii_case("text/plain") {
        PartKind::Text
    } else if content_type.eq_ignore_ascii_case("text/html") {
        PartKind::Html
    } else if inline {
        PartKind::Inline
    } else {
        PartKind::Attachment
    }
}

fn encoding_name(encoding: &async_imap::imap_proto::ContentEncoding<'_>) -> String {
    match encoding {
        async_imap::imap_proto::ContentEncoding::SevenBit => "7bit".to_owned(),
        async_imap::imap_proto::ContentEncoding::EightBit => "8bit".to_owned(),
        async_imap::imap_proto::ContentEncoding::Binary => "binary".to_owned(),
        async_imap::imap_proto::ContentEncoding::Base64 => "base64".to_owned(),
        async_imap::imap_proto::ContentEncoding::QuotedPrintable => "quoted-printable".to_owned(),
        async_imap::imap_proto::ContentEncoding::Other(value) => value.to_ascii_lowercase(),
    }
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
    let parsed = mailparse::parse_mail(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse RFC822 message: {e}")))?;

    let headers = parse_all_headers(raw)?;
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

/// Assemble a parsed message from bounded IMAP body sections.
/// Header bytes may be empty when the IMAP parser cannot safely retrieve them;
/// body sections and BODYSTRUCTURE metadata are still assembled independently.
pub fn parse_from_sections(
    sections: MessageSections<'_>,
    plan: &MessageFetchPlan,
    body_max_chars: usize,
    body_mode: BodyMode,
    attachment_mode: AttachmentMode,
    limits: &ProcessingLimits,
) -> AppResult<ParsedMessage> {
    let MessageSections {
        header_bytes,
        text_bytes,
        html_bytes,
        attachment_sections,
    } = sections;
    let headers = if header_bytes.is_empty() {
        Vec::new()
    } else {
        parse_all_headers(header_bytes)?
    };
    let include_text = matches!(body_mode, BodyMode::Text | BodyMode::Both);
    let include_html = matches!(body_mode, BodyMode::Html | BodyMode::Both);
    let mut processing_limits = Vec::new();
    if plan.parts_truncated {
        processing_limits.push(ProcessingLimit::MaxParts);
    }
    if plan.depth_truncated {
        processing_limits.push(ProcessingLimit::MaxDepth);
    }
    let mut decoded_bytes = 0;

    let decoded_text = decode_section_bounded(
        text_bytes,
        plan.text_part.as_deref(),
        plan,
        &mut decoded_bytes,
        limits.decode_budget_bytes,
        &mut processing_limits,
    )?
    .map(|(bytes, charset)| decode_charset(&bytes, charset.as_deref()))
    .transpose()?;
    let sanitized_html = decode_section_bounded(
        html_bytes,
        plan.html_part.as_deref(),
        plan,
        &mut decoded_bytes,
        limits.decode_budget_bytes,
        &mut processing_limits,
    )?
    .map(|(bytes, charset)| {
        decode_charset(&bytes, charset.as_deref()).map(|html| ammonia::clean(&html))
    })
    .transpose()?;

    let body_text = include_text
        .then(|| select_body_text(decoded_text, sanitized_html.as_deref()))
        .flatten()
        .map(|text| truncate_chars(text, body_max_chars));
    let body_html_sanitized = include_html
        .then_some(sanitized_html)
        .flatten()
        .map(|html| truncate_chars(html, body_max_chars));

    let mut attachments = Vec::new();
    let mut attachments_truncated = false;
    let mut attachment_decoded_bytes = 0usize;
    if attachment_mode != AttachmentMode::None {
        for part in &plan.attachment_parts {
            if attachments.len() >= MAX_ATTACHMENTS {
                attachments_truncated = true;
                break;
            }
            let payload = attachment_sections
                .iter()
                .find(|fetched| fetched.plan.path == part.path);
            let mut extracted_text = None;
            // BODYSTRUCTURE octets describe transfer-encoded bytes, not the
            // decoded attachment size exposed by AttachmentInfo.
            let mut size_bytes = None;
            if let Some(payload) = payload {
                let remaining = limits.decode_budget_bytes.saturating_sub(decoded_bytes);
                let (decoded, decode_partial) =
                    decode_transfer_encoded_bounded(&payload.bytes, &part.encoding, remaining)?;
                if payload.complete && !decode_partial {
                    size_bytes = Some(decoded.len());
                }
                if decode_partial {
                    push_limit(&mut processing_limits, ProcessingLimit::DecodeBudgetBytes);
                }
                decoded_bytes += decoded.len();
                let extract_pdf = attachment_mode == AttachmentMode::ExtractText
                    && part.content_type.eq_ignore_ascii_case("application/pdf");
                if extract_pdf {
                    let extract_remaining = limits
                        .attachment_extract_budget_bytes
                        .saturating_sub(attachment_decoded_bytes);
                    if decoded.len() > extract_remaining {
                        push_limit(
                            &mut processing_limits,
                            ProcessingLimit::AttachmentExtractBudgetBytes,
                        );
                    } else if !decode_partial {
                        attachment_decoded_bytes += decoded.len();
                        if let Ok(text) = pdf_extract::extract_text_from_mem(&decoded) {
                            extracted_text =
                                Some(truncate_chars(text, limits.attachment_text_max_chars));
                        }
                    }
                }
            }
            attachments.push(AttachmentInfo {
                filename: part.filename.clone(),
                content_type: part.content_type.clone(),
                size_bytes,
                part_id: part.path.clone(),
                extracted_text,
            });
        }
    }

    let header_map = to_header_map(&headers);
    Ok(ParsedMessage {
        date: header_map.get("date").cloned(),
        from: header_map.get("from").cloned(),
        to: header_map.get("to").cloned(),
        cc: header_map.get("cc").cloned(),
        subject: header_map.get("subject").cloned(),
        headers_all: headers,
        body_text,
        body_html_sanitized,
        attachments,
        attachments_truncated,
        processing_limits,
    })
}

fn decode_section_bounded(
    bytes: Option<&[u8]>,
    path: Option<&str>,
    plan: &MessageFetchPlan,
    decoded_bytes: &mut usize,
    budget: usize,
    processing_limits: &mut Vec<ProcessingLimit>,
) -> AppResult<Option<(Vec<u8>, Option<String>)>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let part = path.and_then(|path| plan.parts.iter().find(|part| part.path == path));
    let encoding = part.map_or("unknown", |part| part.encoding.as_str());
    let remaining = budget.saturating_sub(*decoded_bytes);
    let (decoded, partial) = decode_transfer_encoded_bounded(bytes, encoding, remaining)?;
    if partial {
        push_limit(processing_limits, ProcessingLimit::DecodeBudgetBytes);
    }
    if partial && decoded.is_empty() && !bytes.is_empty() {
        return Ok(None);
    }
    *decoded_bytes = decoded_bytes.saturating_add(decoded.len());
    Ok(Some((decoded, part.and_then(|part| part.charset.clone()))))
}

fn decode_charset(bytes: &[u8], charset: Option<&str>) -> AppResult<String> {
    let Some(charset) = charset.filter(|value| !value.eq_ignore_ascii_case("utf-8")) else {
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    };
    let mut message = format!(
        "Content-Type: text/plain; charset=\"{charset}\"\r\nContent-Transfer-Encoding: 8bit\r\n\r\n"
    )
    .into_bytes();
    message.extend_from_slice(bytes);
    mailparse::parse_mail(&message)
        .and_then(|part| part.get_body())
        .map_err(|error| AppError::Internal(format!("failed decoding message charset: {error}")))
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

#[cfg(test)]
/// Decode a MIME transfer-encoded body section.
pub fn decode_transfer_encoded(data: &[u8], encoding: &str) -> AppResult<Vec<u8>> {
    decode_transfer_encoded_bounded(data, encoding, usize::MAX).map(|(decoded, _)| decoded)
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
        let body_candidate =
            !explicit_attachment && matches!(ctype.as_str(), "text/plain" | "text/html");

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
    let decoded = part
        .get_body()
        .map_err(|error| AppError::Internal(format!("failed decoding message body: {error}")))?;
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
    use std::borrow::Cow;

    use super::{
        FetchedPart, MAX_ATTACHMENTS, MIME_MAX_PARTS, MessageFetchPlan, MessageSections, PartKind,
        PartPlan, ProcessingLimit, ProcessingLimits, attachment_size_bytes, curated_headers,
        decode_transfer_encoded, decode_transfer_encoded_bounded, parse_from_sections,
        parse_message, plan_from_bodystructure, plan_from_raw_headers, truncate_chars,
    };
    use crate::models::{AttachmentMode, BodyMode};
    use async_imap::imap_proto::{
        BodyContentCommon, BodyContentSinglePart, BodyStructure, ContentDisposition,
        ContentEncoding, ContentType,
    };

    fn test_limits(attachment_text_max_chars: usize) -> ProcessingLimits {
        ProcessingLimits {
            decode_budget_bytes: 10_000_000,
            max_depth: 64,
            max_parts: MIME_MAX_PARTS,
            attachment_extract_budget_bytes: 5_000_000,
            attachment_text_max_chars,
        }
    }

    fn common(
        ty: &'static str,
        subtype: &'static str,
        disposition: Option<(&'static str, Option<&'static str>)>,
    ) -> BodyContentCommon<'static> {
        let disposition = disposition.map(|(ty, filename)| ContentDisposition {
            ty: Cow::Borrowed(ty),
            params: filename
                .map(|filename| vec![(Cow::Borrowed("filename"), Cow::Borrowed(filename))]),
        });
        BodyContentCommon {
            ty: ContentType {
                ty: Cow::Borrowed(ty),
                subtype: Cow::Borrowed(subtype),
                params: None,
            },
            disposition,
            language: None,
            location: None,
        }
    }

    fn text_part(
        ty: &'static str,
        subtype: &'static str,
        disposition: Option<(&'static str, Option<&'static str>)>,
    ) -> BodyStructure<'static> {
        BodyStructure::Text {
            common: common(ty, subtype, disposition),
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::SevenBit,
                octets: 42,
            },
            lines: 1,
            extension: None,
        }
    }

    fn nested_multipart(depth: usize) -> BodyStructure<'static> {
        if depth == 0 {
            return text_part("text", "plain", None);
        }

        BodyStructure::Multipart {
            common: common("multipart", "mixed", None),
            bodies: vec![nested_multipart(depth - 1)],
            extension: None,
        }
    }

    #[test]
    fn plans_multipart_text_html_and_attachment_sections() {
        let bodystructure = BodyStructure::Multipart {
            common: common("multipart", "mixed", None),
            bodies: vec![
                text_part("text", "plain", None),
                text_part("text", "html", None),
                text_part(
                    "application",
                    "pdf",
                    Some(("attachment", Some("report.pdf"))),
                ),
            ],

            extension: None,
        };

        let plan = plan_from_bodystructure(&bodystructure, 64, MIME_MAX_PARTS);

        assert_eq!(plan.parts.len(), 3);
        assert_eq!(plan.parts[0].path, "1");
        assert_eq!(plan.parts[0].kind, PartKind::Text);
        assert_eq!(plan.parts[1].path, "2");
        assert_eq!(plan.parts[1].kind, PartKind::Html);
        assert_eq!(plan.parts[2].path, "3");
        assert_eq!(plan.parts[2].kind, PartKind::Attachment);
        assert_eq!(plan.parts[2].filename.as_deref(), Some("report.pdf"));
        assert_eq!(plan.text_part.as_deref(), Some("1"));
        assert_eq!(plan.html_part.as_deref(), Some("2"));
        assert_eq!(plan.attachment_parts.len(), 1);
    }

    #[test]
    fn bodystructure_carries_charset_and_inline_text_is_body_and_metadata() {
        let mut inline = text_part("text", "plain", Some(("inline", Some("note.txt"))));
        if let BodyStructure::Text { common, .. } = &mut inline {
            common.ty.params = Some(vec![(
                Cow::Borrowed("charset"),
                Cow::Borrowed("iso-8859-1"),
            )]);
        }
        let plan = plan_from_bodystructure(&inline, 64, MIME_MAX_PARTS);

        assert_eq!(plan.parts[0].kind, PartKind::Text);
        assert_eq!(plan.parts[0].charset.as_deref(), Some("iso-8859-1"));
        assert_eq!(plan.text_part.as_deref(), Some("1"));
        assert_eq!(plan.attachment_parts.len(), 1);
    }

    #[test]
    fn bodystructure_plan_stops_at_max_parts() {
        let bodystructure = nested_multipart(MIME_MAX_PARTS + 1);

        let plan = plan_from_bodystructure(&bodystructure, usize::MAX, MIME_MAX_PARTS);

        assert_eq!(plan.parts.len(), MIME_MAX_PARTS);
        assert!(plan.parts_truncated);
    }

    #[test]
    fn bodystructure_plan_reports_depth_limit() {
        let plan = plan_from_bodystructure(&nested_multipart(3), 1, MIME_MAX_PARTS);

        assert!(plan.depth_truncated);
        assert!(!plan.parts_truncated);
        assert_eq!(plan.parts.len(), 1);
    }

    #[test]
    fn plans_single_part_message() {
        let plan = plan_from_bodystructure(&text_part("text", "plain", None), 64, MIME_MAX_PARTS);

        assert_eq!(plan.parts.len(), 1);
        assert_eq!(plan.parts[0].path, "1");
        assert_eq!(plan.parts[0].kind, PartKind::Text);
        assert_eq!(plan.text_part.as_deref(), Some("1"));
        assert!(!plan.parts_truncated);
    }

    #[test]
    fn raw_header_fallback_uses_whole_message_section_marker() {
        let plan = plan_from_raw_headers(b"Content-Type: text/html; charset=utf-8\r\n\r\n");

        assert!(!plan.has_bodystructure);
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(plan.parts[0].path, "");
        assert_eq!(plan.parts[0].kind, PartKind::Html);
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

    fn section_plan(
        parts: Vec<PartPlan>,
        text_part: Option<&str>,
        html_part: Option<&str>,
    ) -> MessageFetchPlan {
        let attachment_parts = parts
            .iter()
            .filter(|part| matches!(part.kind, PartKind::Attachment | PartKind::Inline))
            .cloned()
            .collect();
        MessageFetchPlan {
            parts,
            text_part: text_part.map(str::to_owned),
            html_part: html_part.map(str::to_owned),
            attachment_parts,
            has_bodystructure: true,
            parts_truncated: false,
            depth_truncated: false,
        }
    }

    #[test]
    fn assembles_text_and_html_from_sections() {
        let plan = section_plan(
            vec![
                PartPlan {
                    path: "1".to_owned(),
                    kind: PartKind::Text,
                    content_type: "text/plain".to_owned(),
                    encoding: "7bit".to_owned(),
                    charset: None,
                    declared_octets: Some(10),
                    filename: None,
                    disposition: None,
                },
                PartPlan {
                    path: "2".to_owned(),
                    kind: PartKind::Html,
                    content_type: "text/html".to_owned(),
                    encoding: "7bit".to_owned(),
                    charset: None,
                    declared_octets: Some(20),
                    filename: None,
                    disposition: None,
                },
            ],
            Some("1"),
            Some("2"),
        );

        let parsed = parse_from_sections(
            MessageSections {
                header_bytes: b"",
                text_bytes: Some(b"Plain body"),
                html_bytes: Some(b"<p>HTML <b>body</b><script>alert(1)</script></p>"),
                attachment_sections: &[],
            },
            &plan,
            2000,
            BodyMode::Both,
            AttachmentMode::Metadata,
            &test_limits(10_000),
        )
        .expect("sections should parse");

        assert!(parsed.headers_all.is_empty());
        assert_eq!(parsed.from, None);
        assert_eq!(parsed.subject, None);
        assert_eq!(parsed.body_text.as_deref(), Some("Plain body"));
        assert_eq!(
            parsed.body_html_sanitized.as_deref(),
            Some("<p>HTML <b>body</b></p>")
        );
    }

    #[test]
    fn decodes_base64_section_text() {
        let plan = section_plan(
            vec![PartPlan {
                path: "1".to_owned(),
                kind: PartKind::Text,
                content_type: "text/plain".to_owned(),
                encoding: "base64".to_owned(),
                charset: None,
                declared_octets: Some(12),
                filename: None,
                disposition: None,
            }],
            Some("1"),
            None,
        );
        let parsed = parse_from_sections(
            MessageSections {
                header_bytes: b"Subject: Encoded\r\n\r\n",
                text_bytes: Some(b"SGVsbG8gYmFzZTY0"),
                html_bytes: None,
                attachment_sections: &[],
            },
            &plan,
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(10_000),
        )
        .expect("base64 section must parse");

        assert_eq!(parsed.body_text.as_deref(), Some("Hello base64"));
        assert_eq!(
            decode_transfer_encoded(b"Hello=20quoted=2Dprintable", "quoted-printable")
                .expect("quoted-printable must decode"),
            b"Hello quoted-printable"
        );
        let (decoded, partial) = decode_transfer_encoded_bounded(b"a=\r\n", "quoted-printable", 1)
            .expect("soft line break should not consume decoded-byte budget");
        assert_eq!(decoded, b"a");
        assert!(!partial);
    }

    #[test]
    fn bounded_sections_decode_charset_and_complete_encoding_prefixes() {
        let plan = section_plan(
            vec![PartPlan {
                path: "1".to_owned(),
                kind: PartKind::Text,
                content_type: "text/plain".to_owned(),
                encoding: "quoted-printable".to_owned(),
                charset: Some("iso-8859-1".to_owned()),
                declared_octets: None,
                filename: None,
                disposition: None,
            }],
            Some("1"),
            None,
        );
        let parsed = parse_from_sections(
            MessageSections {
                header_bytes: b"Subject: Prefix\r\n\r\n",
                text_bytes: Some(b"caf=E9="),
                html_bytes: None,
                attachment_sections: &[],
            },
            &plan,
            100,
            BodyMode::Text,
            AttachmentMode::None,
            &test_limits(100),
        )
        .expect("bounded atom must return partial content");

        assert_eq!(parsed.body_text.as_deref(), Some("café"));
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }

    #[test]
    fn assembles_attachment_metadata_from_plan() {
        let attachment = PartPlan {
            path: "3".to_owned(),
            kind: PartKind::Attachment,
            content_type: "application/pdf".to_owned(),
            encoding: "base64".to_owned(),
            charset: None,
            declared_octets: Some(1234),
            filename: Some("report.pdf".to_owned()),
            disposition: Some("attachment".to_owned()),
        };
        let plan = section_plan(vec![attachment.clone()], None, None);

        let parsed = parse_from_sections(
            MessageSections {
                header_bytes: b"",
                text_bytes: None,
                html_bytes: None,
                attachment_sections: &[],
            },
            &plan,
            2000,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &test_limits(10_000),
        )
        .expect("sections should parse");

        assert_eq!(parsed.attachments.len(), 1);
        assert!(parsed.headers_all.is_empty());
        assert_eq!(parsed.subject, None);
        assert_eq!(parsed.attachments[0].filename, attachment.filename);
        assert_eq!(parsed.attachments[0].content_type, attachment.content_type);
        assert_eq!(parsed.attachments[0].size_bytes, None);
        assert_eq!(parsed.attachments[0].part_id, attachment.path);
        assert_eq!(parsed.attachments[0].extracted_text, None);
    }

    #[test]
    fn attachment_section_size_requires_a_complete_fetch() {
        let attachment = PartPlan {
            path: "2".to_owned(),
            kind: PartKind::Attachment,
            content_type: "application/octet-stream".to_owned(),
            encoding: "base64".to_owned(),
            charset: None,
            declared_octets: Some(8),
            filename: Some("hello.bin".to_owned()),
            disposition: Some("attachment".to_owned()),
        };
        let plan = section_plan(vec![attachment.clone()], None, None);
        let parse = |complete| {
            let fetched = [FetchedPart {
                plan: attachment.clone(),
                bytes: b"aGVsbG8=".to_vec(),
                complete,
            }];
            parse_from_sections(
                MessageSections {
                    header_bytes: b"Subject: Attachment\r\n\r\n",
                    text_bytes: None,
                    html_bytes: None,
                    attachment_sections: &fetched,
                },
                &plan,
                100,
                BodyMode::Text,
                AttachmentMode::Metadata,
                &test_limits(100),
            )
            .expect("attachment section should parse")
        };

        assert_eq!(parse(true).attachments[0].size_bytes, Some(5));
        assert_eq!(parse(false).attachments[0].size_bytes, None);
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
    fn attachment_sections_share_decode_and_extraction_budgets() {
        let first = PartPlan {
            path: "1".to_owned(),
            kind: PartKind::Attachment,
            content_type: "application/octet-stream".to_owned(),
            encoding: "7bit".to_owned(),
            charset: None,
            declared_octets: None,
            filename: Some("one.bin".to_owned()),
            disposition: Some("attachment".to_owned()),
        };
        let second = PartPlan {
            path: "2".to_owned(),
            filename: Some("two.bin".to_owned()),
            ..first.clone()
        };
        let plan = section_plan(vec![first.clone(), second.clone()], None, None);
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 5;
        limits.attachment_extract_budget_bytes = 5;
        let sections = vec![
            FetchedPart {
                plan: first,
                bytes: b"abc".to_vec(),
                complete: true,
            },
            FetchedPart {
                plan: second,
                bytes: b"def".to_vec(),
                complete: true,
            },
        ];

        let parsed = parse_from_sections(
            MessageSections {
                header_bytes: b"Subject: Attachments\r\n\r\n",
                text_bytes: None,
                html_bytes: None,
                attachment_sections: &sections,
            },
            &plan,
            100,
            BodyMode::Text,
            AttachmentMode::Metadata,
            &limits,
        )
        .expect("budget hits return a partial message");

        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].size_bytes, Some(3));
        assert_eq!(parsed.attachments[1].size_bytes, None);
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
        assert!(
            !parsed
                .processing_limits
                .contains(&ProcessingLimit::AttachmentExtractBudgetBytes)
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
        limits.max_depth = 0;
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
    fn section_decode_budget_is_shared_by_body_parts() {
        let plan = section_plan(
            vec![
                PartPlan {
                    path: "1".to_owned(),
                    kind: PartKind::Text,
                    content_type: "text/plain".to_owned(),
                    encoding: "7bit".to_owned(),
                    charset: None,
                    declared_octets: None,
                    filename: None,
                    disposition: None,
                },
                PartPlan {
                    path: "2".to_owned(),
                    kind: PartKind::Html,
                    content_type: "text/html".to_owned(),
                    encoding: "7bit".to_owned(),
                    charset: None,
                    declared_octets: None,
                    filename: None,
                    disposition: None,
                },
            ],
            Some("1"),
            Some("2"),
        );
        let mut limits = test_limits(100);
        limits.decode_budget_bytes = 5;

        let parsed = parse_from_sections(
            MessageSections {
                header_bytes: b"Subject: Budget\r\n\r\n",
                text_bytes: Some(b"hello"),
                html_bytes: Some(b"<b>x</b>"),
                attachment_sections: &[],
            },
            &plan,
            100,
            BodyMode::Both,
            AttachmentMode::None,
            &limits,
        )
        .expect("decode budget returns a partial message");

        assert_eq!(parsed.body_text.as_deref(), Some("hello"));
        assert!(parsed.body_html_sanitized.is_none());
        assert!(
            parsed
                .processing_limits
                .contains(&ProcessingLimit::DecodeBudgetBytes)
        );
    }
}
