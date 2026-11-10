//! What an op changes in the local store, and how to undo or redo that.

use std::collections::BTreeSet;

use openmail_core::{
    Label, LabelId, LabelListVisibility, LabelType, Message, MessageId, MessageListVisibility,
    SystemLabel, Thread, ThreadId,
};
use openmail_store::Store;
use serde::{Deserialize, Serialize};

use super::{Op, OpId, Target};
use crate::Result;

/// The prefix of the temporary ID a label created offline has until Gmail
/// assigns its real one.
pub(crate) const LOCAL_LABEL_PREFIX: &str = "openmail-pending-";

/// An op as persisted in `pending_ops.payload`: the op and what applying it
/// changed locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) op: Op,
    #[serde(default)]
    pub(crate) effect: Effect,
}

/// What applying an op changed in the store, so it can be reverted, and the
/// IDs it resolved its target to, so it can be sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Effect {
    /// The stored messages a label change covers, with the labels it actually
    /// added and removed on each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) messages: Vec<MessageEffect>,
    /// Targets that are not stored, so nothing changed locally; they are
    /// still sent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unresolved_threads: Vec<ThreadId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unresolved_messages: Vec<MessageId>,
    /// What a delete removed, to put back if it is undone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deleted_threads: Vec<(Thread, Vec<Message>)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deleted_messages: Vec<Message>,
    /// A label as it was before it was renamed, recolored or deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label_before: Option<Label>,
    /// The messages a deleted label was removed from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) label_messages: Vec<MessageId>,
    /// The temporary ID of a label created offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_label: Option<LabelId>,
}

/// The labels an op actually changed on one message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MessageEffect {
    pub(crate) id: MessageId,
    pub(crate) thread: ThreadId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) added: Vec<LabelId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) removed: Vec<LabelId>,
}

/// What an op is ordered against: ops sharing a key are sent in queue order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Key {
    Thread(ThreadId),
    /// A message that is not stored, so its thread is unknown.
    Message(MessageId),
    Label(LabelId),
}

impl Record {
    /// The keys the op is ordered against.
    pub(crate) fn keys(&self) -> BTreeSet<Key> {
        let mut keys = BTreeSet::new();
        let effect = &self.effect;
        match &self.op {
            Op::ModifyLabels { target, .. }
            | Op::Trash { target }
            | Op::Untrash { target }
            | Op::Delete { target } => {
                if let Target::Threads(threads) = target {
                    keys.extend(threads.iter().cloned().map(Key::Thread));
                }
                keys.extend(
                    effect
                        .messages
                        .iter()
                        .map(|m| Key::Thread(m.thread.clone())),
                );
                keys.extend(
                    effect
                        .deleted_messages
                        .iter()
                        .map(|m| Key::Thread(m.thread_id.clone())),
                );
                keys.extend(effect.unresolved_messages.iter().cloned().map(Key::Message));
            }
            Op::CreateLabel { .. } => {
                keys.extend(effect.local_label.clone().map(Key::Label));
            }
            Op::RenameLabel { label, .. }
            | Op::RecolorLabel { label, .. }
            | Op::DeleteLabel { label } => {
                keys.insert(Key::Label(label.clone()));
            }
        }
        keys
    }

    /// The label IDs the op refers to.
    pub(crate) fn label_ids(&self) -> Vec<&LabelId> {
        match &self.op {
            Op::ModifyLabels { add, remove, .. } => add.iter().chain(remove).collect(),
            Op::RenameLabel { label, .. }
            | Op::RecolorLabel { label, .. }
            | Op::DeleteLabel { label } => vec![label],
            Op::CreateLabel { .. } | Op::Trash { .. } | Op::Untrash { .. } | Op::Delete { .. } => {
                Vec::new()
            }
        }
    }

    /// Whether the op refers to a label created offline that Gmail has not
    /// assigned an ID to yet.
    pub(crate) fn needs_local_label(&self) -> bool {
        self.label_ids()
            .iter()
            .any(|id| id.as_str().starts_with(LOCAL_LABEL_PREFIX))
    }

    /// Replaces the label ID `from` with `to` throughout. Returns whether the
    /// op referred to `from`.
    pub(crate) fn replace_label_id(&mut self, from: &LabelId, to: &LabelId) -> bool {
        let mut changed = false;
        let mut swap = |id: &mut LabelId| {
            if id == from {
                *id = to.clone();
                changed = true;
            }
        };
        match &mut self.op {
            Op::ModifyLabels { add, remove, .. } => {
                add.iter_mut().chain(remove).for_each(&mut swap);
            }
            Op::RenameLabel { label, .. }
            | Op::RecolorLabel { label, .. }
            | Op::DeleteLabel { label } => swap(label),
            Op::CreateLabel { .. } | Op::Trash { .. } | Op::Untrash { .. } | Op::Delete { .. } => {}
        }
        for message in &mut self.effect.messages {
            message
                .added
                .iter_mut()
                .chain(&mut message.removed)
                .for_each(&mut swap);
        }
        changed
    }

