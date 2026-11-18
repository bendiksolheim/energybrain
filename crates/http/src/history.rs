//! The history views behind `/api/day` and `/api/month`.
//!
//! The store hands over plain data ([`History`]). Everything the graphs need is worked out
//! here, under the coverage gate, so the page only draws: local day and month boundaries
//! (23- and 25-hour DST days), each hour's limit, gaps in the sample log, day peaks, the top
//! days and the tier. Series are columnar (`{"unit": "kW", "values": [...]}`), which is what
//! the plotting library wants and keeps a day of samples small.

use std::collections::BTreeMap;
use std::sync::Arc;

use energybrain_core::energy::{HourEnergy, Quality};
use energybrain_core::guard::GuardStatus;
use energybrain_core::ids::ZoneId;
use energybrain_core::io::{Sample, ZoneSample};
use energybrain_core::mode::Mode;
use energybrain_core::peaks::{DayPeak, MonthLedger};
use energybrain_core::tariff::{CapacityTariff, Tier};
use energybrain_core::time::{HourStart, LocalCalendar, LocalMonth};
use energybrain_core::units::{Celsius, Kw, Kwh};
use jiff::civil::Date;
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;

/// What the store returns for a time range.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    /// Samples, oldest first.
    pub samples: Vec<Sample>,
    /// Hourly energy, oldest first.
    pub hours: Vec<HourEnergy>,
    /// Hourly energy per zone.
    pub zone_hours: Vec<(ZoneId, HourStart, Kwh)>,
    /// Stored events (JSON) with their time, newest first.
    pub events: Vec<(Timestamp, serde_json::Value)>,
}

/// How much of the history a view needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Hourly energy only (a month).
    Hours,
    /// Hours, zone hours, samples and events (a day).
    Full,
}

/// Reads the history in `[from, to)` (runs on a blocking thread).
pub type HistoryQuery =
    Arc<dyn Fn(Timestamp, Timestamp, Detail) -> Result<History, String> + Send + Sync>;

/// Most events a day view returns.
pub const MAX_DAY_EVENTS: usize = 10_000;

/// Samples further apart than this get a gap (`null`) between them, so lines are not drawn
/// across a period the service was down.
const GAP: SignedDuration = SignedDuration::from_mins(3);

/// What the views need to know about the installation.
#[derive(Debug, Clone)]
pub struct Site {
    /// Local time.
    pub calendar: LocalCalendar,
    /// Capacity tiers.
    pub tariff: CapacityTariff,
    /// Target tier's upper bound.
    pub target: Kw,
    /// Zones as `(id, display name)`, in configuration order.
    pub zones: Vec<(ZoneId, String)>,
}

impl Site {
    /// The zone's IANA name. Zones from the configuration always have one; only a fixed
    /// offset (tests) does not.
    fn time_zone(&self) -> String {
        self.calendar
            .time_zone()
            .iana_name()
            .unwrap_or("UTC")
            .to_owned()
    }
}

/// A requested date or month that has no valid range.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RangeError(String);

/// Numbers in one unit, `null` where unknown.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Series {
    /// Unit symbol, as in `{value, unit}` quantities.
    pub unit: &'static str,
    /// One value per point.
    pub values: Vec<Option<f64>>,
}

impl Series {
    fn new(unit: &'static str, values: impl IntoIterator<Item = Option<f64>>) -> Self {
        Self {
            unit,
            values: values.into_iter().collect(),
        }
    }
}

/// `/api/day`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayView {
    /// The local date shown.
    pub date: Date,
    /// Today's local date (by the service's clock).
    pub today: Date,
    /// IANA time zone the page should show times in.
    pub time_zone: String,
    /// First instant of the day.
    pub from: Timestamp,
    /// First instant of the next day.
    pub to: Timestamp,
    /// One entry per local hour (23, 24 or 25).
    pub hours: HourColumns,
    /// The sample log, with gaps marked.
    pub samples: SampleColumns,
    /// The day's events grouped by hour, newest hour first. Hours without events are left out.
    pub event_hours: Vec<EventHour>,
}

/// One hour's events.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventHour {
    /// Index into [`DayView::hours`], for the hour's energy and limit.
    pub hour: usize,
    /// Hour start.
    pub start: Timestamp,
    /// Hour end.
    pub end: Timestamp,
    /// Whether the hour's energy went over the limit the guard acted on.
    pub over_limit: bool,
    /// The events, newest first.
    pub events: Vec<serde_json::Value>,
}

