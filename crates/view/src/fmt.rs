//! Text for the page: numbers with units, local clock times and dates.
//!
//! The browser has no time zone database. The server sends a [`LocalClock`]: the UTC offset in
//! force at each hour start of the page's range. With it, the browser can label any instant in
//! that range (cursor readouts, zoomed axes), and DST stays decided on the server.

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

/// Shown for an unknown value.
pub const DASH: &str = "–";

/// `v` with `digits` decimals, or [`DASH`].
pub fn num(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| DASH.to_owned(), |v| format!("{v:.digits$}"))
}

/// `v` with `digits` decimals and `unit`, or [`DASH`].
pub fn qty(v: Option<f64>, digits: usize, unit: &str) -> String {
    v.map_or_else(|| DASH.to_owned(), |v| format!("{v:.digits$} {unit}"))
}

/// Local clock times for instants (Unix seconds) inside a range of hours.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalClock {
    /// `(hour start, UTC offset in seconds)`, oldest first.
    hours: Vec<(i64, i32)>,
}

impl LocalClock {
    /// The offsets of `tz` at each of `hours` (hour starts, oldest first).
    pub fn of(tz: &TimeZone, hours: &[Timestamp]) -> Self {
        Self {
            hours: hours
                .iter()
                .map(|&h| (h.as_second(), tz.to_offset(h).seconds()))
                .collect(),
        }
    }

    /// The offset in force at `t`: the latest hour start at or before it (the first hour's
    /// before the range, none without hours).
    pub fn offset(&self, t: i64) -> i32 {
        let i = self.hours.partition_point(|&(start, _)| start <= t);
        self.hours
            .get(i.saturating_sub(1))
            .map_or(0, |&(_, offset)| offset)
    }

    /// Seconds since local midnight.
    pub fn of_day(&self, t: i64) -> i64 {
        (t + i64::from(self.offset(t))).rem_euclid(86_400)
    }

    /// `HH:MM` local time.
    pub fn hm(&self, t: i64) -> String {
        let s = self.of_day(t);
        format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
    }

    /// `HH:MM–HH:MM`.
    pub fn span(&self, from: i64, to: i64) -> String {
        format!("{}–{}", self.hm(from), self.hm(to))
    }
}

/// "Sunday 15 November 2026".
pub fn long_date(d: Date) -> String {
    d.strftime("%A %-d %B %Y").to_string()
}

/// "Sun 15".
pub fn short_date(d: Date) -> String {
    d.strftime("%a %-d").to_string()
}

/// "November 2026".
pub fn long_month(d: Date) -> String {
    d.strftime("%B %Y").to_string()
}

/// `YYYY-MM`.
pub fn month_key(d: Date) -> String {
    d.strftime("%Y-%m").to_string()
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn numbers_and_quantities() {
        assert_eq!(num(Some(4.256), 2), "4.26");
        assert_eq!(num(None, 2), DASH);
        assert_eq!(qty(Some(16.0), 0, "A"), "16 A");
        assert_eq!(qty(None, 1, "°C"), DASH);
    }

    #[test]
    fn local_clock_follows_the_offsets_across_fall_back() {
        let tz = TimeZone::get("Europe/Oslo").unwrap();
        // 2026-10-25: 02:00 CEST happens, then 02:00 CET again.
        let hours = [
            ts("2026-10-25T00:00:00Z"),
            ts("2026-10-25T01:00:00Z"),
            ts("2026-10-25T02:00:00Z"),
        ];
        let clock = LocalClock::of(&tz, &hours);
        let at = |s| clock.hm(ts(s).as_second());
        assert_eq!(at("2026-10-25T00:30:00Z"), "02:30");
        assert_eq!(at("2026-10-25T01:30:00Z"), "02:30");
        assert_eq!(at("2026-10-25T02:59:00Z"), "03:59");
        // Before the first hour, the first hour's offset.
        assert_eq!(at("2026-10-24T23:00:00Z"), "01:00");
        assert_eq!(
            clock.span(
                ts("2026-10-25T01:00:00Z").as_second(),
                ts("2026-10-25T02:00:00Z").as_second()
            ),
            "02:00–03:00"
        );
        assert_eq!(LocalClock::default().hm(3600 * 25 + 60), "01:01");
    }

    #[test]
    fn dates() {
        let d = date(2026, 11, 15);
        assert_eq!(long_date(d), "Sunday 15 November 2026");
        assert_eq!(short_date(date(2026, 11, 2)), "Mon 2");
        assert_eq!(long_month(d), "November 2026");
        assert_eq!(month_key(d), "2026-11");
    }
}