    /// Whether `later` exactly undoes this op: both are label changes to the
    /// same target, and `later` put back exactly the labels this one changed,
    /// on the same messages. Sending neither then leaves Gmail as it was, which
    /// is what the store shows.
    pub(crate) fn is_inverted_by(&self, later: &Record) -> bool {
        let (
            Op::ModifyLabels {
                target: a_target, ..
            },
            Op::ModifyLabels {
                target: b_target, ..
            },
        ) = (&self.op, &later.op)
        else {
            return false;
        };
        let (a, b) = (&self.effect, &later.effect);
        let resolved =
            |e: &Effect| e.unresolved_threads.is_empty() && e.unresolved_messages.is_empty();
        a_target == b_target
            && resolved(a)
            && resolved(b)
            && !a
                .messages
                .iter()
                .all(|m| m.added.is_empty() && m.removed.is_empty())
            && a.messages.len() == b.messages.len()
            && a.messages.iter().zip(&b.messages).all(|(a, b)| {
                a.id == b.id
                    && sorted(&a.added) == sorted(&b.removed)
                    && sorted(&a.removed) == sorted(&b.added)
            })
    }
}

fn union(into: &mut Vec<LabelId>, from: &[LabelId]) {
    for id in from {
        if !into.contains(id) {
            into.push(id.clone());
        }
    }
}

fn sorted(ids: &[LabelId]) -> Vec<&LabelId> {
    let mut ids: Vec<&LabelId> = ids.iter().collect();
    ids.sort();
    ids
}

/// The labels Gmail's trash and untrash change.
fn trash_labels(trash: bool) -> (Vec<LabelId>, Vec<LabelId>) {
    let (trash_label, inbox) = (
        LabelId::from(SystemLabel::Trash),
        LabelId::from(SystemLabel::Inbox),
    );
    if trash {
        (vec![trash_label], vec![inbox])
    } else {
        (Vec::new(), vec![trash_label])
    }
}

/// Applies `op` to the store and returns what it changed.
///
/// `prior` is the effect of an earlier application, when the op is being
/// re-applied on top of changes from Gmail: it keeps the temporary ID of a
/// label created offline, and what deletes removed that is no longer stored.
pub(crate) fn apply(store: &Store, id: OpId, op: &Op, prior: Option<&Effect>) -> Result<Effect> {
    let mut effect = Effect::default();
    match op {
        Op::ModifyLabels {
            target,
            add,
            remove,
        } => change_labels(store, target, add, remove, prior, &mut effect)?,
        Op::Trash { target } => {
            let (add, remove) = trash_labels(true);
            change_labels(store, target, &add, &remove, prior, &mut effect)?;
        }
        Op::Untrash { target } => {
            let (add, remove) = trash_labels(false);
            change_labels(store, target, &add, &remove, prior, &mut effect)?;
        }
        Op::Delete { target } => {
            delete(store, target, &mut effect)?;
            if let Some(prior) = prior {
                // Keep what an earlier application removed and Gmail has not
                // sent back.
                for (thread, messages) in &prior.deleted_threads {
                    if !effect
                        .deleted_threads
                        .iter()
                        .any(|(t, _)| t.id == thread.id)
                    {
                        effect
                            .deleted_threads
                            .push((thread.clone(), messages.clone()));
                    }
                }
                for message in &prior.deleted_messages {
                    if !effect.deleted_messages.iter().any(|m| m.id == message.id) {
                        effect.deleted_messages.push(message.clone());
                    }
                }
                effect
                    .unresolved_threads
                    .retain(|t| !effect.deleted_threads.iter().any(|(d, _)| &d.id == t));
                effect
                    .unresolved_messages
                    .retain(|m| !effect.deleted_messages.iter().any(|d| &d.id == m));
            }
        }
        Op::CreateLabel { name, color } => {
            let local = prior
                .and_then(|p| p.local_label.clone())
                .unwrap_or_else(|| LabelId::new(format!("{LOCAL_LABEL_PREFIX}{id}")));
            store.upsert_label(&Label {
                id: local.clone(),
                name: name.clone(),
                label_type: LabelType::User,
                color: color.clone(),
                label_list_visibility: LabelListVisibility::default(),
                message_list_visibility: MessageListVisibility::default(),
            })?;
            effect.local_label = Some(local);
        }
        Op::RenameLabel { label, name } => {
            edit_label(store, label, &mut effect, |l| l.name.clone_from(name))?;
        }
        Op::RecolorLabel { label, color } => {
            edit_label(store, label, &mut effect, |l| l.color = Some(color.clone()))?;
        }
        Op::DeleteLabel { label } => {
            effect.label_before = store.label(label)?;
            effect.label_messages = store.messages_with_label(label)?;
            store.delete_label(label)?;
        }
    }
    if let Some(prior) = prior {
        // What the label looked like before the user first changed it.
        if prior.label_before.is_some() {
            effect.label_before.clone_from(&prior.label_before);
        }
        for message in &prior.label_messages {
            if !effect.label_messages.contains(message) {
                effect.label_messages.push(message.clone());
            }
        }
    }
    Ok(effect)
}

