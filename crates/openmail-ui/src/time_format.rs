//! Dates in the user's local time. Compact list dates are
//! [`format_date`](crate::thread_list::format_date).

use time::{OffsetDateTime, UtcOffset};

/// `date` in the local time zone at that date (so dates across a daylight
/// saving change are right), or unchanged if the zone can't be read.
pub fn local(date: OffsetDateTime) -> OffsetDateTime {
    use chrono::{Offset as _, TimeZone as _};

    chrono::Local
        .timestamp_opt(date.unix_timestamp(), 0)
        .single()
        .and_then(|local| {
            UtcOffset::from_whole_seconds(local.offset().fix().local_minus_utc()).ok()
        })
        .map_or(date, |offset| date.to_offset(offset))
}

/// A full date for an expanded message, as Gmail shows it:
/// `Tue, Nov 3, 2026, 3:07 PM`. `date` is already in local time.
pub fn full_date(date: OffsetDateTime) -> String {
    let (hour, period) = match date.hour() {
        0 => (12, "AM"),
        h @ 1..=11 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    let weekday = date.weekday().to_string();
    let month = date.month().to_string();
    format!(
        "{}, {} {}, {}, {hour}:{:02} {period}",
        &weekday[..3],
        &month[..3],
        date.day(),
        date.year(),
        date.minute()
    )
}

#[cfg(test)]
mod tests {
    use time::macros::{datetime, offset};

    use super::*;

    #[test]
    fn full_dates_name_the_weekday() {
        assert_eq!(
            full_date(datetime!(2026-11-03 15:07 UTC)),
            "Tue, Nov 3, 2026, 3:07 PM"
        );
        assert_eq!(
            full_date(datetime!(2026-01-01 00:05 -8)),
            "Thu, Jan 1, 2026, 12:05 AM"
        );
        assert_eq!(
            full_date(datetime!(2026-06-30 12:00 UTC).to_offset(offset!(+1))),
            "Tue, Jun 30, 2026, 1:00 PM"
        );
    }

    #[test]
    fn local_keeps_the_instant() {
        let date = datetime!(2026-11-03 15:07 UTC);
        assert_eq!(local(date), date);
    }
}