/// The hours of a day, columnar.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HourColumns {
    /// Hour starts.
    pub start: Vec<Timestamp>,
    /// Whole-house energy.
    pub energy: Series,
    /// How each hour's energy was determined.
    pub quality: Vec<Option<Quality>>,
    /// The hour's allowed energy (its last sample's), before the safety margin.
    pub allowed: Series,
    /// What the guard acted on (its last sample's).
    pub limit: Series,
    /// Energy per zone, from the thermostats' counters.
    pub zones: Vec<ZoneHours>,
}

/// One zone's hourly energy.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneHours {
    /// Zone.
    pub id: ZoneId,
    /// Display name.
    pub name: String,
    /// Energy per hour.
    pub energy: Series,
}

/// The sample log, columnar. A gap is a point with every value `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SampleColumns {
    /// Sample times.
    pub at: Vec<Timestamp>,
    /// Mode.
    pub mode: Vec<Option<Mode>>,
    /// Guard state.
    pub guard: Vec<Option<GuardStatus>>,
    /// Moving-average power.
    pub power: Series,
    /// The configured zones' reported power added up (`null` when none has reported).
    pub floors: Series,
    /// Energy used so far in the hour.
    pub used: Series,
    /// Projection for the hour.
    pub projected: Series,
    /// Allowed energy, before the safety margin.
    pub allowed: Series,
    /// What the guard acts on.
    pub limit: Series,
    /// Per zone, in configuration order.
    pub zones: Vec<ZoneColumns>,
}

/// One zone's part of the sample log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneColumns {
    /// Zone.
    pub id: ZoneId,
    /// Display name.
    pub name: String,
    /// The user's setpoint.
    pub base: Series,
    /// What the thermostat reported.
    pub reported: Series,
    /// What the engine wanted.
    pub decision: Series,
    /// Last reported power.
    pub power: Series,
    /// Whether the guard shed it.
    pub shed: Vec<Option<bool>>,
    /// Whether a manual override was active.
    pub overridden: Vec<Option<bool>>,
}

/// `/api/month`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthView {
    /// The month shown, `YYYY-MM`.
    pub month: String,
    /// Today's local date (by the service's clock).
    pub today: Date,
    /// IANA time zone.
    pub time_zone: String,
    /// First instant of the month.
    pub from: Timestamp,
    /// First instant of the next month.
    pub to: Timestamp,
    /// Target tier's upper bound.
    pub target: Kw,
    /// The up to three highest day peaks, highest first.
    pub top_days: Vec<DayPeak>,
    /// Their average.
    pub top_average: Kw,
    /// The tier that average falls in.
    pub tier: Tier,
    /// One entry per local day.
    pub days: DayColumns,
}

/// The days of a month, columnar.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayColumns {
    /// Local dates.
    pub date: Vec<Date>,
    /// The day's highest hour.
    pub peak: Series,
    /// When that hour started.
    pub peak_hour: Vec<Option<Timestamp>>,
    /// Whether the day is one of the top days.
    pub top: Vec<bool>,
    /// The day's total energy (known hours only).
    pub energy: Series,
    /// Energy per local clock hour 0–23, for a day × hour heatmap. The skipped hour of
    /// spring-forward is `null`; the repeated hour of fall-back holds the larger of the two.
    pub by_hour: Vec<Series>,
}

/// Parses `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<Date, RangeError> {
    s.parse()
        .map_err(|e| RangeError(format!("date {s:?}: {e}")))
}

/// Parses `YYYY-MM` into the month's first day.
pub fn parse_month(s: &str) -> Result<Date, RangeError> {
    format!("{s}-01")
        .parse()
        .map_err(|e| RangeError(format!("month {s:?}: {e}")))
}

/// `[first instant of date, first instant of the next day)`.
pub fn day_range(cal: &LocalCalendar, date: Date) -> Result<(Timestamp, Timestamp), RangeError> {
    range(cal, date, date.tomorrow())
}

/// `[first instant of the month, first instant of the next one)`, for any day in it.
pub fn month_range(cal: &LocalCalendar, day: Date) -> Result<(Timestamp, Timestamp), RangeError> {
    let first = day.first_of_month();
    range(cal, first, first.last_of_month().tomorrow())
}

