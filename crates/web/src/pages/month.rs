//! The month page: the tier tiles, each day's highest hour, and a day × hour heatmap.

use energybrain_view::chart::{Linear, bar, value_axis};
use energybrain_view::month::MonthPage as Month;
use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::load_month;
use crate::header::TileView;
use crate::pages::refresh_every_minute;

#[component]
pub fn MonthPage() -> impl IntoView {
    let query = use_query_map();
    let month = move || query.read().get("month");
    let tick = RwSignal::new(0_u32);
    let page = Resource::new(move || (month(), tick.get()), |(m, _)| load_month(m));
    refresh_every_minute(tick, move || {
        page.get_untracked()
            .and_then(Result::ok)
            .is_some_and(|p| p.is_current)
    });
    view! {
        <Transition fallback=|| view! { <p class="empty">"Loading…"</p> }>
            {move || page.get().map(|r| match r {
                Ok(p) => view! { <MonthView page=p/> }.into_any(),
                Err(e) => view! { <p class="empty">"Could not load the month: " {e.to_string()}</p> }.into_any(),
            })}
        </Transition>
    }
}

fn day_href(date: &str) -> String {
    format!("/?date={date}")
}

#[component]
fn MonthView(page: Month) -> impl IntoView {
    let tiles = page
        .tiles
        .iter()
        .cloned()
        .map(|t| view! { <TileView tile=t/> })
        .collect_view();
    view! {
        <nav class="pager" aria-label="Month">
            <a href=format!("/month?month={}", page.prev) aria-label="Previous month">"‹"</a>
            <span class="label">{page.title.clone()}</span>
            <a href=format!("/month?month={}", page.next) aria-label="Next month">"›"</a>
        </nav>
        <div class="actions">
            {(!page.is_current).then(|| view! { <a class="ghost" href="/month">"This month"</a> })}
        </div>
        <div class="tiles">{tiles}</div>
        <section class="card">
            <h2>"Highest hour per day"</h2>
            <p class="sub">"The three highest days set the capacity tier. Tap a day to open it."</p>
            <Peaks page=page.clone()/>
        </section>
        <section class="card">
            <h2>"Energy by hour of day"</h2>
            <p class="sub">"Each cell is one hour, coloured relative to the target. Tap a cell to open its day."</p>
            <Heatmap page/>
        </section>
    }
}

#[component]
fn Peaks(page: Month) -> impl IntoView {
    let n = i64::try_from(page.days.len()).unwrap_or(1).max(1);
    // Days are slots 0..n on a unit scale (the bar helper takes "times").
    let x = Linear::time((0, n));
    let max = page
        .days
        .iter()
        .filter_map(|d| d.peak)
        .fold(page.target.max(page.top_average), f64::max);
    let axis = value_axis(max * 1.05, 1.0);
    let y = axis.scale;
    let mut i = 0_i64;
    let bars = page
        .days
        .iter()
        .map(|d| {
            let slot = (i, i + 1);
            i += 1;
            let rect = d.peak.map(|v| bar(slot, v, x, y));
            let class = if d.top { "bar" } else { "bar other" };
            let label = format!(
                "{}: {} at {}",
                d.label,
                energybrain_view::fmt::qty(d.peak, 2, "kWh"),
                d.peak_at
            );
            let (left, width) = (
                x.at_time(slot.0) / 10.0,
                x.at_time(slot.1) / 10.0 - x.at_time(slot.0) / 10.0,
            );
            (
                rect.map(|r| {
                    view! {
                        <rect class=class x=format!("{:.1}", r.x) y=format!("{:.1}", r.y)
                            width=format!("{:.1}", r.w) height=format!("{:.1}", r.h)/>
                    }
                }),
                view! {
                    <a class="slot" href=day_href(&d.date) aria-label=label.clone() title=label
                        style:left=format!("{left:.2}%") style:width=format!("{width:.2}%")></a>
                },
            )
        })
        .collect::<Vec<_>>();
    let (rects, slots): (Vec<_>, Vec<_>) = bars.into_iter().unzip();
    let level = |v: f64, class: &'static str| {
        let ly = format!("{:.1}", y.up(v));
        view! { <line class=class x1="0" x2="1000" y1=ly.clone() y2=ly/> }
    };
    let labels = axis
        .ticks
        .iter()
        .map(|t| view! { <span class="yl" style:top=format!("{:.2}%", t.pos)>{t.label.clone()}</span> })
        .collect_view();
    let x_labels = page
        .days
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 5 == 0)
        .map(|(i, d)| {
            let i = i64::try_from(i).unwrap_or(0);
            let left = (x.at_time(i) + x.at_time(i + 1)) / 20.0;
            view! { <span class="xl" style:left=format!("{left:.2}%")>{d.label.clone()}</span> }
        })
        .collect_view();
    view! {
        <div class="chart">
            <div class="plot">
                <span class="unit">"kWh"</span>
                {labels}
                <svg viewBox="0 0 1000 1000" preserveAspectRatio="none" aria-hidden="true">
                    {rects}
                    {level(page.target, "target")}
                    {level(page.top_average, "average")}
                </svg>
                <div class="hit slots">{slots}</div>
                <div class="xaxis">{x_labels}</div>
            </div>
            <div class="legend">
                <span><i class="sw s1"></i>"Top 3"</span>
                <span><i class="sw other"></i>"Other days"</span>
                <span><i class="sw ink"></i>"Target"</span>
                <span><i class="sw s2"></i>"Top-3 average"</span>
            </div>
        </div>
    }
}

#[component]
fn Heatmap(page: Month) -> impl IntoView {
    let hours = (0..24)
        .map(|h| view! { <div class="hour">{(h % 3 == 0).then(|| format!("{h:02}"))}</div> })
        .collect_view();
    let rows = page
        .heat
        .into_iter()
        .map(|row| {
            let href = day_href(&row.date);
            let cells = row
                .cells
                .into_iter()
                .map(|c| {
                    let class = c.level.map_or_else(|| "cell".to_owned(), |l| format!("cell heat-{l}"));
                    view! { <a class=class href=href.clone() title=c.text.clone() aria-label=c.text></a> }
                })
                .collect_view();
            view! { <div class="day">{row.label}</div>{cells} }
        })
        .collect_view();
    let legend = page
        .legend
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let swatch = (i < 7).then(|| view! { <i class=format!("sw heat-{}", i + 1)></i> });
            view! { <span>{swatch}{l}</span> }
        })
        .collect_view();
    view! {
        <div class="heat"><div></div>{hours}{rows}</div>
        <div class="scale">{legend}</div>
    }
}
