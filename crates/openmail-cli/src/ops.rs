//! `archive`, `star`, `unstar`, `read`, `unread` and `label`: queue a change
//! through the offline ops queue, then send it.

use std::time::Duration;

use anyhow::{Context as _, bail};
use openmail_core::{LabelId, ThreadId};
use openmail_gmail::GmailClient;
use openmail_store::{PendingOpState, Store};
use openmail_sync::{Op, OpEvent, OpsConfig, OpsQueue, Target};
use time::OffsetDateTime;

use crate::sync::{cancel_on_ctrl_c, open_store};
use crate::watch::error_chain;

/// A change to threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Archive,
    Star,
    Unstar,
    Read,
    Unread,
    /// Labels to add and remove, by ID or name.
    Label {
        add: Vec<String>,
        remove: Vec<String>,
    },
}

/// Applies `action` to `threads` in the local store, queues it, and sends
/// the queue (with any ops left from earlier runs) until it is empty or
/// Ctrl-C, retrying failures.
pub async fn run(
    client: GmailClient,
    account: &str,
    action: Action,
    threads: Vec<String>,
) -> anyhow::Result<()> {
    let (mut store, _) = open_store(account)?;
    let target = Target::Threads(threads.into_iter().map(ThreadId::new).collect());
    let op = match action {
        Action::Archive => Op::archive(target),
        Action::Star => Op::star(target),
        Action::Unstar => Op::unstar(target),
        Action::Read => Op::mark_read(target),
        Action::Unread => Op::mark_unread(target),
        Action::Label { add, remove } => {
            if add.is_empty() && remove.is_empty() {
                bail!("give at least one --add or --remove label");
            }
            Op::modify(
                target,
                resolve_labels(&store, &add)?,
                resolve_labels(&store, &remove)?,
            )
        }
    };
    // The CLI sends at once: there is no undo here.
    let config = OpsConfig {
        undo_window: Duration::ZERO,
        ..OpsConfig::default()
    };
    let queue = OpsQueue::new(client, config);
    let id = queue
        .enqueue(&store, op)
        .context("could not queue the change")?;
    println!("Queued op {id}. Sending; Ctrl-C stops (queued ops are sent next time).");

    let cancel = cancel_on_ctrl_c();
    queue
        .drain(&mut store, &cancel, |event| {
            println!("{}", event_line(&event));
        })
        .await
        .context("sending stopped; queued ops are kept")?;
    let left = queue
        .queued(&store)?
        .iter()
        .filter(|op| op.state != PendingOpState::Sent)
        .count();
    if left > 0 {
        println!("Stopped with {left} op(s) still queued.");
    }
    Ok(())
}

/// Label IDs for `labels`, each an ID or a name (as stored by `sync`).
fn resolve_labels(store: &Store, labels: &[String]) -> anyhow::Result<Vec<LabelId>> {
    let stored = store.labels()?;
    labels
        .iter()
        .map(|label| {
            stored
                .iter()
                .find(|l| l.id.as_str() == label)
                .or_else(|| stored.iter().find(|l| l.name.eq_ignore_ascii_case(label)))
                .map(|l| l.id.clone())
                .with_context(|| {
                    format!("unknown label {label:?}; run `openmail-cli sync` to fetch labels")
                })
        })
        .collect()
}

/// One line describing what happened to an op.
pub fn event_line(event: &OpEvent) -> String {
    match event {
        OpEvent::Sent { id, op } => format!("op {id} ({}): sent", op.kind()),
        OpEvent::CancelledOut { id, op } => {
            format!("op {id} ({}): cancelled out by another op", op.kind())
        }
        OpEvent::Retrying {
            id,
            op,
            attempts,
            retry_at,
            error,
        } => {
            let wait = (*retry_at - OffsetDateTime::now_utc())
                .whole_seconds()
                .max(0);
            format!(
                "op {id} ({}): failed ({}); attempt {attempts}, retrying in {wait}s",
                op.kind(),
                error_chain(&**error),
            )
        }
        OpEvent::Failed { id, op, error } => format!(
            "op {id} ({}): Gmail rejected it ({}); rolled back",
            op.kind(),
            error_chain(&**error),
        ),
        _ => format!("{event:?}"),
    }
}

#[cfg(test)]
mod tests {
    use openmail_core::{Label, LabelListVisibility, LabelType, MessageListVisibility};

    use super::*;

    #[test]
    fn resolves_labels_by_id_or_name() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_label(&Label {
                id: LabelId::new("Label_12"),
                name: "Work/Projects".to_owned(),
                label_type: LabelType::User,
                color: None,
                label_list_visibility: LabelListVisibility::LabelShow,
                message_list_visibility: MessageListVisibility::Show,
            })
            .unwrap();
        let labels = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
            resolve_labels(&store, &names)
        };
        assert_eq!(
            labels(&["Label_12", "work/projects"]).unwrap(),
            [LabelId::new("Label_12"), LabelId::new("Label_12")]
        );
        assert!(labels(&["Nope"]).is_err());
    }
}
