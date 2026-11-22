//! Chart geometry for the web app's SVG charts, computed the same way on the server and in the
//! browser (zoom and cursor), and tested here.
//!
//! A plot is an SVG with `viewBox="0 0 1000 1000"` stretched to its box
//! (`preserveAspectRatio="none"`): x and y are both 0–1000, y pointing down. Axis labels are
//! HTML, positioned in percent of the plot ([`Tick::pos`]), so text never stretches. Times are
//! Unix seconds.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::fmt::{LocalClock, num};

/// Width and height of every plot's view box.
pub const SIZE: f64 = 1000.0;

/// A minute: how long the last sample of a run is taken to last.
const SAMPLE: i64 = 60;

/// Maps `[lo, hi]` onto `[0, SIZE]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Linear {
    lo: f64,
    hi: f64,
}

impl Linear {
    /// A scale over `[lo, hi]`; an empty or reversed range becomes `[lo, lo + 1]`.
    pub fn new(lo: f64, hi: f64) -> Self {
        Self {
            lo,
            hi: if hi > lo { hi } else { lo + 1.0 },
        }
    }

    /// A time scale.
    #[allow(clippy::cast_precision_loss)] // Unix seconds are far below 2^52.
    pub fn time((from, to): (i64, i64)) -> Self {
        Self::new(from as f64, to as f64)
    }

    /// Position along the axis, 0–[`SIZE`].
    pub fn at(&self, v: f64) -> f64 {
        (v - self.lo) / (self.hi - self.lo) * SIZE
    }

    /// Position of an instant.
    #[allow(clippy::cast_precision_loss)]
    pub fn at_time(&self, t: i64) -> f64 {
        self.at(t as f64)
    }

    /// Vertical position of a value (up is larger).
    pub fn up(&self, v: f64) -> f64 {
        SIZE - self.at(v)
    }

    /// The value at a fraction (0–1) of the axis.
    pub fn value_at(&self, fraction: f64) -> f64 {
        self.lo + fraction.clamp(0.0, 1.0) * (self.hi - self.lo)
    }
}

/// One axis label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    /// Percent along the axis: from the left for time, from the top for values.
    pub pos: f64,
    /// Label.
    pub label: String,
}

/// A value axis from 0.
#[derive(Debug, Clone, PartialEq)]
pub struct Axis {
    /// The scale.
    pub scale: Linear,
    /// Labels, bottom up.
    pub ticks: Vec<Tick>,
}

/// The smallest of 1, 2, 2.5, 5 × 10ⁿ at or above `v` (1 for nothing positive).
pub fn nice(v: f64) -> f64 {
    if v.is_nan() || v <= 0.0 {
        return 1.0;
    }
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * p)
        .find(|&c| c >= v * (1.0 - 1e-9))
        .unwrap_or(10.0 * p)
}

/// The smallest of 1, 2, 5 × 10ⁿ at or above `v`: axis steps whose multiples read well.
fn step_of(v: f64) -> f64 {
    let n = nice(v);
    let p = 10f64.powf(n.log10().floor());
    if (n / p - 2.5).abs() < 1e-9 {
        5.0 * p
    } else {
        n
    }
}

/// An axis from 0 that fits `max` (at least `min_top`), with three to five round ticks.
pub fn value_axis(max: f64, min_top: f64) -> Axis {
    let max = if max.is_finite() {
        max.max(min_top)
    } else {
        min_top
    };
    let step = step_of(max / 4.0);
    let top = (max / step).ceil().max(1.0) * step;
    let scale = Linear::new(0.0, top);
    let digits = usize::from(step < 1.0);
    let ticks = std::iter::successors(Some(0.0), |v| Some(v + step))
        .take_while(|v| *v <= top * (1.0 + 1e-9))
        .map(|v| Tick {
            pos: scale.up(v) / 10.0,
            label: num(Some(v), digits),
        })
        .collect();
    Axis { scale, ticks }
}

/// Time labels for `[from, to)`: every third local hour for long ranges, every hour for a few
/// hours, every 15 minutes within an hour or two. `hours` are the hour starts of the day, so
/// DST days label their real hours.
pub fn time_ticks(clock: &LocalClock, hours: &[i64], (from, to): (i64, i64)) -> Vec<Tick> {
    let x = Linear::time((from, to));
    let span = to - from;
    let at: Vec<i64> = if span > 6 * 3600 {
        hours
            .iter()
            .copied()
            .filter(|&h| clock.of_day(h) / 3600 % 3 == 0)
            .collect()
    } else if span > 2 * 3600 {
        hours.to_vec()
    } else {
        let first = from.div_euclid(900) * 900;
        std::iter::successors(Some(first), |t| Some(t + 900))
            .take_while(|&t| t <= to)
            .collect()
    };
    at.into_iter()
        .filter(|&t| t >= from && t <= to)
        .map(|t| Tick {
            pos: x.at_time(t) / 10.0,
            label: clock.hm(t),
        })
        .collect()
}

fn point(x: f64, y: f64) -> String {
    format!("{x:.1} {y:.1}")
}

