//! Provider-aware mailbox metadata mapping.
//!
//! IMAP LIST attributes are the source of truth. Provider behavior is enabled
//! only by advertised capabilities, never by account hostnames.

use async_imap::types::{Capabilities, Capability, Name, NameAttribute};

use crate::mailbox_codec::normalize_mailbox_name;
use crate::models::{MailboxInfo, MailboxRole};

/// Whether the server advertises Gmail's IMAP extension.
pub(crate) fn is_gmail_capable(capabilities: &Capabilities) -> bool {
    capabilities.iter().any(|capability| {
        matches!(capability, Capability::Atom(value) if value.eq_ignore_ascii_case("X-GM-EXT-1"))
    })
}

/// Convert one async-imap LIST response into lossless mailbox metadata.
pub(crate) fn mailbox_info(name: &Name, gmail_capable: bool) -> MailboxInfo {
    mailbox_info_from_parts(
        name.name(),
        name.delimiter(),
        name.attributes(),
        gmail_capable,
    )
}

/// Convert LIST fields into mailbox metadata.
///
/// Kept separate from [`mailbox_info`] so exact-mailbox checks and unit tests
/// share the same classification rules.
pub(crate) fn mailbox_info_from_parts(
    name: &str,
    delimiter: Option<&str>,
    attributes: &[NameAttribute<'_>],
    gmail_capable: bool,
) -> MailboxInfo {
    let attributes = attributes
        .iter()
        .map(attribute_to_string)
        .collect::<Vec<_>>();
    let normalized_name = normalize_mailbox_name(name);
    let role = mailbox_role(&normalized_name, attributes.as_slice());
    let selectable = !attributes
        .iter()
        .any(|attribute| attribute.eq_ignore_ascii_case("\\NoSelect"));
    let provider_managed =
        gmail_capable && (role.is_some() || normalized_name.eq_ignore_ascii_case("[Gmail]"));

    MailboxInfo {
        name: normalized_name,
        delimiter: delimiter.map(str::to_owned),
        attributes,
        role,
        selectable,
        provider_managed,
    }
}

fn attribute_to_string(attribute: &NameAttribute<'_>) -> String {
    match attribute {
        NameAttribute::NoInferiors => "\\NoInferiors".to_owned(),
        NameAttribute::NoSelect => "\\NoSelect".to_owned(),
        NameAttribute::Marked => "\\Marked".to_owned(),
        NameAttribute::Unmarked => "\\Unmarked".to_owned(),
        NameAttribute::All => "\\All".to_owned(),
        NameAttribute::Archive => "\\Archive".to_owned(),
        NameAttribute::Drafts => "\\Drafts".to_owned(),
        NameAttribute::Flagged => "\\Flagged".to_owned(),
        NameAttribute::Junk => "\\Junk".to_owned(),
        NameAttribute::Sent => "\\Sent".to_owned(),
        NameAttribute::Trash => "\\Trash".to_owned(),
        NameAttribute::Extension(value) => value.to_string(),
        _ => format!("{attribute:?}"),
    }
}

fn mailbox_role(name: &str, attributes: &[String]) -> Option<MailboxRole> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some(MailboxRole::Inbox);
    }

    attributes.iter().find_map(|attribute| {
        if attribute.eq_ignore_ascii_case("\\All") {
            Some(MailboxRole::All)
        } else if attribute.eq_ignore_ascii_case("\\Archive") {
            Some(MailboxRole::Archive)
        } else if attribute.eq_ignore_ascii_case("\\Drafts") {
            Some(MailboxRole::Drafts)
        } else if attribute.eq_ignore_ascii_case("\\Flagged") {
            Some(MailboxRole::Flagged)
        } else if attribute.eq_ignore_ascii_case("\\Important") {
            Some(MailboxRole::Important)
        } else if attribute.eq_ignore_ascii_case("\\Junk") {
            Some(MailboxRole::Junk)
        } else if attribute.eq_ignore_ascii_case("\\Sent") {
            Some(MailboxRole::Sent)
        } else if attribute.eq_ignore_ascii_case("\\Trash") {
            Some(MailboxRole::Trash)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use async_imap::types::NameAttribute;

    use super::{mailbox_info_from_parts, mailbox_role};
    use crate::models::MailboxRole;

    #[test]
    fn preserves_unknown_attributes_and_normalizes_inbox() {
        let attributes = [
            NameAttribute::NoSelect,
            NameAttribute::Extension("\\X-Provider".into()),
        ];
        let mailbox = mailbox_info_from_parts("inbox", Some("/"), &attributes, true);

        assert_eq!(mailbox.name, "inbox");
        assert_eq!(mailbox.role, Some(MailboxRole::Inbox));
        assert!(!mailbox.selectable);
        assert!(mailbox.provider_managed);
        assert_eq!(mailbox.attributes, ["\\NoSelect", "\\X-Provider"]);
    }

    #[test]
    fn maps_gmail_important_extension_without_dropping_it() {
        let attributes = [NameAttribute::Extension("\\Important".into())];
        let mailbox = mailbox_info_from_parts("[Gmail]/Important", Some("/"), &attributes, true);

        assert_eq!(mailbox.role, Some(MailboxRole::Important));
        assert!(mailbox.provider_managed);
        assert_eq!(mailbox.attributes, ["\\Important"]);
    }

    #[test]
    fn non_gmail_special_use_mailboxes_are_not_provider_managed() {
        let attributes = [NameAttribute::Trash];
        let mailbox = mailbox_info_from_parts("Deleted", Some("/"), &attributes, false);

        assert_eq!(mailbox.role, Some(MailboxRole::Trash));
        assert!(!mailbox.provider_managed);
    }

    #[test]
    fn mailbox_role_prioritizes_inbox_over_conflicting_attributes() {
        assert_eq!(
            mailbox_role("INBOX", &["\\Trash".to_owned()]),
            Some(MailboxRole::Inbox)
        );
    }
}
