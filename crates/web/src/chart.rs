//! SVG chart components. All geometry comes from `energybrain_view::chart`; these only put it
//! into markup and turn taps into a cursor time.

use std::sync::Arc;

use energybrain_view::chart::{
    Linear, SIZE, Tick, area, line, nearest, runs, steps, time_ticks, value_axis,
};
use energybrain_view::fmt::{LocalClock, qty};
use leptos::prelude::*;

/// What every minute chart of a page shares: the visible range and the cursor.
#[derive(Clone, Copy)]
pub struct Frame {
    /// `[from, to)` shown (the day, or the zoomed hour).
    pub range: Signal<(i64, i64)>,
    /// The time the user last pointed at.
    pub cursor: RwSignal<Option<i64>>,
}

/// How a series is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Straight lines between samples.
    Line,
    /// Each value holds until the next sample.
    Step,
    /// Not drawn; only in the readout.
    Readout,
}

/// One series.
#[derive(Clone)]
pub struct Series {
    /// Legend name.
    pub name: &'static str,
    /// Colour class (`s1`, `s2`, `s3`, `ink`, `muted`).
    pub class: &'static str,
    /// Drawing.
    pub kind: Kind,
    /// One value per sample.
    pub values: Arc<Vec<Option<f64>>>,
    /// Unit for the readout.
    pub unit: &'static str,
    /// Decimals in the readout.
    pub digits: usize,
}

/// Shading where a flag holds.
#[derive(Clone)]
pub struct Shade {
    /// Colour class (`shed`, `unreachable`, `stale`, `override`, `heating`).
    pub class: &'static str,
    /// One flag per sample.
    pub on: Arc<Vec<bool>>,
    /// A thin strip along the bottom instead of the full height.
    pub strip: bool,
}

/// A filled area between two series.
#[derive(Clone)]
pub struct Fill {
    /// Colour class.
    pub class: &'static str,
    /// Lower edge.
    pub lower: Arc<Vec<Option<f64>>>,
    /// Upper edge.
    pub upper: Arc<Vec<Option<f64>>>,
}

/// Turns a pointer position into a cursor time.
fn point_at(e: &leptos::ev::PointerEvent, frame: Frame) {
    let el: leptos::web_sys::HtmlElement = event_target(e);
    let width = el.client_width();
    if width > 0 {
        let fraction = f64::from(e.offset_x()) / f64::from(width);
        #[allow(clippy::cast_possible_truncation)] // a time inside the visible range
        let t = Linear::time(frame.range.get_untracked())
            .value_at(fraction)
            .round() as i64;
        frame.cursor.set(Some(t));
    }
}

/// The value axis labels and grid lines.
fn value_grid(ticks: &[Tick]) -> (impl IntoView + use<>, impl IntoView + use<>) {
    let labels = ticks
        .iter()
        .map(|t| view! { <span class="yl" style:top=format!("{:.2}%", t.pos)>{t.label.clone()}</span> })
        .collect_view();
    let lines = ticks
        .iter()
        .map(|t| {
            let y = format!("{:.1}", t.pos * 10.0);
            view! { <line class="gridline" x1="0" x2="1000" y1=y.clone() y2=y/> }
        })
        .collect_view();
    (labels, lines)
}

/// A minute chart over the frame's range, with the shared cursor and a readout under it.
#[allow(clippy::too_many_lines)] // one view, easier to follow in one piece
#[component]
pub fn TimeChart(
    frame: Frame,
    /// Sample times.
    at: Arc<Vec<i64>>,
    /// The day's hour starts (for time labels).
    hours: Arc<Vec<i64>>,
    clock: LocalClock,
    series: Vec<Series>,
    #[prop(default = vec![])] shades: Vec<Shade>,
    #[prop(default = None)] fill: Option<Fill>,
    /// Axis unit.
    unit: &'static str,
    /// The axis reaches at least this.
    min_top: f64,
    /// A lower chart (zones).
    #[prop(optional)]
    short: bool,
) -> impl IntoView {
    let max = series
        .iter()
        .filter(|s| s.kind != Kind::Readout)
        .flat_map(|s| s.values.iter().flatten().copied())
        .chain(fill.iter().flat_map(|f| f.upper.iter().flatten().copied()))
        .fold(0.0, f64::max);
    let axis = value_axis(max * 1.05, min_top);
    let y = axis.scale;
    let (y_labels, grid) = value_grid(&axis.ticks);
    let x = move || Linear::time(frame.range.get());

    let paths = {
        let (at, series) = (at.clone(), series.clone());
        move || {
            let x = x();
            series
                .iter()
                .filter(|s| s.kind != Kind::Readout)
                .map(|s| {
                    let d = match s.kind {
                        Kind::Step => steps(&at, &s.values, x, y),
                        Kind::Line | Kind::Readout => line(&at, &s.values, x, y),
                    };
                    view! { <path class=format!("series {}", s.class) d=d/> }
                })
                .collect_view()
        }
    };
    let shading = {
        let at = at.clone();
        move || {
            let x = x();
            shades
                .iter()
                .flat_map(|s| {
                    let (y, h) = if s.strip {
                        ("940", "60")
                    } else {
                        ("0", "1000")
                    };
                    runs(&at, &s.on, x).into_iter().map(move |(x0, w)| {
                        view! {
                            <rect class=format!("band {}", s.class) x=format!("{x0:.1}") y=y
                                width=format!("{w:.1}") height=h/>
                        }
                    })
                })
                .collect_view()
        }
    };
    let filled = {
        let at = at.clone();
        move || {
            fill.as_ref().map(|f| {
                view! { <path class=format!("fill {}", f.class) d=area(&at, &f.lower, &f.upper, x(), y)/> }
            })
        }
    };
    let cursor_line = move || {
        let (from, to) = frame.range.get();
        frame
            .cursor
            .get()
            .filter(|t| (from..=to).contains(t))
            .map(|t| {
                let px = format!("{:.1}", Linear::time((from, to)).at_time(t));
                view! { <line class="cursor" x1=px.clone() x2=px y1="0" y2="1000"/> }
            })
    };
    let x_labels = {
        let clock = clock.clone();
        move || {
            time_ticks(&clock, &hours, frame.range.get())
                .into_iter()
                .map(|t| view! { <span class="xl" style:left=format!("{:.2}%", t.pos)>{t.label}</span> })
                .collect_view()
        }
    };
    let readout = move || {
        let i = frame
            .cursor
            .get()
            .and_then(|t| nearest(&at, t))
            .or_else(|| at.len().checked_sub(1))?;
        let time = clock.hm(at[i]);
        let values = series
            .iter()
            .map(|s| {
                view! {
                    <span><i class=format!("sw {}", s.class)></i>{s.name} " "
                        <b>{qty(s.values[i], s.digits, s.unit)}</b></span>
                }
            })
            .collect_view();
        Some(view! { <span class="time">{time}</span>{values} })
    };
    let view_box = format!("0 0 {SIZE} {SIZE}");
    view! {
        <div class="chart">
            <div class=if short { "plot short" } else { "plot" }>
                <span class="unit">{unit}</span>
                {y_labels}
                <svg viewBox=view_box preserveAspectRatio="none" aria-hidden="true">
                    {shading}
                    {grid}
                    {filled}
                    {paths}
                    {cursor_line}
                </svg>
                <div class="hit"
                    on:pointerdown=move |e| point_at(&e, frame)
                    on:pointermove=move |e| point_at(&e, frame)></div>
                <div class="xaxis">{x_labels}</div>
            </div>
            <div class="legend">{readout}</div>
        </div>
    }
}