/// A polyline through the known points; `None` breaks it.
pub fn line(xs: &[i64], ys: &[Option<f64>], x: Linear, y: Linear) -> String {
    let mut d = String::new();
    let mut open = false;
    for (&t, v) in xs.iter().zip(ys) {
        match v {
            Some(v) => {
                d.push_str(if open { " L" } else { " M" });
                d.push_str(&point(x.at_time(t), y.up(*v)));
                open = true;
            }
            None => open = false,
        }
    }
    d.trim_start().to_owned()
}

/// A step line: each value holds until the next point; `None` ends a run there.
pub fn steps(xs: &[i64], ys: &[Option<f64>], x: Linear, y: Linear) -> String {
    let mut d = String::new();
    let mut open = false;
    for (&t, v) in xs.iter().zip(ys) {
        let px = x.at_time(t);
        if open {
            let _ = write!(d, " H{px:.1}");
        }
        match v {
            Some(v) if open => {
                let _ = write!(d, " V{:.1}", y.up(*v));
            }
            Some(v) => {
                let _ = write!(d, " M{}", point(px, y.up(*v)));
                open = true;
            }
            None => open = false,
        }
    }
    d.trim_start().to_owned()
}

/// Closed shapes between `lower` and `upper` wherever both are known.
pub fn area(
    xs: &[i64],
    lower: &[Option<f64>],
    upper: &[Option<f64>],
    x: Linear,
    y: Linear,
) -> String {
    let mut shapes = Vec::new();
    let mut run: Vec<(f64, f64, f64)> = Vec::new();
    let mut close = |run: &mut Vec<(f64, f64, f64)>| {
        if !run.is_empty() {
            let top = run.iter().map(|&(px, _, hi)| point(px, hi));
            let bottom = run.iter().rev().map(|&(px, lo, _)| point(px, lo));
            let pts: Vec<String> = top.chain(bottom).collect();
            shapes.push(format!("M{} Z", pts.join(" L")));
            run.clear();
        }
    };
    for ((&t, lo), hi) in xs.iter().zip(lower).zip(upper) {
        match lo.zip(*hi) {
            Some((lo, hi)) => run.push((x.at_time(t), y.up(lo), y.up(hi))),
            None => close(&mut run),
        }
    }
    close(&mut run);
    shapes.join(" ")
}

/// `(x, width)` of each run of points where `on` holds, each point lasting until the next
/// (the last a minute).
pub fn runs(xs: &[i64], on: &[bool], x: Linear) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, (&t, &flag)) in xs.iter().zip(on).enumerate() {
        let end = xs.get(i + 1).copied().unwrap_or(t + SAMPLE);
        match (flag, start) {
            (true, None) => start = Some(t),
            (false, Some(s)) => {
                out.push((s, t));
                start = None;
            }
            _ => {}
        }
        if flag && i + 1 == xs.len() {
            out.push((start.unwrap_or(t), end));
        }
    }
    out.into_iter()
        .map(|(a, b)| {
            let x0 = x.at_time(a);
            (x0, x.at_time(b) - x0)
        })
        .collect()
}

/// The index of the sample nearest to `t`.
pub fn nearest(xs: &[i64], t: i64) -> Option<usize> {
    let i = xs.partition_point(|&x| x < t);
    match (i.checked_sub(1), xs.get(i)) {
        (Some(before), Some(&after)) => Some(if t - xs[before] <= after - t {
            before
        } else {
            i
        }),
        (Some(before), None) => Some(before),
        (None, Some(_)) => Some(0),
        (None, None) => None,
    }
}

/// A rectangle in view-box units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left.
    pub x: f64,
    /// Top.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

