//! The month page: a [`MonthView`] turned into tiles, day bars and a heatmap.

use energybrain_core::units::{Kw, Kwh};
use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use crate::fmt::{LocalClock, long_month, month_key, num, qty, short_date};
use crate::history::{MonthView, Site};
use crate::status::{Tile, Tone, tier_text};

/// Heatmap levels: an hour's energy as a fraction of the target, binned at these steps.
const STEPS: [f64; 6] = [0.2, 0.4, 0.6, 0.8, 1.0, 1.2];

/// One day of the month.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayBar {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// "Sun 15".
    pub label: String,
    /// The day's highest hour, kWh.
    pub peak: Option<f64>,
    /// When it started, "17:00" (or "–").
    pub peak_at: String,
    /// The day's total, kWh.
    pub total: Option<f64>,
    /// One of the days that set the tier.
    pub top: bool,
}

/// One cell of the heatmap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatCell {
    /// 1 (lowest) to 7 (≥ 1.2 × target); `None` without data.
    pub level: Option<u8>,
    /// "1.23 kWh, Sun 15, 17:00".
    pub text: String,
}

/// One day row of the heatmap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatRow {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// "Sun 15".
    pub label: String,
    /// Local clock hours 0–23.
    pub cells: Vec<HeatCell>,
}

/// The month page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthPage {
    /// `YYYY-MM`.
    pub month: String,
    /// "November 2026".
    pub title: String,
    /// The month before, `YYYY-MM`.
    pub prev: String,
    /// The month after.
    pub next: String,
    /// Whether this is the running month (it keeps filling in).
    pub is_current: bool,
    /// Top-3 average, tier, the top days.
    pub tiles: Vec<Tile>,
    /// The target tier's bound, kWh per hour.
    pub target: f64,
    /// The top days' average.
    pub top_average: f64,
    /// One per local day.
    pub days: Vec<DayBar>,
    /// Day × hour.
    pub heat: Vec<HeatRow>,
    /// One label per level, then a note on the target.
    pub legend: Vec<String>,
}

/// The heat level of `v` against `target`.
pub fn level(v: f64, target: f64) -> u8 {
    // At most 6 steps are passed, so the count fits.
    1 + u8::try_from(STEPS.iter().filter(|&&s| v >= s * target).count()).unwrap_or(6)
}

fn legend(target: f64) -> Vec<String> {
    let steps = STEPS.map(|s| s * target);
    let fmt = |v: f64| num(Some(v), 1);
    let mut out = vec![format!("< {}", fmt(steps[0]))];
    out.extend(
        steps
            .windows(2)
            .map(|w| format!("{}–{}", fmt(w[0]), fmt(w[1]))),
    );
    out.push(format!("≥ {} kWh", fmt(steps[5])));
    out.push(format!("Target: {} kWh per hour.", fmt(target)));
    out
}

fn shift_month(first: Date, months: i32) -> String {
    // Months next to a valid range's month always exist; the fallback keeps this total.
    first
        .checked_add(jiff::Span::new().months(months))
        .map_or_else(|_| month_key(first), month_key)
}

