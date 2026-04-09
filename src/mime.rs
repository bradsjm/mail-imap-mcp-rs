//! Message parsing and MIME handling
//!
//! Parses RFC822 messages using `mailparse`, extracts body text/HTML,
//! and handles attachments. Sanitizes HTML, derives fallback text from HTML,
//! and supports optional PDF text extraction.

use std::collections::BTreeMap;

use mailparse::{DispositionType, MailHeader, ParsedMail};

use crate::errors::{AppError, AppResult};
use crate::models::AttachmentInfo;

/// Maximum attachments collected during MIME parsing.
pub const MAX_ATTACHMENTS: usize = 50;

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
}

struct WalkConfig {
    extract_attachment_text: bool,
    attachment_text_max_chars: usize,
}

struct WalkState {
    body_text: Option<String>,
    body_html: Option<String>,
    attachments: Vec<AttachmentInfo>,
    attachments_truncated: bool,
}

/// Parse RFC822 message into structured representation
///
/// Extracts headers, body text/HTML, and attachment info. Sanitizes
/// HTML and optionally extracts text from PDF attachments.
///
/// # Parameters
///
/// - `raw`: RFC822 message bytes
/// - `body_max_chars`: Maximum characters for body text/HTML (caller truncates)
/// - `include_html`: Whether to include HTML body
/// - `extract_attachment_text`: Whether to extract text from PDFs
/// - `attachment_text_max_chars`: Maximum characters for extracted PDF text
///
/// # Errors
///
/// - `Internal` if `mailparse` fails
pub fn parse_message(
    raw: &[u8],
    body_max_chars: usize,
    include_html: bool,
    extract_attachment_text: bool,
    attachment_text_max_chars: usize,
) -> AppResult<ParsedMessage> {
    let parsed = mailparse::parse_mail(raw)
        .map_err(|e| AppError::Internal(format!("failed to parse RFC822 message: {e}")))?;

    let headers = parse_all_headers(raw)?;
    let mut state = WalkState {
        body_text: None,
        body_html: None,
        attachments: Vec::new(),
        attachments_truncated: false,
    };
    let config = WalkConfig {
        extract_attachment_text,
        attachment_text_max_chars,
    };

    walk_parts(&parsed, "1".to_owned(), &mut state, &config)?;

    let text = select_body_text(state.body_text, state.body_html.as_deref())
        .map(|t| truncate_chars(t, body_max_chars));
    let html = if include_html {
        state.body_html.map(|h| truncate_chars(h, body_max_chars))
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
    })
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
    state: &mut WalkState,
    config: &WalkConfig,
) -> AppResult<()> {
    if part.subparts.is_empty() {
        let ctype = part.ctype.mimetype.to_ascii_lowercase();
        let disp = part.get_content_disposition();
        let filename = attachment_filename(part, &disp.params);
        let is_attachment = disp.disposition == DispositionType::Attachment || filename.is_some();

        if !is_attachment {
            if ctype == "text/plain"
                && state.body_text.is_none()
                && let Ok(text) = part.get_body()
            {
                state.body_text = Some(text);
            }

            if ctype == "text/html"
                && state.body_html.is_none()
                && let Ok(html) = part.get_body()
            {
                state.body_html = Some(ammonia::clean(&html));
            }
        }

        if is_attachment {
            if state.attachments.len() >= MAX_ATTACHMENTS {
                state.attachments_truncated = true;
                return Ok(());
            }
            let raw_body = part
                .get_body_raw()
                .map_err(|e| AppError::Internal(format!("failed decoding attachment body: {e}")))?;
            let mut extracted_text = None;
            if config.extract_attachment_text
                && ctype == "application/pdf"
                && raw_body.len() <= 5_000_000
                && let Ok(text) = pdf_extract::extract_text_from_mem(&raw_body)
            {
                extracted_text = Some(truncate_chars(text, config.attachment_text_max_chars));
            }

            state.attachments.push(AttachmentInfo {
                filename,
                content_type: ctype,
                size_bytes: raw_body.len(),
                part_id,
                extracted_text,
            });
        }

        return Ok(());
    }

    for (idx, sub) in part.subparts.iter().enumerate() {
        let next_id = format!("{part_id}.{}", idx + 1);
        walk_parts(sub, next_id, state, config)?;
    }
    Ok(())
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
    use super::{MAX_ATTACHMENTS, curated_headers, parse_message, truncate_chars};

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
        let parsed = parse_message(raw, 2000, false, false, 10000).expect("parse should succeed");

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

        let parsed = parse_message(raw, 2000, false, false, 10000).expect("parse should succeed");

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

        let parsed = parse_message(raw, 2000, true, false, 10000).expect("parse should succeed");

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

        let parsed = parse_message(raw, 2000, true, false, 10000).expect("parse should succeed");

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

        let parsed = parse_message(raw, 2000, false, false, 10000).expect("parse should succeed");

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

        let parsed =
            parse_message(raw.as_bytes(), 2000, false, false, 10000).expect("parse should succeed");
        assert_eq!(parsed.body_text.as_deref(), Some("hello"));
        assert_eq!(parsed.attachments.len(), MAX_ATTACHMENTS);
        assert!(parsed.attachments_truncated);
    }
}