/// A bar over `[start, end)` from 0 to `v`, 70 % of the slot wide.
pub fn bar((start, end): (i64, i64), v: f64, x: Linear, y: Linear) -> Rect {
    let x0 = x.at_time(start);
    let w = x.at_time(end) - x0;
    let top = y.up(v.max(0.0));
    Rect {
        x: x0 + w * 0.15,
        y: top,
        w: w * 0.7,
        h: SIZE - top,
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use jiff::tz::TimeZone;

    use super::*;

    fn ts(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_second()
    }

    fn labels(ticks: &[Tick]) -> Vec<&str> {
        ticks.iter().map(|t| t.label.as_str()).collect()
    }

    #[test]
    fn scales() {
        let s = Linear::new(0.0, 4.0);
        assert!((s.at(1.0) - 250.0).abs() < 1e-9);
        assert!((s.up(1.0) - 750.0).abs() < 1e-9);
        assert!((s.value_at(0.5) - 2.0).abs() < 1e-9);
        assert!((s.value_at(2.0) - 4.0).abs() < 1e-9);
        assert_eq!(Linear::new(3.0, 3.0), Linear::new(3.0, 4.0));
        assert!((Linear::time((0, 100)).at_time(25) - 250.0).abs() < 1e-9);
    }

    #[test]
    fn nice_numbers_and_axes() {
        assert_eq!(
            [0.0, -1.0, f64::NAN, 0.3, 1.0, 1.2, 2.2, 3.0, 7.0, 47.0].map(nice),
            [1.0, 1.0, 1.0, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0, 50.0]
        );
        let axis = value_axis(4.7, 1.0);
        assert_eq!(labels(&axis.ticks), ["0", "2", "4", "6"]);
        assert_eq!(axis.ticks[3].pos, 0.0);
        assert_eq!(axis.ticks[0].pos, 100.0);
        assert_eq!(labels(&value_axis(0.2, 1.0).ticks), ["0.0", "0.5", "1.0"]);
        assert_eq!(
            labels(&value_axis(f64::NEG_INFINITY, 16.0).ticks),
            ["0", "5", "10", "15", "20"]
        );
    }

    #[test]
    fn time_ticks_follow_the_range_and_dst() {
        let tz = TimeZone::get("Europe/Oslo").unwrap();
        // 2026-10-25 has 25 hours: 02:00 twice.
        let from = ts("2026-10-24T22:00:00Z");
        let hours: Vec<i64> = (0..25).map(|i| from + i * 3600).collect();
        let stamps: Vec<Timestamp> = hours
            .iter()
            .map(|&h| Timestamp::from_second(h).unwrap())
            .collect();
        let clock = LocalClock::of(&tz, &stamps);
        let to = from + 25 * 3600;
        let day = time_ticks(&clock, &hours, (from, to));
        assert_eq!(
            labels(&day),
            [
                "00:00", "03:00", "06:00", "09:00", "12:00", "15:00", "18:00", "21:00"
            ]
        );
        assert_eq!(day[0].pos, 0.0);
        let few = time_ticks(&clock, &hours, (from + 3600, from + 4 * 3600));
        assert_eq!(labels(&few), ["01:00", "02:00", "02:00", "03:00"]);
        let hour = time_ticks(&clock, &hours, (from + 3600 + 600, from + 2 * 3600));
        assert_eq!(labels(&hour), ["01:15", "01:30", "01:45", "02:00"]);
    }

    #[test]
    fn lines_break_at_gaps() {
        let x = Linear::time((0, 100));
        let y = Linear::new(0.0, 10.0);
        let xs = [0, 10, 20, 30, 40];
        let ys = [Some(0.0), Some(5.0), None, Some(10.0), Some(5.0)];
        assert_eq!(
            line(&xs, &ys, x, y),
            "M0.0 1000.0 L100.0 500.0 M300.0 0.0 L400.0 500.0"
        );
        assert_eq!(
            steps(&xs, &ys, x, y),
            "M0.0 1000.0 H100.0 V500.0 H200.0 M300.0 0.0 H400.0 V500.0"
        );
        assert_eq!(line(&[], &[], x, y), "");
    }

    #[test]
    fn areas_close_each_known_run() {
        let x = Linear::time((0, 100));
        let y = Linear::new(0.0, 10.0);
        let xs = [0, 10, 20, 30];
        let lower = [Some(0.0), Some(0.0), Some(1.0), Some(1.0)];
        let upper = [Some(5.0), Some(5.0), None, Some(2.0)];
        assert_eq!(
            area(&xs, &lower, &upper, x, y),
            "M0.0 500.0 L100.0 500.0 L100.0 1000.0 L0.0 1000.0 Z M300.0 800.0 L300.0 900.0 Z"
        );
        // A leading gap and a trailing one leave nothing open.
        assert_eq!(
            area(&xs, &[None, Some(0.0), Some(0.0), None], &upper, x, y),
            "M100.0 500.0 L100.0 1000.0 Z"
        );
    }

    #[test]
    fn runs_of_flags() {
        let x = Linear::time((0, 1000));
        let xs = [0, 60, 120, 180, 240];
        assert_eq!(
            runs(&xs, &[true, true, false, true, true], x),
            [(0.0, 120.0), (180.0, 120.0)]
        );
        assert_eq!(
            runs(&xs, &[false, false, false, false, true], x),
            [(240.0, 60.0)]
        );
        assert_eq!(runs(&xs, &[false; 5], x), Vec::<(f64, f64)>::new());
    }

    #[test]
    fn nearest_sample() {
        let xs = [0, 60, 120];
        assert_eq!(
            [-5, 0, 29, 31, 90, 500].map(|t| nearest(&xs, t)),
            [Some(0), Some(0), Some(0), Some(1), Some(1), Some(2)]
        );
        assert_eq!(nearest(&[], 5), None);
    }

    #[test]
    fn bars_take_most_of_their_slot() {
        let r = bar(
            (0, 100),
            5.0,
            Linear::time((0, 1000)),
            Linear::new(0.0, 10.0),
        );
        assert_eq!(
            r,
            Rect {
                x: 15.0,
                y: 500.0,
                w: 70.0,
                h: 500.0
            }
        );
        assert_eq!(
            bar(
                (0, 100),
                -1.0,
                Linear::time((0, 1000)),
                Linear::new(0.0, 10.0)
            )
            .h,
            0.0
        );
    }
}
