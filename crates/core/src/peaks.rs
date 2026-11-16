//! Monthly peaks and the hourly limit.
//!
//! The capacity tier is set by the average of the month's three highest *daily maximum*
//! hours. Only the highest hour of each local day can count, which gives the guard one
//! free relaxation: once today already has an hour at some level, other hours today may
//! reach that level too without changing the bill.
//!
//! Policy (deliberately simple, see the decision log): the limit is the target tier's upper
//! bound, raised to today's highest completed hour if that is higher, minus a safety margin.
//! There is no month-end relaxation and no "tier already lost" mode.

use std::collections::BTreeMap;

use jiff::civil::Date;

use crate::energy::{HourEnergy, Quality};
use crate::time::{HourStart, LocalCalendar, LocalMonth};
use crate::units::{Kw, Kwh};

/// Number of days that set the capacity tier.
pub const TOP_DAYS: usize = 3;

/// One day's highest hour.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct DayPeak {
    /// Local date.
    pub date: Date,
    /// The day's highest hour.
    pub hour: HourStart,
    /// Its energy.
    pub kwh: Kwh,
}

/// Completed hours of one local month.
#[derive(Debug, Clone, PartialEq)]
pub struct MonthLedger {
    month: LocalMonth,
    hours: BTreeMap<HourStart, (Date, Kwh, Quality)>,
}

impl MonthLedger {
    /// An empty ledger for `month`.
    pub fn new(month: LocalMonth) -> Self {
        Self {
            month,
            hours: BTreeMap::new(),
        }
    }

    /// The month this ledger covers.
    pub fn month(&self) -> LocalMonth {
        self.month
    }

    /// Clears the ledger if `month` is a different month. Returns whether it rolled over.
    pub fn roll_to(&mut self, month: LocalMonth) -> bool {
        if month == self.month {
            return false;
        }
        *self = Self::new(month);
        true
    }

    /// Inserts or replaces an hour. Hours outside this ledger's month are ignored (they may
    /// still be persisted by the caller). Returns whether the hour was recorded.
    pub fn upsert(&mut self, cal: &LocalCalendar, e: HourEnergy) -> bool {
        let date = cal.date_of(e.hour);
        if LocalMonth::of(date) != self.month {
            return false;
        }
        self.hours.insert(e.hour, (date, e.kwh, e.quality));
        true
    }

    /// Number of recorded hours.
    pub fn len(&self) -> usize {
        self.hours.len()
    }

    /// Whether no hours are recorded.
    pub fn is_empty(&self) -> bool {
        self.hours.is_empty()
    }

    /// Each day's highest hour, ordered by date. Ties keep the earliest hour.
    pub fn day_peaks(&self) -> Vec<DayPeak> {
        let mut days: BTreeMap<Date, DayPeak> = BTreeMap::new();
        for (&hour, &(date, kwh, _)) in &self.hours {
            let candidate = DayPeak { date, hour, kwh };
            days.entry(date)
                .and_modify(|p| {
                    if kwh > p.kwh {
                        *p = candidate;
                    }
                })
                .or_insert(candidate);
        }
        days.into_values().collect()
    }

    /// The up to three highest day peaks, highest first.
    pub fn top_days(&self) -> Vec<DayPeak> {
        let mut peaks = self.day_peaks();
        peaks.sort_by(|a, b| b.kwh.total_cmp(&a.kwh).then(a.date.cmp(&b.date)));
        peaks.truncate(TOP_DAYS);
        peaks
    }

    /// Average of the top days as power (kWh/h → kW). With fewer than three days so far it
    /// averages the days there are; with none it is zero.
    pub fn top_average(&self) -> Kw {
        let top = self.top_days();
        if top.is_empty() {
            return Kw::ZERO;
        }
        #[allow(clippy::cast_precision_loss)] // at most 3
        let n = top.len() as f64;
        (top.iter().map(|p| p.kwh).sum::<Kwh>() / n).per_hour()
    }

    /// The highest completed hour on `date` before `before`, if any.
    pub fn day_max_before(&self, date: Date, before: HourStart) -> Option<Kwh> {
        self.hours
            .range(..before)
            .filter(|(_, (d, _, _))| *d == date)
            .map(|(_, (_, kwh, _))| *kwh)
            .reduce(Kwh::max)
    }
}