/// Changes the labels of the target's stored messages.
///
/// When re-applying, a label the op changed before still counts as changed
/// by it even if the message already has it (the earlier application may
/// still be in place), so that undoing the op reverts it.
fn change_labels(
    store: &Store,
    target: &Target,
    add: &[LabelId],
    remove: &[LabelId],
    prior: Option<&Effect>,
    effect: &mut Effect,
) -> Result<()> {
    let mut change = |id: &MessageId, thread: &ThreadId| -> Result<()> {
        if let Some(delta) = store.change_message_labels(id, add, remove)? {
            let mut message = MessageEffect {
                id: id.clone(),
                thread: thread.clone(),
                added: delta.added,
                removed: delta.removed,
            };
            if let Some(before) = prior.and_then(|p| p.messages.iter().find(|m| &m.id == id)) {
                union(&mut message.added, &before.added);
                union(&mut message.removed, &before.removed);
            }
            effect.messages.push(message);
        }
        Ok(())
    };
    match target {
        Target::Threads(threads) => {
            for thread in threads {
                let messages = store.thread_message_ids(thread)?;
                if messages.is_empty() {
                    effect.unresolved_threads.push(thread.clone());
                }
                for message in &messages {
                    change(message, thread)?;
                }
            }
        }
        Target::Messages(messages) => {
            for id in messages {
                match store.message(id)? {
                    Some(message) => change(id, &message.thread_id)?,
                    None => effect.unresolved_messages.push(id.clone()),
                }
            }
        }
    }
    Ok(())
}

fn delete(store: &Store, target: &Target, effect: &mut Effect) -> Result<()> {
    match target {
        Target::Threads(threads) => {
            for id in threads {
                match store.thread(id)? {
                    Some(thread) => {
                        let messages = store
                            .thread_message_ids(id)?
                            .iter()
                            .filter_map(|m| store.message(m).transpose())
                            .collect::<Result<Vec<_>, _>>()?;
                        store.delete_thread(id)?;
                        effect.deleted_threads.push((thread, messages));
                    }
                    None => effect.unresolved_threads.push(id.clone()),
                }
            }
        }
        Target::Messages(messages) => {
            for id in messages {
                match store.message(id)? {
                    Some(message) => {
                        store.delete_message(id)?;
                        effect.deleted_messages.push(message);
                    }
                    None => effect.unresolved_messages.push(id.clone()),
                }
            }
        }
    }
    Ok(())
}

fn edit_label(
    store: &Store,
    id: &LabelId,
    effect: &mut Effect,
    edit: impl FnOnce(&mut Label),
) -> Result<()> {
    if let Some(mut label) = store.label(id)? {
        effect.label_before = Some(label.clone());
        edit(&mut label);
        store.upsert_label(&label)?;
    }
    Ok(())
}

/// Reverts what `effect` changed, as far as the store still allows: labels
/// go back on and off the messages, deleted rows come back, label edits are
/// undone and a label created offline is removed.
pub(crate) fn revert(store: &Store, effect: &Effect) -> Result<()> {
    for message in effect.messages.iter().rev() {
        store.modify_message_labels(&message.id, &message.removed, &message.added)?;
    }
    if !effect.deleted_threads.is_empty() {
        store.replace_threads(&effect.deleted_threads)?;
    }
    for message in &effect.deleted_messages {
        if store.thread(&message.thread_id)?.is_some() {
            store.upsert_message(message)?;
        }
    }
    if let Some(label) = &effect.label_before {
        store.upsert_label(label)?;
        for message in &effect.label_messages {
            store.modify_message_labels(message, std::slice::from_ref(&label.id), &[])?;
        }
    }
    if let Some(local) = &effect.local_label {
        store.delete_label(local)?;
    }
    Ok(())
}
