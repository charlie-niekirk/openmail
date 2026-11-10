//! Accounts and labels.

use openmail_core::{
    Account, AccountId, Label, LabelColor, LabelId, LabelListVisibility, LabelType,
    MessageListVisibility,
};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Error, Result, Store};

impl Store {
    /// Inserts or updates the account this database belongs to.
    pub fn upsert_account(&self, account: &Account) -> Result<()> {
        self.conn().execute(
            "INSERT INTO accounts (id, email, display_name) VALUES (:id, :email, :name)
             ON CONFLICT (id) DO UPDATE SET email = excluded.email, display_name = excluded.display_name",
            named_params! {
                ":id": account.id.as_str(),
                ":email": account.email,
                ":name": account.display_name,
            },
        )?;
        Ok(())
    }

    /// The account with this ID, if stored.
    pub fn account(&self, id: &AccountId) -> Result<Option<Account>> {
        let account = self
            .conn()
            .query_row(
                "SELECT id, email, display_name FROM accounts WHERE id = ?1",
                [id.as_str()],
                |row| {
                    Ok(Account {
                        id: AccountId::new(row.get::<_, String>(0)?),
                        email: row.get(1)?,
                        display_name: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(account)
    }

    /// Inserts or updates a label.
    pub fn upsert_label(&self, label: &Label) -> Result<()> {
        let (text_color, background_color) = label
            .color
            .as_ref()
            .map(|c| (c.text_color.as_str(), c.background_color.as_str()))
            .unzip();
        self.conn().execute(
            "INSERT INTO labels (id, name, type, text_color, background_color,
                                 label_list_visibility, message_list_visibility)
             VALUES (:id, :name, :type, :text_color, :background_color, :label_list, :message_list)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 type = excluded.type,
                 text_color = excluded.text_color,
                 background_color = excluded.background_color,
                 label_list_visibility = excluded.label_list_visibility,
                 message_list_visibility = excluded.message_list_visibility",
            named_params! {
                ":id": label.id.as_str(),
                ":name": label.name,
                ":type": label_type_to_sql(label.label_type),
                ":text_color": text_color,
                ":background_color": background_color,
                ":label_list": label_list_to_sql(label.label_list_visibility),
                ":message_list": message_list_to_sql(label.message_list_visibility),
            },
        )?;
        Ok(())
    }

    /// The label with this ID, if stored.
    pub fn label(&self, id: &LabelId) -> Result<Option<Label>> {
        self.conn()
            .query_row(
                &format!("{SELECT_LABEL} WHERE id = ?1"),
                [id.as_str()],
                LabelRow::read,
            )
            .optional()?
            .map(LabelRow::into_label)
            .transpose()
    }

    /// Every stored label: system labels first, then by name.
    pub fn labels(&self) -> Result<Vec<Label>> {
        let mut stmt = self
            .conn()
            .prepare(&format!("{SELECT_LABEL} ORDER BY type = 'user', name, id"))?;
        let rows = stmt
            .query_map([], LabelRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(LabelRow::into_label).collect()
    }

    /// Deletes a label and removes it from every message. Returns whether it existed.
    pub fn delete_label(&self, id: &LabelId) -> Result<bool> {
        self.transaction(|store| {
            let conn = store.conn();
            conn.execute(
                "DELETE FROM message_labels WHERE label_id = ?1",
                [id.as_str()],
            )?;
            Ok(conn.execute("DELETE FROM labels WHERE id = ?1", [id.as_str()])? > 0)
        })
    }
}

const SELECT_LABEL: &str = "SELECT id, name, type, text_color, background_color, \
                            label_list_visibility, message_list_visibility FROM labels";

/// A `labels` row as stored.
struct LabelRow {
    id: String,
    name: String,
    label_type: String,
    text_color: Option<String>,
    background_color: Option<String>,
    label_list: String,
    message_list: String,
}

impl LabelRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            label_type: row.get(2)?,
            text_color: row.get(3)?,
            background_color: row.get(4)?,
            label_list: row.get(5)?,
            message_list: row.get(6)?,
        })
    }

    fn into_label(self) -> Result<Label> {
        let color = match (self.text_color, self.background_color) {
            (Some(text_color), Some(background_color)) => Some(LabelColor {
                text_color,
                background_color,
            }),
            _ => None,
        };
        Ok(Label {
            id: LabelId::new(self.id),
            name: self.name,
            label_type: label_type_from_sql(&self.label_type)?,
            color,
            label_list_visibility: label_list_from_sql(&self.label_list)?,
            message_list_visibility: message_list_from_sql(&self.message_list)?,
        })
    }
}

// The column values match Gmail's API strings (and the serde names in core).

fn label_type_to_sql(value: LabelType) -> &'static str {
    match value {
        LabelType::System => "system",
        LabelType::User => "user",
    }
}

fn label_type_from_sql(value: &str) -> Result<LabelType> {
    match value {
        "system" => Ok(LabelType::System),
        "user" => Ok(LabelType::User),
        other => Err(Error::Corrupt(format!("label type {other:?}"))),
    }
}

fn label_list_to_sql(value: LabelListVisibility) -> &'static str {
    match value {
        LabelListVisibility::LabelShow => "labelShow",
        LabelListVisibility::LabelShowIfUnread => "labelShowIfUnread",
        LabelListVisibility::LabelHide => "labelHide",
    }
}

fn label_list_from_sql(value: &str) -> Result<LabelListVisibility> {
    match value {
        "labelShow" => Ok(LabelListVisibility::LabelShow),
        "labelShowIfUnread" => Ok(LabelListVisibility::LabelShowIfUnread),
        "labelHide" => Ok(LabelListVisibility::LabelHide),
        other => Err(Error::Corrupt(format!("label list visibility {other:?}"))),
    }
}

fn message_list_to_sql(value: MessageListVisibility) -> &'static str {
    match value {
        MessageListVisibility::Show => "show",
        MessageListVisibility::Hide => "hide",
    }
}

fn message_list_from_sql(value: &str) -> Result<MessageListVisibility> {
    match value {
        "show" => Ok(MessageListVisibility::Show),
        "hide" => Ok(MessageListVisibility::Hide),
        other => Err(Error::Corrupt(format!("message list visibility {other:?}"))),
    }
}