/// The page for `view`.
pub fn month_page(site: &Site, view: &MonthView) -> MonthPage {
    let tz = site.calendar.time_zone();
    let hm = |t: jiff::Timestamp| LocalClock::of(tz, &[t]).hm(t.as_second());
    let target = view.target.value();
    let d = &view.days;
    let first = d
        .date
        .first()
        .copied()
        .unwrap_or(view.today)
        .first_of_month();
    let mut tiles = vec![
        Tile {
            key: "Top-3 average".to_owned(),
            value: qty(Some(view.top_average.value()), 2, Kw::UNIT),
            detail: Some(format!("target {}", qty(Some(target), 0, Kw::UNIT))),
            tone: if view.top_average.value() > target {
                Tone::Alert
            } else {
                Tone::Normal
            },
        },
        Tile {
            key: "Tier".to_owned(),
            value: tier_text(&view.tier),
            detail: None,
            tone: Tone::Normal,
        },
    ];
    tiles.extend(view.top_days.iter().enumerate().map(|(i, p)| Tile {
        key: format!("Top day {}", i + 1),
        value: qty(Some(p.kwh.value()), 2, Kwh::UNIT),
        detail: Some(format!("{}, {}", short_date(p.date), hm(p.hour.start()))),
        tone: if p.kwh.value() > target {
            Tone::Warn
        } else {
            Tone::Normal
        },
    }));
    let days = d
        .date
        .iter()
        .enumerate()
        .map(|(i, date)| DayBar {
            date: date.to_string(),
            label: short_date(*date),
            peak: d.peak.values[i],
            peak_at: d.peak_hour[i].map_or_else(|| crate::fmt::DASH.to_owned(), hm),
            total: d.energy.values[i],
            top: d.top[i],
        })
        .collect();
    let heat = d
        .date
        .iter()
        .zip(&d.by_hour)
        .map(|(date, hours)| HeatRow {
            date: date.to_string(),
            label: short_date(*date),
            cells: hours
                .values
                .iter()
                .enumerate()
                .map(|(hour, v)| HeatCell {
                    level: v.map(|v| level(v, target)),
                    text: format!(
                        "{}, {}, {hour:02}:00",
                        v.map_or_else(|| "no data".to_owned(), |v| qty(Some(v), 2, Kwh::UNIT)),
                        short_date(*date)
                    ),
                })
                .collect(),
        })
        .collect();
    MonthPage {
        month: view.month.clone(),
        title: long_month(first),
        prev: shift_month(first, -1),
        next: shift_month(first, 1),
        is_current: month_key(view.today) == view.month,
        tiles,
        target,
        top_average: view.top_average.value(),
        days,
        heat,
        legend: legend(target),
    }
}

#[cfg(test)]
mod tests {
    use energybrain_core::energy::{HourEnergy, Quality};
    use energybrain_core::time::HourStart;
    use jiff::civil::date;

    use super::*;
    use crate::history::fixtures::{site, ts};
    use crate::history::{History, month_range, month_view};

    fn page(day: Date, hours: Vec<HourEnergy>) -> MonthPage {
        let site = site();
        let range = month_range(&site.calendar, day).unwrap();
        let history = History {
            hours,
            ..History::default()
        };
        month_page(
            &site,
            &month_view(&site, day, range, date(2026, 11, 15), &history),
        )
    }

    fn hour(at: &str, kwh: f64) -> HourEnergy {
        HourEnergy {
            hour: HourStart::containing(ts(at)),
            kwh: Kwh::new(kwh),
            quality: Quality::Measured,
        }
    }

    #[test]
    fn month_page_json() {
        let p = page(
            date(2026, 11, 1),
            vec![
                hour("2026-11-02T16:00:00Z", 5.6),
                hour("2026-11-02T17:00:00Z", 3.0),
                hour("2026-11-03T06:00:00Z", 2.1),
                hour("2026-11-14T17:00:00Z", 4.4),
            ],
        );
        let back: MonthPage = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        // Days 1–3 are enough to see the shape.
        let p = MonthPage {
            days: p.days[..3].to_vec(),
            heat: p.heat[1..2].to_vec(),
            ..p
        };
        insta::assert_snapshot!(serde_json::to_string_pretty(&p).unwrap());
    }

    #[test]
    fn levels_bin_against_the_target() {
        assert_eq!(
            [0.0, 0.99, 1.0, 2.5, 4.99, 5.0, 5.99, 6.0, 60.0].map(|v| level(v, 5.0)),
            [1, 1, 2, 3, 5, 6, 6, 7, 7]
        );
    }

    #[test]
    fn neighbours_and_the_running_month() {
        let p = page(date(2026, 1, 20), vec![]);
        assert_eq!((p.prev.as_str(), p.next.as_str()), ("2025-12", "2026-02"));
        assert!(!p.is_current);
        assert!(page(date(2026, 11, 3), vec![]).is_current);
        assert_eq!(shift_month(date(9999, 12, 1), 1), "9999-12");
    }

    #[test]
    fn a_top_average_over_the_target_is_an_alert() {
        let p = page(
            date(2026, 11, 1),
            ["02", "03", "04"]
                .map(|d| hour(&format!("2026-11-{d}T16:00:00Z"), 6.0))
                .to_vec(),
        );
        assert_eq!(p.tiles[0].tone, Tone::Alert);
        assert_eq!(p.tiles[4].tone, Tone::Warn);
    }
}
