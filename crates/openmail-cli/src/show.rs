//! `show <thread-id>`: every message in a thread, parsed from its raw form.

use std::fmt::Write as _;

use anyhow::Context as _;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use openmail_core::{Address, ThreadId};
use openmail_gmail::GmailClient;
use openmail_gmail::api::{MessageFormat, ThreadFormat};
use openmail_mime::{ParsedMessage, ParsedPart};
use time::UtcOffset;

use crate::text::format_date;

/// How many message requests run at once.
const CONCURRENCY: usize = 4;

/// Prints each message in the thread, oldest first.
pub async fn run(client: &GmailClient, thread_id: &str, offset: UtcOffset) -> anyhow::Result<()> {
    let thread = client
        .get_thread(&ThreadId::new(thread_id), ThreadFormat::Minimal, &[])
        .await
        .with_context(|| format!("could not fetch thread {thread_id}"))?;

    let messages: Vec<ParsedMessage> = stream::iter(thread.messages)
        .map(|message| {
            let client = client.clone();
            async move {
                let id = message.id;
                let raw = client
                    .get_message(&id, MessageFormat::Raw, &[])
                    .await
                    .with_context(|| format!("could not fetch message {id}"))?;
                let raw = raw
                    .raw
                    .with_context(|| format!("message {id} came back without its raw form"))?;
                ParsedMessage::parse_base64url(&raw)
                    .with_context(|| format!("could not parse message {id}"))
            }
        })
        .buffered(CONCURRENCY)
        .try_collect()
        .await?;

    print!("{}", format_thread(thread_id, &messages, offset));
    Ok(())
}

/// The thread's messages, each under a numbered rule.
pub fn format_thread(thread_id: &str, messages: &[ParsedMessage], offset: UtcOffset) -> String {
    let count = messages.len();
    let mut out = format!(
        "Thread {thread_id} ({count} message{})\n",
        if count == 1 { "" } else { "s" }
    );
    for (i, message) in messages.iter().enumerate() {
        let title = format!("── Message {} of {count} ", i + 1);
        let _ = writeln!(out, "\n{title:─<72}");
        out.push_str(&format_message(message, offset));
    }
    out
}

/// One message: its headers, a blank line, the text body (or text converted
/// from the HTML body), then the attachment names.
pub fn format_message(message: &ParsedMessage, offset: UtcOffset) -> String {
    let headers = &message.headers;
    let mut out = String::new();
    let mut header = |name: &str, value: &str| {
        let _ = writeln!(out, "{:<9}{value}", format!("{name}:"));
    };
    for (name, addresses) in [
        ("From", &headers.from),
        ("To", &headers.to),
        ("Cc", &headers.cc),
        ("Reply-To", &headers.reply_to),
    ] {
        if !addresses.is_empty() {
            header(name, &join_addresses(addresses));
        }
    }
    let date = headers
        .date
        .map_or_else(|| "(no date)".to_owned(), |d| format_date(d, offset));
    header("Date", &date);
    header(
        "Subject",
        headers.subject.as_deref().unwrap_or("(no subject)"),
    );

    let body = message
        .plain_text()
        .map(|text| text.replace("\r\n", "\n").trim_end().to_owned())
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| "(no text body)".to_owned());
    let _ = writeln!(out, "\n{body}");

    if !message.attachments.is_empty() {
        out.push_str("\nAttachments:\n");
        for part in &message.attachments {
            let _ = writeln!(out, "  {}", describe_attachment(part));
        }
    }
    out
}

fn join_addresses(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `report.pdf (application/pdf, 12.3 KB)`.
fn describe_attachment(part: &ParsedPart) -> String {
    let name = part
        .filename
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or("(unnamed)");
    format!("{name} ({}, {})", part.mime_type, format_size(part.size()))
}

/// A byte count in B, KB or MB (powers of 1024, one decimal place).
fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    // Display only: the precision lost converting to f64 does not matter.
    #[allow(clippy::cast_precision_loss)]
    let scaled = |unit: usize| bytes as f64 / unit as f64;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", scaled(KB))
    } else {
        format!("{:.1} MB", scaled(MB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_ATTACHMENT: &str = "\
From: Ada Lovelace <ada@example.com>\r
To: Grace Hopper <grace@example.com>, bob@example.com\r
Cc: \"Lovelace, Ada\" <ada.l@example.com>\r
Subject: Launch plan\r
Date: Tue, 10 Nov 2026 14:03:00 +0000\r
Message-ID: <plan@example.com>\r
MIME-Version: 1.0\r
Content-Type: multipart/mixed; boundary=\"b1\"\r
\r
--b1\r
Content-Type: text/plain; charset=utf-8\r
\r
Here's the latest draft.\r
\r
Ada\r
\r
--b1\r
Content-Type: application/pdf; name=\"plan.pdf\"\r
Content-Disposition: attachment; filename=\"plan.pdf\"\r
Content-Transfer-Encoding: base64\r
\r
JVBERi0xLjQK\r
--b1--\r
";

    const HTML_ONLY: &str = "\
From: news@example.com\r
To: grace@example.com\r
Content-Type: text/html; charset=utf-8\r
\r
<html><body><p>Hello <b>Grace</b></p></body></html>\r
";

    fn parse(raw: &str) -> ParsedMessage {
        ParsedMessage::parse(raw.as_bytes()).unwrap()
    }

    #[test]
    fn formats_headers_body_and_attachments() {
        assert_eq!(
            format_message(&parse(WITH_ATTACHMENT), UtcOffset::UTC),
            "\
From:    Ada Lovelace <ada@example.com>
To:      Grace Hopper <grace@example.com>, bob@example.com
Cc:      \"Lovelace, Ada\" <ada.l@example.com>
Date:    2026-11-10 14:03
Subject: Launch plan

Here's the latest draft.

Ada

Attachments:
  plan.pdf (application/pdf, 9 B)
"
        );
    }

    #[test]
    fn falls_back_to_text_from_html() {
        let out = format_message(&parse(HTML_ONLY), UtcOffset::UTC);
        assert!(
            out.starts_with(
                "\
From:    news@example.com
To:      grace@example.com
Date:    (no date)
Subject: (no subject)

"
            ),
            "{out}"
        );
        let body = out.lines().nth(5).unwrap();
        assert!(body.contains("Hello") && body.contains("Grace"), "{out}");
        assert!(!out.contains('<'), "{out}");
        assert!(!out.contains("Attachments:"));
    }

    #[test]
    fn formats_a_thread_with_numbered_messages() {
        let messages = [parse(WITH_ATTACHMENT), parse(HTML_ONLY)];
        let out = format_thread("18c2f0a1b2c3d4e5", &messages, UtcOffset::UTC);
        assert!(out.starts_with("Thread 18c2f0a1b2c3d4e5 (2 messages)\n\n── Message 1 of 2 ─"));
        assert!(out.contains("\n\n── Message 2 of 2 ─"));
        let rule = out.lines().nth(2).unwrap();
        assert_eq!(rule.chars().count(), 72);

        let single = format_thread("1", &messages[..1], UtcOffset::UTC);
        assert!(single.starts_with("Thread 1 (1 message)\n"));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(12_595), "12.3 KB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }
}
