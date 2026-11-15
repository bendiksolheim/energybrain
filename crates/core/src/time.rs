//! Hour keys and local-calendar bucketing.
//!
//! Every stored instant is UTC. Hours are identified by their UTC start ([`HourStart`]).
//! The local calendar ([`LocalCalendar`]) is only used to answer "which local day / month does
//! this hour belong to", which is what the capacity tariff's top-3-days rule needs.
//!
//! Assumption: the configured time zone has whole-hour UTC offsets (true for `Europe/Oslo`),
//! so a UTC hour is also exactly one local hour.

use std::fmt;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

const HOUR: SignedDuration = SignedDuration::from_hours(1);
const NANOS_PER_HOUR: i128 = 3_600_000_000_000;

/// Errors from calendar arithmetic. These only occur at the edges of the representable
/// date range (years ±9999).
#[derive(Debug, thiserror::Error)]
#[error("calendar arithmetic out of range: {0}")]
pub struct TimeError(#[from] jiff::Error);

/// The UTC start of an hour. Always aligned to a whole hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct HourStart(Timestamp);

impl HourStart {
    /// The hour that contains `at`.
    pub fn containing(at: Timestamp) -> Self {
        // Always in [0, 3.6e12) ns, so it fits an i64. (Before 1970 jiff reports negative
        // seconds *and* nanoseconds, so this must work on the full nanosecond count.)
        #[allow(clippy::cast_possible_truncation)]
        let into_hour =
            SignedDuration::from_nanos(at.as_nanosecond().rem_euclid(NANOS_PER_HOUR) as i64);
        // The `Sub` impl panics only if the result precedes `Timestamp::MIN`, i.e. for an
        // instant in the first, partial hour of jiff's range (year -9999). No meter reports
        // such instants.
        Self(at - into_hour)
    }

    /// The instant this hour begins.
    pub const fn start(self) -> Timestamp {
        self.0
    }

    /// The instant this hour ends (= the next hour's start).
    pub fn end(self) -> Timestamp {
        // Overflows only for the final hour of year 9999.
        self.0 + HOUR
    }

    /// The following hour.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.end())
    }

    /// How much of this hour lies before `at`, as a fraction clamped to `[0, 1]`.
    pub fn elapsed_fraction(self, at: Timestamp) -> f64 {
        let elapsed = at.duration_since(self.0).as_secs_f64() / HOUR.as_secs_f64();
        elapsed.clamp(0.0, 1.0)
    }

    /// How much of this hour lies after `at`, as a fraction clamped to `[0, 1]`.
    pub fn remaining_fraction(self, at: Timestamp) -> f64 {
        1.0 - self.elapsed_fraction(at)
    }
}

impl fmt::Display for HourStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A local calendar month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalMonth {
    /// Calendar year.
    pub year: i16,
    /// Month, 1–12.
    pub month: i8,
}

impl LocalMonth {
    /// The month a local date belongs to.
    pub fn of(date: Date) -> Self {
        Self {
            year: date.year(),
            month: date.month(),
        }
    }
}

/// Local-time bucketing for a configured time zone.
#[derive(Debug, Clone)]
pub struct LocalCalendar {
    tz: TimeZone,
}

impl LocalCalendar {
    /// A calendar for `tz`. Resolving a zone name is the caller's job (it may touch the
    /// file system), which keeps this crate free of I/O.
    pub fn new(tz: TimeZone) -> Self {
        Self { tz }
    }

    /// The time zone in use.
    pub fn time_zone(&self) -> &TimeZone {
        &self.tz
    }

    /// The local date an instant falls on.
    pub fn local_date(&self, at: Timestamp) -> Date {
        self.tz.to_datetime(at).date()
    }

    /// The local date an hour belongs to (the date of its start).
    pub fn date_of(&self, hour: HourStart) -> Date {
        self.local_date(hour.start())
    }

    /// The local month an hour belongs to.
    pub fn month_of(&self, hour: HourStart) -> LocalMonth {
        LocalMonth::of(self.date_of(hour))
    }

    /// The first hour of a local date.
    pub fn first_hour(&self, date: Date) -> Result<HourStart, TimeError> {
        let start = date.to_zoned(self.tz.clone())?.timestamp();
        Ok(HourStart::containing(start))
    }

