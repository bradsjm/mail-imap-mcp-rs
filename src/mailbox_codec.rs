//! IMAP mailbox name codec helpers.
//!
//! IMAP servers commonly return mailbox names in modified UTF-7 (RFC 3501
//! section 5.1.3), while `async-imap` forwards those names as raw strings.
//! This module normalizes names for MCP responses and re-encodes user-facing
//! mailbox names before issuing IMAP commands.

use base64::Engine;

/// Decode an IMAP modified UTF-7 mailbox name for user-facing output.
///
/// Falls back to the original input if decoding fails on malformed data.
pub fn decode_mailbox_name_for_display(mailbox: &str) -> String {
    decode_mailbox_name_inner(mailbox).unwrap_or_else(|| mailbox.to_owned())
}

/// Encode a mailbox name for IMAP commands.
///
/// Accepts both decoded user-facing mailbox names and legacy canonical
/// modified UTF-7 names from older server responses.
pub fn encode_mailbox_name_for_command(mailbox: &str) -> String {
    if is_canonical_modified_utf7(mailbox) {
        return mailbox.to_owned();
    }
    utf7_imap::encode_utf7_imap(mailbox.to_owned())
}

/// Normalize mailbox identity for equality checks.
///
/// This treats canonical modified UTF-7 and decoded Unicode mailbox names as
/// equivalent while preserving malformed input as-is.
pub fn normalize_mailbox_name(mailbox: &str) -> String {
    decode_mailbox_name_inner(mailbox).unwrap_or_else(|| mailbox.to_owned())
}

fn decode_mailbox_name_inner(mailbox: &str) -> Option<String> {
    let mut decoded = String::with_capacity(mailbox.len());
    let mut rest = mailbox;

    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let segment = &rest[start..];
        let end = segment.find('-')?;
        let token = &segment[..=end];
        decoded.push_str(&decode_modified_utf7_token(token)?);
        rest = &segment[end + 1..];
    }

    decoded.push_str(rest);
    Some(decoded)
}

fn decode_modified_utf7_token(token: &str) -> Option<String> {
    if token == "&-" {
        return Some("&".to_owned());
    }

    let payload = token.strip_prefix('&')?.strip_suffix('-')?;
    let mut base64_text = payload.replace(',', "/");
    while base64_text.len() % 4 != 0 {
        base64_text.push('=');
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_text)
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }

    let utf16 = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&utf16).ok()
}

fn is_canonical_modified_utf7(mailbox: &str) -> bool {
    let Some(decoded) = decode_mailbox_name_inner(mailbox) else {
        return false;
    };
    utf7_imap::encode_utf7_imap(decoded) == mailbox
}

#[cfg(test)]
mod tests {
    use super::{
        decode_mailbox_name_for_display, encode_mailbox_name_for_command, normalize_mailbox_name,
    };

    #[test]
    fn decodes_modified_utf7_mailbox_names() {
        assert_eq!(decode_mailbox_name_for_display("&ZeVnLIqe-"), "日本語");
        assert_eq!(decode_mailbox_name_for_display("th&AOkA4g-tre"), "théâtre");
    }

    #[test]
    fn preserves_plain_ascii_mailbox_names() {
        assert_eq!(
            decode_mailbox_name_for_display("INBOX/Receipts"),
            "INBOX/Receipts"
        );
        assert_eq!(
            encode_mailbox_name_for_command("INBOX/Receipts"),
            "INBOX/Receipts"
        );
    }

    #[test]
    fn round_trips_unicode_mailbox_names() {
        let mailbox = "旅行/日本語 & Stuff";
        let encoded = encode_mailbox_name_for_command(mailbox);
        assert_ne!(encoded, mailbox);
        assert!(encoded.contains('&'));
        assert_eq!(decode_mailbox_name_for_display(&encoded), mailbox);
    }

    #[test]
    fn preserves_legacy_encoded_mailbox_names_for_commands() {
        let encoded = "&ZeVnLIqe-";
        assert_eq!(encode_mailbox_name_for_command(encoded), encoded);
    }

    #[test]
    fn normalizes_encoded_and_decoded_mailbox_names_equally() {
        assert_eq!(normalize_mailbox_name("&ZeVnLIqe-"), "日本語");
        assert_eq!(normalize_mailbox_name("日本語"), "日本語");
    }

    #[test]
    fn falls_back_for_malformed_encoded_mailbox_names() {
        assert_eq!(decode_mailbox_name_for_display("&invalid-"), "&invalid-");
        assert_eq!(encode_mailbox_name_for_command("&invalid-"), "&-invalid-");
        assert_eq!(normalize_mailbox_name("&invalid-"), "&invalid-");
    }
}