fn range(
    cal: &LocalCalendar,
    from: Date,
    to: Result<Date, jiff::Error>,
) -> Result<(Timestamp, Timestamp), RangeError> {
    let bound = |d: Date| {
        cal.first_hour(d)
            .map(HourStart::start)
            .map_err(|e| RangeError(format!("{d}: {e}")))
    };
    let to = to.map_err(|e| RangeError(format!("after {from}: {e}")))?;
    Ok((bound(from)?, bound(to)?))
}

/// Builds the view of `date`, whose [`day_range`] is `(from, to)`.
pub fn day_view(
    site: &Site,
    date: Date,
    (from, to): (Timestamp, Timestamp),
    today: Date,
    history: History,
) -> DayView {
    let hours: Vec<HourStart> =
        std::iter::successors(Some(HourStart::containing(from)), |h| Some(h.next()))
            .take_while(|h| h.start() < to)
            .collect();
    let columns = hour_columns(site, &hours, &history);
    DayView {
        date,
        today,
        time_zone: site.time_zone(),
        from,
        to,
        event_hours: event_hours(&hours, &columns, history.events),
        hours: columns,
        samples: sample_columns(site, &history.samples),
    }
}

/// Groups events (newest first) by the hour they fall in, keeping that order.
fn event_hours(
    hours: &[HourStart],
    columns: &HourColumns,
    events: Vec<(Timestamp, serde_json::Value)>,
) -> Vec<EventHour> {
    let mut groups: Vec<EventHour> = Vec::new();
    for (at, event) in events {
        // The store only returns events inside the day, so `at` is never before the first
        // hour; saturating keeps this total anyway.
        let hour = hours.partition_point(|h| h.start() <= at).saturating_sub(1);
        match groups.last_mut() {
            Some(g) if g.hour == hour => g.events.push(event),
            _ => {
                // `hours` covers the whole day (23–25 entries), and the columns have one
                // value per hour, so `hour` is in range.
                let start = hours[hour];
                groups.push(EventHour {
                    hour,
                    start: start.start(),
                    end: start.next().start(),
                    over_limit: columns.energy.values[hour]
                        .zip(columns.limit.values[hour])
                        .is_some_and(|(e, l)| e > l),
                    events: vec![event],
                });
            }
        }
    }
    groups
}

fn hour_columns(site: &Site, hours: &[HourStart], history: &History) -> HourColumns {
    let energy: BTreeMap<HourStart, &HourEnergy> =
        history.hours.iter().map(|e| (e.hour, e)).collect();
    // Later samples overwrite earlier ones: each hour keeps its last.
    let last: BTreeMap<HourStart, &Sample> = history
        .samples
        .iter()
        .map(|s| (HourStart::containing(s.at), s))
        .collect();
    let per_hour = |f: &dyn Fn(HourStart) -> Option<f64>| -> Vec<Option<f64>> {
        hours.iter().map(|&h| f(h)).collect()
    };
    let zones = site
        .zones
        .iter()
        .map(|(id, name)| {
            let kwh: BTreeMap<HourStart, Kwh> = history
                .zone_hours
                .iter()
                .filter(|(z, _, _)| z == id)
                .map(|&(_, h, kwh)| (h, kwh))
                .collect();
            ZoneHours {
                id: id.clone(),
                name: name.clone(),
                energy: Series::new(Kwh::UNIT, per_hour(&|h| kwh.get(&h).map(|k| k.value()))),
            }
        })
        .collect();
    HourColumns {
        start: hours.iter().map(|h| h.start()).collect(),
        energy: Series::new(
            Kwh::UNIT,
            per_hour(&|h| energy.get(&h).map(|e| e.kwh.value())),
        ),
        quality: hours
            .iter()
            .map(|h| energy.get(h).map(|e| e.quality))
            .collect(),
        allowed: Series::new(
            Kwh::UNIT,
            per_hour(&|h| last.get(&h).map(|s| s.allowed.value())),
        ),
        limit: Series::new(
            Kwh::UNIT,
            per_hour(&|h| last.get(&h).map(|s| s.limit.value())),
        ),
        zones,
    }
}