    /// Every hour of a local date, in order. 23 on the spring-forward day, 25 on fall-back.
    pub fn hours_of(&self, date: Date) -> Result<Vec<HourStart>, TimeError> {
        let first = self.first_hour(date)?;
        let end = self.first_hour(date.tomorrow()?)?;
        Ok(std::iter::successors(Some(first), |h| Some(h.next()))
            .take_while(|h| *h < end)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use proptest::prelude::*;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn oslo() -> LocalCalendar {
        LocalCalendar::new(TimeZone::get("Europe/Oslo").unwrap())
    }

    #[test]
    fn hour_start_truncates() {
        let h = HourStart::containing(ts("2026-11-15T17:42:13.5Z"));
        assert_eq!(h.start(), ts("2026-11-15T17:00:00Z"));
        assert_eq!(h.end(), ts("2026-11-15T18:00:00Z"));
        assert_eq!(h.next().start(), ts("2026-11-15T18:00:00Z"));
        assert_eq!(HourStart::containing(h.start()), h);
        assert_eq!(h.to_string(), "2026-11-15T17:00:00Z");
    }

    #[test]
    fn hour_start_before_the_epoch() {
        let h = HourStart::containing(ts("1969-12-31T23:30:00Z"));
        assert_eq!(h.start(), ts("1969-12-31T23:00:00Z"));
    }

    #[test]
    fn fractions() {
        let h = HourStart::containing(ts("2026-11-15T17:00:00Z"));
        assert!((h.elapsed_fraction(ts("2026-11-15T17:15:00Z")) - 0.25).abs() < 1e-12);
        assert!((h.remaining_fraction(ts("2026-11-15T17:15:00Z")) - 0.75).abs() < 1e-12);
        assert_eq!(h.elapsed_fraction(ts("2026-11-15T16:00:00Z")), 0.0);
        assert_eq!(h.elapsed_fraction(ts("2026-11-15T19:00:00Z")), 1.0);
    }

    #[test]
    fn local_date_crosses_midnight_in_local_time() {
        let cal = oslo();
        assert_eq!(cal.time_zone().iana_name(), Some("Europe/Oslo"));
        // 23:30 UTC on 14 Nov is 00:30 local on 15 Nov (UTC+1 in winter).
        let h = HourStart::containing(ts("2026-11-14T23:30:00Z"));
        assert_eq!(cal.date_of(h), date(2026, 11, 15));
        assert_eq!(
            cal.month_of(HourStart::containing(ts("2026-11-30T23:00:00Z"))),
            LocalMonth {
                year: 2026,
                month: 12
            }
        );
    }

    #[test]
    fn ordinary_day_has_24_hours() {
        let hours = oslo().hours_of(date(2026, 11, 15)).unwrap();
        assert_eq!(hours.len(), 24);
        assert_eq!(hours[0].start(), ts("2026-11-14T23:00:00Z"));
    }

    #[test]
    fn spring_forward_day_has_23_hours() {
        // 2026-03-29: clocks jump 02:00 → 03:00 local.
        let hours = oslo().hours_of(date(2026, 3, 29)).unwrap();
        assert_eq!(hours.len(), 23);
        assert_eq!(hours[0].start(), ts("2026-03-28T23:00:00Z"));
        assert_eq!(hours[22].start(), ts("2026-03-29T21:00:00Z"));
    }

    #[test]
    fn fall_back_day_has_25_hours() {
        // 2026-10-25: clocks fall back 03:00 → 02:00 local, so 02:xx happens twice.
        let cal = oslo();
        let hours = cal.hours_of(date(2026, 10, 25)).unwrap();
        assert_eq!(hours.len(), 25);
        assert_eq!(hours[0].start(), ts("2026-10-24T22:00:00Z"));
        // Both 02:00 local hours (00:00Z and 01:00Z) belong to the 25th.
        assert_eq!(
            cal.date_of(HourStart::containing(ts("2026-10-25T00:00:00Z"))),
            date(2026, 10, 25)
        );
        assert_eq!(
            cal.date_of(HourStart::containing(ts("2026-10-25T01:00:00Z"))),
            date(2026, 10, 25)
        );
    }

    #[test]
    fn calendar_range_errors_are_reported() {
        let err = oslo().hours_of(date(9999, 12, 31)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("calendar arithmetic out of range")
        );
    }

    proptest! {
        /// Every hour of a local day maps back to that day, days have 23/24/25 hours, and
        /// consecutive days tile time without gaps or overlaps.
        #[test]
        fn local_days_tile_utc_hours(days in 0i32..36_500) {
            let cal = oslo();
            let d = date(2000, 1, 1).checked_add(jiff::Span::new().days(days)).unwrap();
            let hours = cal.hours_of(d).unwrap();
            prop_assert!(matches!(hours.len(), 23..=25));
            for h in &hours {
                prop_assert_eq!(cal.date_of(*h), d);
            }
            let next = cal.hours_of(d.tomorrow().unwrap()).unwrap();
            prop_assert_eq!(hours.last().unwrap().next(), next[0]);
        }

        /// Any instant lies inside the hour that contains it.
        #[test]
        fn hour_contains_instant(secs in -2_000_000_000i64..4_000_000_000, nanos in 0i32..1_000_000_000) {
            let at = Timestamp::new(secs, nanos).unwrap();
            let h = HourStart::containing(at);
            prop_assert!(h.start() <= at && at < h.end());
            prop_assert_eq!(h.start().as_second().rem_euclid(3600), 0);
            prop_assert_eq!(h.start().subsec_nanosecond(), 0);
        }
    }
}
