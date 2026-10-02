//! Budget periods: Asia/Kolkata calendar day and month, computed from UTC instants.
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

/// Asia/Kolkata is a fixed UTC+05:30 offset with no DST.
const IST_OFFSET_SECONDS: i64 = 5 * 3600 + 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub day: NaiveDate,
    pub month: NaiveDate,
}

impl Period {
    pub fn at(instant: DateTime<Utc>) -> Self {
        let local = (instant + Duration::seconds(IST_OFFSET_SECONDS)).date_naive();
        let month = local.with_day(1).unwrap_or(local);
        Self { day: local, month }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0)
            .single()
            .expect("valid test instant")
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    #[test]
    fn test_period_at_utc_evening_rolls_to_next_ist_day() {
        // 18:30 UTC == 00:00 IST next day.
        assert_eq!(Period::at(utc(2026, 10, 3, 18, 30)).day, date(2026, 10, 4));
        assert_eq!(Period::at(utc(2026, 10, 3, 18, 29)).day, date(2026, 10, 3));
    }

    #[test]
    fn test_period_at_month_end_rolls_to_next_ist_month() {
        assert_eq!(
            Period::at(utc(2026, 10, 31, 18, 30)).month,
            date(2026, 11, 1)
        );
        assert_eq!(
            Period::at(utc(2026, 10, 31, 18, 29)).month,
            date(2026, 10, 1)
        );
    }
}