/// Samples with a `None` row inserted wherever two samples are more than [`GAP`] apart.
fn with_gaps(samples: &[Sample]) -> Vec<(Timestamp, Option<&Sample>)> {
    let mut rows = Vec::with_capacity(samples.len());
    let mut prev: Option<Timestamp> = None;
    for s in samples {
        if let Some(p) = prev.filter(|p| s.at.duration_since(*p) > GAP) {
            rows.push((p + SignedDuration::from_mins(1), None));
        }
        rows.push((s.at, Some(s)));
        prev = Some(s.at);
    }
    rows
}

fn sample_columns(site: &Site, samples: &[Sample]) -> SampleColumns {
    let rows = with_gaps(samples);
    let col = |unit, f: &dyn Fn(&Sample) -> Option<f64>| {
        Series::new(unit, rows.iter().map(|(_, s)| s.and_then(f)))
    };
    let zones = site
        .zones
        .iter()
        .map(|(id, name)| {
            let parts: Vec<Option<&ZoneSample>> = rows
                .iter()
                .map(|(_, s)| s.and_then(|s| s.zones.iter().find(|z| &z.id == id)))
                .collect();
            let col = |unit, f: &dyn Fn(&ZoneSample) -> Option<f64>| {
                Series::new(unit, parts.iter().map(|z| z.and_then(f)))
            };
            ZoneColumns {
                id: id.clone(),
                name: name.clone(),
                base: col(Celsius::UNIT, &|z| z.base.map(Celsius::value)),
                reported: col(Celsius::UNIT, &|z| z.reported.map(Celsius::value)),
                decision: col(Celsius::UNIT, &|z| z.decision.map(Celsius::value)),
                power: col(Kw::UNIT, &|z| z.power.map(Kw::value)),
                shed: parts.iter().map(|z| z.map(|z| z.shed)).collect(),
                overridden: parts.iter().map(|z| z.map(|z| z.overridden)).collect(),
            }
        })
        .collect();
    SampleColumns {
        at: rows.iter().map(|(at, _)| *at).collect(),
        mode: rows.iter().map(|(_, s)| s.map(|s| s.mode)).collect(),
        guard: rows.iter().map(|(_, s)| s.map(|s| s.guard)).collect(),
        power: col(Kw::UNIT, &|s| s.power.map(Kw::value)),
        floors: col(Kw::UNIT, &|s| floors(site, s)),
        used: col(Kwh::UNIT, &|s| Some(s.used.value())),
        projected: col(Kwh::UNIT, &|s| s.projected.map(Kwh::value)),
        allowed: col(Kwh::UNIT, &|s| Some(s.allowed.value())),
        limit: col(Kwh::UNIT, &|s| Some(s.limit.value())),
        zones,
    }
}

/// The configured zones' power in `s` added up, if any of them has reported.
fn floors(site: &Site, s: &Sample) -> Option<f64> {
    s.zones
        .iter()
        .filter(|z| site.zones.iter().any(|(id, _)| *id == z.id))
        .filter_map(|z| z.power.map(Kw::value))
        .reduce(|a, b| a + b)
}