/// Why the limit has the value it has.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LimitBasis {
    /// The target tier's upper bound.
    TierBound,
    /// Today already has a higher hour, so up to that level is free.
    TodayAlreadyHigher {
        /// Today's highest completed hour.
        today_max: Kwh,
    },
}

/// The energy the running hour may use.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct HourLimit {
    /// Allowed energy for the hour before the safety margin.
    pub allowed: Kwh,
    /// `allowed` minus the safety margin; what the guard acts on.
    pub effective: Kwh,
    /// Where `allowed` came from.
    pub basis: LimitBasis,
}

/// The limit for `hour`, given a target tier upper bound and a safety margin.
pub fn hour_limit(
    ledger: &MonthLedger,
    cal: &LocalCalendar,
    hour: HourStart,
    target: Kw,
    margin: Kwh,
) -> HourLimit {
    let bound = target.for_one_hour();
    let (allowed, basis) = match ledger.day_max_before(cal.date_of(hour), hour) {
        Some(today_max) if today_max > bound => {
            (today_max, LimitBasis::TodayAlreadyHigher { today_max })
        }
        _ => (bound, LimitBasis::TierBound),
    };
    HourLimit {
        allowed,
        effective: allowed - margin,
        basis,
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use jiff::civil::date;
    use jiff::tz::TimeZone;
    use proptest::prelude::*;

    use super::*;

    fn cal() -> LocalCalendar {
        LocalCalendar::new(TimeZone::get("Europe/Oslo").unwrap())
    }

    fn hour(s: &str) -> HourStart {
        HourStart::containing(s.parse::<Timestamp>().unwrap())
    }

    fn e(h: HourStart, kwh: f64) -> HourEnergy {
        HourEnergy {
            hour: h,
            kwh: Kwh::new(kwh),
            quality: Quality::Measured,
        }
    }

    fn nov() -> LocalMonth {
        LocalMonth {
            year: 2026,
            month: 11,
        }
    }

    #[test]
    fn empty_ledger() {
        let l = MonthLedger::new(nov());
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
        assert_eq!(l.month(), nov());
        assert_eq!(l.top_average(), Kw::ZERO);
        assert_eq!(l.top_days(), Vec::<DayPeak>::new());
    }

    #[test]
    fn only_each_days_max_counts_and_local_days_are_used() {
        let c = cal();
        let mut l = MonthLedger::new(nov());
        // 23:00Z on the 14th is 00:00 local on the 15th.
        assert!(l.upsert(&c, e(hour("2026-11-14T23:00:00Z"), 6.0)));
        assert!(l.upsert(&c, e(hour("2026-11-15T17:00:00Z"), 4.0)));
        assert!(l.upsert(&c, e(hour("2026-11-15T18:00:00Z"), 6.0)));
        assert!(l.upsert(&c, e(hour("2026-11-14T17:00:00Z"), 3.0)));
        assert!(l.upsert(&c, e(hour("2026-11-13T17:00:00Z"), 2.0)));
        assert!(l.upsert(&c, e(hour("2026-11-12T17:00:00Z"), 1.0)));
        let peaks = l.day_peaks();
        assert_eq!(peaks.len(), 4);
        let fifteenth = peaks.iter().find(|p| p.date == date(2026, 11, 15)).unwrap();
        assert_eq!(fifteenth.kwh, Kwh::new(6.0));
        assert_eq!(
            fifteenth.hour,
            hour("2026-11-14T23:00:00Z"),
            "ties keep the earliest"
        );
        let top: Vec<_> = l.top_days().iter().map(|p| p.kwh.value()).collect();
        assert_eq!(top, [6.0, 3.0, 2.0]);
        assert!((l.top_average().value() - 11.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn upsert_replaces_and_ignores_other_months() {
        let c = cal();
        let mut l = MonthLedger::new(nov());
        let h = hour("2026-11-15T17:00:00Z");
        l.upsert(&c, e(h, 9.0));
        l.upsert(&c, e(h, 4.0));
        assert_eq!(l.len(), 1);
        assert_eq!(l.top_days()[0].kwh, Kwh::new(4.0));
        // 23:00Z on 30 Nov is already 1 Dec locally.
        assert!(!l.upsert(&c, e(hour("2026-11-30T23:00:00Z"), 1.0)));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn month_rollover_resets() {
        let c = cal();
        let mut l = MonthLedger::new(nov());
        l.upsert(&c, e(hour("2026-11-15T17:00:00Z"), 4.0));
        assert!(!l.roll_to(nov()));
        assert_eq!(l.len(), 1);
        let dec = c.month_of(hour("2026-11-30T23:00:00Z"));
        assert!(l.roll_to(dec));
        assert!(l.is_empty());
        assert_eq!(l.month(), dec);
    }

    #[test]
    fn limit_is_the_tier_bound_by_default() {
        let l = MonthLedger::new(nov());
        let lim = hour_limit(
            &l,
            &cal(),
            hour("2026-11-15T17:00:00Z"),
            Kw::new(5.0),
            Kwh::new(0.3),
        );
        assert_eq!(lim.allowed, Kwh::new(5.0));
        assert!((lim.effective.value() - 4.7).abs() < 1e-12);
        assert_eq!(lim.basis, LimitBasis::TierBound);
    }

    #[test]
    fn limit_relaxes_to_todays_earlier_peak() {
        let c = cal();
        let mut l = MonthLedger::new(nov());
        l.upsert(&c, e(hour("2026-11-15T07:00:00Z"), 7.0));
        // Yesterday's higher hour does not relax today.
        l.upsert(&c, e(hour("2026-11-14T17:00:00Z"), 9.0));
        let lim = hour_limit(
            &l,
            &c,
            hour("2026-11-15T17:00:00Z"),
            Kw::new(5.0),
            Kwh::ZERO,
        );
        assert_eq!(lim.allowed, Kwh::new(7.0));
        assert_eq!(
            lim.basis,
            LimitBasis::TodayAlreadyHigher {
                today_max: Kwh::new(7.0)
            }
        );
        // A lower earlier hour today does not change the bound.
        let mut l = MonthLedger::new(nov());
        l.upsert(&c, e(hour("2026-11-15T07:00:00Z"), 3.0));
        let lim = hour_limit(
            &l,
            &c,
            hour("2026-11-15T17:00:00Z"),
            Kw::new(5.0),
            Kwh::ZERO,
        );
        assert_eq!(lim.basis, LimitBasis::TierBound);
    }

    #[test]
    fn later_hours_do_not_relax_the_current_one() {
        let c = cal();
        let mut l = MonthLedger::new(nov());
        l.upsert(&c, e(hour("2026-11-15T18:00:00Z"), 9.0));
        let lim = hour_limit(
            &l,
            &c,
            hour("2026-11-15T17:00:00Z"),
            Kw::new(5.0),
            Kwh::ZERO,
        );
        assert_eq!(lim.basis, LimitBasis::TierBound);
    }

    proptest! {
        /// If no earlier day broke the target, ending the hour at the effective limit keeps the
        /// month within the target tier.
        #[test]
        fn allowed_value_keeps_an_unbroken_month_in_tier(
            days in prop::collection::vec(0.0f64..4.99, 0..14),
            today_before in prop::option::of(0.0f64..10.0),
        ) {
            let c = cal();
            let target = Kw::new(5.0);
            let mut l = MonthLedger::new(nov());
            for (i, kwh) in days.iter().enumerate() {
                let d = date(2026, 11, 1 + i8::try_from(i).unwrap());
                let h = c.first_hour(d).unwrap();
                l.upsert(&c, e(h.next().next(), *kwh));
            }
            let now = c.first_hour(date(2026, 11, 20)).unwrap().next().next().next();
            if let Some(kwh) = today_before {
                l.upsert(&c, e(c.first_hour(date(2026, 11, 20)).unwrap(), kwh));
            }
            let lim = hour_limit(&l, &c, now, target, Kwh::new(0.3));
            prop_assert!(lim.allowed >= target.for_one_hour());
            l.upsert(&c, e(now, lim.effective.value()));
            let today_broke = today_before.is_some_and(|k| k > 5.0);
            if !today_broke {
                // Bands are half-open, so staying in tier means strictly below the bound.
                prop_assert!(l.top_average() < target);
            }
            prop_assert_eq!(l.day_peaks().len(), days.len() + 1);
        }
    }
}
