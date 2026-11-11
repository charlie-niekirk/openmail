//! Senders whose remote images are always shown ("Always show from this
//! sender"). Each account's database keeps its own list.

use time::OffsetDateTime;

use crate::convert::date_to_sql;
use crate::{Result, Store};

impl Store {
    /// Every sender whose remote images are always shown, lowercased and
    /// sorted.
    pub fn image_senders(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT address FROM image_senders ORDER BY address")?;
        let addresses = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(addresses)
    }

    /// Always shows remote images from `address` (compared without regard to
    /// case). Adding an address twice keeps the first.
    pub fn allow_images_from(&self, address: &str, now: OffsetDateTime) -> Result<()> {
        self.conn().execute(
            "INSERT INTO image_senders (address, added_at) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
            rusqlite::params![address.trim().to_lowercase(), date_to_sql(now)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use crate::Store;

    #[test]
    fn senders_are_kept_lowercased_once() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.image_senders().unwrap().is_empty());
        let now = OffsetDateTime::UNIX_EPOCH;
        store.allow_images_from(" News@Example.com", now).unwrap();
        store.allow_images_from("news@example.com", now).unwrap();
        store.allow_images_from("ada@example.com", now).unwrap();
        assert_eq!(
            store.image_senders().unwrap(),
            ["ada@example.com", "news@example.com"]
        );
    }
}
