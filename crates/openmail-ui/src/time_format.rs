//! Dates as the mailbox shows them, in the user's local time.
//!
//! `time` can only read the local UTC offset while the process has a single
//! thread, so the app captures it at start-up with [`capture_local_offset`]
//! and views read it with [`local_offset`].

use std::sync::OnceLock;

use time::{OffsetDateTime, UtcOffset};

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Records the local UTC offset. Call at the top of `main`, before any other
/// thread starts; later calls do nothing.
pub fn capture_local_offset() {
    LOCAL_OFFSET.get_or_init(read_local_offset);
}

/// The local UTC offset recorded by [`capture_local_offset`], or UTC if it
/// could not be read.
pub fn local_offset() -> UtcOffset {
    *LOCAL_OFFSET.get_or_init(read_local_offset)
}

fn read_local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

/// A compact date for lists and collapsed messages: `15:07` today, `3 Nov`
/// earlier this year, `3 Nov 2024` before that.
pub fn short_date(date: OffsetDateTime, now: OffsetDateTime, offset: UtcOffset) -> String {
    let (date, now) = (date.to_offset(offset), now.to_offset(offset));
    if date.date() == now.date() {
        format!("{:02}:{:02}", date.hour(), date.minute())
    } else if date.year() == now.year() {
        format!("{} {}", date.day(), month(date))
    } else {
        format!("{} {} {}", date.day(), month(date), date.year())
    }
}

/// A full date for an expanded message: `Tue, 3 Nov 2026, 15:07`.
pub fn full_date(date: OffsetDateTime, offset: UtcOffset) -> String {
    let date = date.to_offset(offset);
    let weekday = date.weekday().to_string();
    format!(
        "{}, {} {} {}, {:02}:{:02}",
        &weekday[..3],
        date.day(),
        month(date),
        date.year(),
        date.hour(),
        date.minute()
    )
}

/// The month's three-letter English abbreviation.
fn month(date: OffsetDateTime) -> String {
    date.month().to_string()[..3].to_owned()
}

#[cfg(test)]
mod tests {
    use time::macros::{datetime, offset};

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-11-11 09:30 UTC);

    #[test]
    fn today_shows_the_time() {
        let date = datetime!(2026-11-11 08:05 UTC);
        assert_eq!(short_date(date, NOW, UtcOffset::UTC), "08:05");
    }

    #[test]
    fn this_year_shows_day_and_month() {
        let date = datetime!(2026-03-02 23:00 UTC);
        assert_eq!(short_date(date, NOW, UtcOffset::UTC), "2 Mar");
    }

    #[test]
    fn earlier_years_show_the_year() {
        let date = datetime!(2024-12-31 12:00 UTC);
        assert_eq!(short_date(date, NOW, UtcOffset::UTC), "31 Dec 2024");
    }

    #[test]
    fn days_are_compared_in_local_time() {
        // 23:30 UTC on the 10th is already the 11th in UTC+1.
        let date = datetime!(2026-11-10 23:30 UTC);
        assert_eq!(short_date(date, NOW, offset!(+1)), "00:30");
        assert_eq!(short_date(date, NOW, UtcOffset::UTC), "10 Nov");
    }

    #[test]
    fn full_dates_name_the_weekday() {
        let date = datetime!(2026-11-03 15:07 UTC);
        assert_eq!(full_date(date, UtcOffset::UTC), "Tue, 3 Nov 2026, 15:07");
        assert_eq!(full_date(date, offset!(-8)), "Tue, 3 Nov 2026, 07:07");
    }
}
