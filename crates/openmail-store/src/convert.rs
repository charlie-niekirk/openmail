//! Conversions between domain values and their column representations.

use openmail_core::{Address, HistoryId};
use time::OffsetDateTime;

use crate::{Error, Result};

const NANOS_PER_MILLI: i128 = 1_000_000;

/// A date as Unix milliseconds.
pub(crate) fn date_to_sql(date: OffsetDateTime) -> Result<i64> {
    i64::try_from(date.unix_timestamp_nanos() / NANOS_PER_MILLI)
        .map_err(|_| Error::OutOfRange("date"))
}

/// A date from Unix milliseconds.
pub(crate) fn date_from_sql(millis: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * NANOS_PER_MILLI)
        .map_err(|_| Error::Corrupt(format!("date {millis} is out of range")))
}

pub(crate) fn history_to_sql(id: HistoryId) -> Result<i64> {
    u64_to_sql(id.get(), "history ID")
}

pub(crate) fn history_from_sql(id: i64) -> Result<HistoryId> {
    u64_from_sql(id, "history ID").map(HistoryId::new)
}

pub(crate) fn u64_to_sql(value: u64, what: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::OutOfRange(what))
}

pub(crate) fn u64_from_sql(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Corrupt(format!("negative {what} {value}")))
}

/// An address list as a JSON array.
pub(crate) fn addresses_to_sql(addresses: &[Address]) -> Result<String> {
    serde_json::to_string(addresses).map_err(|err| Error::Corrupt(err.to_string()))
}

pub(crate) fn addresses_from_sql(json: &str) -> Result<Vec<Address>> {
    serde_json::from_str(json).map_err(|err| Error::Corrupt(format!("address list: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_at_millisecond_precision() {
        let date = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_000_000).unwrap();
        let millis = date_to_sql(date).unwrap();
        assert_eq!(millis, 1_700_000_000_123);
        assert_eq!(date_from_sql(millis).unwrap(), date);
    }

    #[test]
    fn rejects_values_that_do_not_fit() {
        assert!(matches!(
            history_to_sql(HistoryId::new(u64::MAX)),
            Err(Error::OutOfRange(_))
        ));
        assert!(matches!(history_from_sql(-1), Err(Error::Corrupt(_))));
    }
}