/// Builds the view of the month containing `day`, whose [`month_range`] is `(from, to)`.
pub fn month_view(
    site: &Site,
    day: Date,
    (from, to): (Timestamp, Timestamp),
    today: Date,
    history: &History,
) -> MonthView {
    let cal = &site.calendar;
    let mut ledger = MonthLedger::new(LocalMonth::of(day));
    for e in &history.hours {
        // Hours outside the month are skipped by the ledger (and by the per-day maps below).
        let _ = ledger.upsert(cal, *e);
    }
    let top_days = ledger.top_days();
    let top_average = ledger.top_average();
    let peaks: BTreeMap<Date, DayPeak> = ledger
        .day_peaks()
        .into_iter()
        .map(|p| (p.date, p))
        .collect();
    let mut by_day: BTreeMap<Date, [Option<f64>; 24]> = BTreeMap::new();
    let mut totals: BTreeMap<Date, f64> = BTreeMap::new();
    for e in &history.hours {
        let local = cal.time_zone().to_datetime(e.hour.start());
        let slot = &mut by_day.entry(local.date()).or_insert([None; 24])
            [usize::try_from(local.hour()).unwrap_or_default()];
        *slot = Some(slot.map_or(e.kwh.value(), |v| v.max(e.kwh.value())));
        *totals.entry(local.date()).or_default() += e.kwh.value();
    }
    let first = day.first_of_month();
    let dates: Vec<Date> = std::iter::successors(Some(first), |d| d.tomorrow().ok())
        .take_while(|d| d.month() == first.month())
        .collect();
    MonthView {
        month: format!("{:04}-{:02}", first.year(), first.month()),
        today,
        time_zone: site.time_zone(),
        from,
        to,
        target: site.target,
        tier: site.tariff.tier_for(top_average),
        days: DayColumns {
            peak: Series::new(
                Kwh::UNIT,
                dates.iter().map(|d| peaks.get(d).map(|p| p.kwh.value())),
            ),
            peak_hour: dates
                .iter()
                .map(|d| peaks.get(d).map(|p| p.hour.start()))
                .collect(),
            top: dates
                .iter()
                .map(|d| top_days.iter().any(|t| t.date == *d))
                .collect(),
            energy: Series::new(Kwh::UNIT, dates.iter().map(|d| totals.get(d).copied())),
            by_hour: dates
                .iter()
                .map(|d| Series::new(Kwh::UNIT, by_day.get(d).copied().unwrap_or([None; 24])))
                .collect(),
            date: dates,
        },
        top_days,
        top_average,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use energybrain_core::tariff::Step;
    use energybrain_core::units::Kr;
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    use super::*;

    pub(crate) fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn id(s: &str) -> ZoneId {
        ZoneId::new(s).unwrap()
    }

    pub(crate) fn site() -> Site {
        let step = |to, price| Step {
            to: Kw::new(to),
            price: Kr::new(price),
        };
        Site {
            calendar: LocalCalendar::new(TimeZone::get("Europe/Oslo").unwrap()),
            tariff: CapacityTariff::new(
                vec![step(2.0, 131.0), step(5.0, 233.0), step(10.0, 397.0)],
                Kr::new(585.0),
            )
            .unwrap(),
            target: Kw::new(5.0),
            zones: vec![
                (id("bathroom"), "Bathroom".to_owned()),
                (id("basement"), "Basement".to_owned()),
            ],
        }
    }

    fn hour(at: &str, kwh: f64, quality: Quality) -> HourEnergy {
        HourEnergy {
            hour: HourStart::containing(ts(at)),
            kwh: Kwh::new(kwh),
            quality,
        }
    }

    pub(crate) fn sample(at: &str, used: f64, shed: bool) -> Sample {
        Sample {
            at: ts(at),
            mode: Mode::Observe,
            guard: if shed {
                GuardStatus::Shedding
            } else {
                GuardStatus::Idle
            },
            power: Some(Kw::new(6.0)),
            used: Kwh::new(used),
            projected: Some(Kwh::new(5.2)),
            allowed: Kwh::new(5.0),
            limit: Kwh::new(4.7),
            zones: vec![
                ZoneSample {
                    id: id("bathroom"),
                    base: Some(Celsius::new(26.5)),
                    reported: Some(Celsius::new(if shed { 22.5 } else { 26.5 })),
                    decision: Some(Celsius::new(if shed { 22.5 } else { 26.5 })),
                    power: Some(Kw::new(1.4)),
                    shed,
                    overridden: false,
                },
                // A zone that is not configured any more: ignored.
                ZoneSample {
                    id: id("gone"),
                    base: None,
                    reported: None,
                    decision: None,
                    power: None,
                    shed: false,
                    overridden: true,
                },
            ],
        }
    }

    pub(crate) fn day_history() -> History {
        History {
            samples: vec![
                sample("2026-11-15T16:58:00Z", 4.3, false),
                sample("2026-11-15T16:59:00Z", 4.4, true),
                sample("2026-11-15T17:00:00Z", 0.1, true),
                // The service was down for ten minutes.
                sample("2026-11-15T17:10:00Z", 1.0, false),
            ],
            hours: vec![
                hour("2026-11-15T15:00:00Z", 3.9, Quality::Measured),
                hour("2026-11-15T16:00:00Z", 4.5, Quality::Interpolated),
                hour("2026-11-15T17:00:00Z", 1.2, Quality::Estimated),
            ],
            zone_hours: vec![
                (
                    id("bathroom"),
                    HourStart::containing(ts("2026-11-15T16:00:00Z")),
                    Kwh::new(1.1),
                ),
                (
                    id("gone"),
                    HourStart::containing(ts("2026-11-15T16:00:00Z")),
                    Kwh::new(9.0),
                ),
            ],
            // Newest first, as the store returns them.
            events: ["17:05:30", "17:05:00", "16:59:00", "15:30:00"]
                .map(|t| {
                    let at = format!("2026-11-15T{t}Z");
                    (ts(&at), serde_json::json!({ "at": at }))
                })
                .to_vec(),
        }
    }

    fn view_of_day(date: Date, history: History) -> DayView {
        let range = day_range(&site().calendar, date).unwrap();
        day_view(
            &site(),
            date,
            range,
            jiff::civil::date(2026, 11, 15),
            history,
        )
    }

    fn json<T: Serialize>(v: &T) -> String {
        serde_json::to_string_pretty(v).unwrap()
    }

    #[test]
    fn parses_dates_and_months() {
        assert_eq!(parse_date("2026-11-15").unwrap(), date(2026, 11, 15));
        assert!(
            parse_date("15.11.2026")
                .unwrap_err()
                .to_string()
                .starts_with("date \"15.11.2026\": ")
        );
        assert_eq!(parse_month("2026-11").unwrap(), date(2026, 11, 1));
        assert!(parse_month("2026-11-15").is_err());
        assert!(
            parse_month("2026-13")
                .unwrap_err()
                .to_string()
                .starts_with("month \"2026-13\": ")
        );
    }

    #[test]
    fn ranges_follow_local_time_and_dst() {
        let cal = site().calendar;
        assert_eq!(
            day_range(&cal, date(2026, 11, 15)).unwrap(),
            (ts("2026-11-14T23:00:00Z"), ts("2026-11-15T23:00:00Z"))
        );
        // Fall-back day: 25 hours.
        assert_eq!(
            day_range(&cal, date(2026, 10, 25)).unwrap(),
            (ts("2026-10-24T22:00:00Z"), ts("2026-10-25T23:00:00Z"))
        );
        assert_eq!(
            month_range(&cal, date(2026, 10, 17)).unwrap(),
            (ts("2026-09-30T22:00:00Z"), ts("2026-10-31T23:00:00Z"))
        );
        assert!(
            day_range(&cal, Date::MAX)
                .unwrap_err()
                .to_string()
                .starts_with("after 9999-12-31: ")
        );
        assert!(
            month_range(&cal, Date::MAX)
                .unwrap_err()
                .to_string()
                .starts_with("after 9999-12-01: ")
        );
        // The last day exists as a date, but its first instant in Oslo is past jiff's range.
        assert!(
            day_range(&cal, date(9999, 12, 30))
                .unwrap_err()
                .to_string()
                .starts_with("9999-12-31: ")
        );
    }

    #[test]
    fn day_view_json() {
        let view = view_of_day(date(2026, 11, 15), day_history());
        assert_eq!(view.hours.start.len(), 24);
        assert_eq!(view.samples.at.len(), 5, "four samples and a gap");
        assert_eq!(view.samples.at[3], ts("2026-11-15T17:01:00Z"));
        insta::assert_snapshot!(json(&view));
    }

    #[test]
    fn day_view_on_fall_back_has_25_hours() {
        let view = view_of_day(date(2026, 10, 25), History::default());
        assert_eq!(view.hours.start.len(), 25);
        assert_eq!(view.hours.start[2], ts("2026-10-25T00:00:00Z"));
        assert_eq!(view.hours.start[3], ts("2026-10-25T01:00:00Z"));
        assert_eq!(view.samples.at, Vec::<Timestamp>::new());
        assert_eq!(view.hours.energy.values, vec![None; 25]);
    }

    #[test]
    fn events_are_grouped_by_hour_newest_first() {
        let mut history = day_history();
        history.hours[1].kwh = Kwh::new(4.8);
        let view = view_of_day(date(2026, 11, 15), history);
        let groups: Vec<_> = view
            .event_hours
            .iter()
            .map(|g| (g.hour, g.start, g.end, g.over_limit, g.events.len()))
            .collect();
        assert_eq!(
            groups,
            [
                // 18:00 local, 1.2 kWh of 4.7.
                (
                    18,
                    ts("2026-11-15T17:00:00Z"),
                    ts("2026-11-15T18:00:00Z"),
                    false,
                    2
                ),
                // 4.8 kWh against a 4.7 kWh limit.
                (
                    17,
                    ts("2026-11-15T16:00:00Z"),
                    ts("2026-11-15T17:00:00Z"),
                    true,
                    1
                ),
                // Energy known, but no sample, so no limit to compare with.
                (
                    16,
                    ts("2026-11-15T15:00:00Z"),
                    ts("2026-11-15T16:00:00Z"),
                    false,
                    1
                ),
            ]
        );
        assert_eq!(view.event_hours[0].events[0]["at"], "2026-11-15T17:05:30Z");
    }

    #[test]
    fn events_in_the_repeated_fall_back_hour_stay_apart() {
        let at = |s: &str| (ts(s), serde_json::json!({ "at": s }));
        let history = History {
            events: vec![at("2026-10-25T01:10:00Z"), at("2026-10-25T00:10:00Z")],
            ..History::default()
        };
        let view = view_of_day(date(2026, 10, 25), history);
        let hours: Vec<_> = view.event_hours.iter().map(|g| (g.hour, g.start)).collect();
        // Both are 02:10 local, in the first and the second 02:00 hour.
        assert_eq!(
            hours,
            [
                (3, ts("2026-10-25T01:00:00Z")),
                (2, ts("2026-10-25T00:00:00Z"))
            ]
        );
    }

    #[test]
    fn month_view_json() {
        let history = History {
            hours: vec![
                // Fall-back day: two hours start at 02:00 local; the heatmap keeps the larger.
                hour("2026-10-25T00:00:00Z", 2.0, Quality::Measured),
                hour("2026-10-25T01:00:00Z", 3.0, Quality::Measured),
                hour("2026-10-25T16:00:00Z", 4.5, Quality::Measured),
                hour("2026-10-01T16:00:00Z", 6.0, Quality::Measured),
                hour("2026-10-02T16:00:00Z", 1.0, Quality::Estimated),
                hour("2026-10-03T16:00:00Z", 5.5, Quality::Interpolated),
                // Outside the month: ignored.
                hour("2026-11-01T16:00:00Z", 9.0, Quality::Measured),
            ],
            ..History::default()
        };
        let day = date(2026, 10, 17);
        let range = month_range(&site().calendar, day).unwrap();
        let view = month_view(&site(), day, range, date(2026, 10, 25), &history);
        assert_eq!(view.days.date.len(), 31);
        let oct25 = &view.days.by_hour[24].values;
        assert_eq!(oct25[2], Some(3.0));
        assert_eq!(oct25[17], Some(4.5), "16:00 UTC is 17:00 local in winter");
        assert_eq!(view.days.energy.values[24], Some(9.5));
        assert_eq!(view.top_average, Kw::new(16.0 / 3.0));
        assert_eq!(view.tier.index.0, 2);
        let n = view.days.date.len();
        let compact = MonthView {
            days: DayColumns {
                date: view.days.date[..3].to_vec(),
                peak: Series::new(Kwh::UNIT, view.days.peak.values[..3].to_vec()),
                peak_hour: view.days.peak_hour[..3].to_vec(),
                top: view.days.top[..3].to_vec(),
                energy: Series::new(Kwh::UNIT, view.days.energy.values[..3].to_vec()),
                by_hour: view.days.by_hour[..1].to_vec(),
            },
            ..view
        };
        assert_eq!(n, 31);
        insta::assert_snapshot!(json(&compact));
    }

    #[test]
    fn floors_add_up_the_configured_zones_that_reported() {
        let mut s = sample("2026-11-15T17:00:00Z", 1.0, false);
        let mut basement = s.zones[0].clone();
        basement.id = id("basement");
        basement.power = Some(Kw::new(0.5));
        s.zones.push(basement);
        assert_eq!(
            floors(&site(), &s),
            Some(1.9),
            "bathroom 1.4 + basement 0.5, not the unknown zone"
        );
        s.zones.iter_mut().for_each(|z| z.power = None);
        assert_eq!(floors(&site(), &s), None);
    }

    #[test]
    fn site_without_a_named_zone_reports_utc() {
        let mut s = site();
        s.calendar = LocalCalendar::new(TimeZone::fixed(jiff::tz::offset(1)));
        assert_eq!(s.time_zone(), "UTC");
    }
}
