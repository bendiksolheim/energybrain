//! The day page: energy per hour (tap an hour to zoom), then minute charts that share the
//! visible range and the cursor.

use std::sync::Arc;

use energybrain_view::chart::{Linear, bar, value_axis};
use energybrain_view::day::{Band, DayPage as Day, EvSamples, HourBar};
use energybrain_view::fmt::LocalClock;
use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::load_day;
use crate::chart::{Fill, Frame, Kind, Series, Shade, TimeChart};
use crate::pages::refresh_every_minute;

#[component]
pub fn DayPage() -> impl IntoView {
    let query = use_query_map();
    let date = move || query.read().get("date");
    let tick = RwSignal::new(0_u32);
    let page = Resource::new(move || (date(), tick.get()), |(d, _)| load_day(d));
    refresh_every_minute(tick, move || {
        page.get_untracked()
            .and_then(Result::ok)
            .is_some_and(|p| p.is_today)
    });
    let cursor = RwSignal::new(None::<i64>);
    view! {
        <Transition fallback=|| view! { <p class="empty">"Loading…"</p> }>
            {move || page.get().map(|r| match r {
                Ok(p) => view! { <DayView page=p cursor/> }.into_any(),
                Err(e) => view! { <p class="empty">"Could not load the day: " {e.to_string()}</p> }.into_any(),
            })}
        </Transition>
    }
}

/// `?date=…&hour=…` links.
fn href(date: &str, hour: Option<usize>) -> String {
    match hour {
        Some(h) => format!("/?date={date}&hour={h}"),
        None => format!("/?date={date}"),
    }
}

/// What every minute chart of the day needs besides its series.
#[derive(Clone)]
struct Minutes {
    frame: Frame,
    at: Arc<Vec<i64>>,
    hours: Arc<Vec<i64>>,
    clock: LocalClock,
}

fn series(
    name: &'static str,
    class: &'static str,
    kind: Kind,
    values: &[Option<f64>],
    unit: &'static str,
    digits: usize,
) -> Series {
    Series {
        name,
        class,
        kind,
        values: Arc::new(values.to_vec()),
        unit,
        digits,
    }
}

fn shade(class: &'static str, on: Vec<bool>) -> Shade {
    Shade {
        class,
        on: Arc::new(on),
    }
}

#[component]
fn DayView(page: Day, cursor: RwSignal<Option<i64>>) -> impl IntoView {
    let query = use_query_map();
    let hours = Arc::new(page.hours.clone());
    let zoom = {
        let hours = hours.clone();
        Memo::new(move |_| {
            query
                .read()
                .get("hour")
                .and_then(|h| h.parse::<usize>().ok())
                .filter(|&h| h < hours.len())
        })
    };
    let (from, to) = (page.from, page.to);
    let range = {
        let hours = hours.clone();
        Signal::derive(move || {
            zoom.get()
                .map_or((from, to), |h| (hours[h].start, hours[h].end))
        })
    };
    let m = Minutes {
        frame: Frame { range, cursor },
        at: Arc::new(page.samples.at.clone()),
        hours: Arc::new(page.hours.iter().map(|h| h.start).collect()),
        clock: page.clock.clone(),
    };
    let date = page.date.clone();
    let charts = if m.at.is_empty() {
        view! { <section class="card"><p class="empty">"No minute samples for this day."</p></section> }
            .into_any()
    } else {
        view! {
            <PowerCard m=m.clone() page=page.clone()/>
            {page.ev.clone().map(|ev| view! { <EvCard m=m.clone() ev/> })}
            <BudgetCard m=m.clone() page=page.clone()/>
            <ZonesCard m=m.clone() page=page.clone()/>
        }
        .into_any()
    };
    view! {
        <nav class="pager" aria-label="Day">
            <a href=href(&page.prev, None) aria-label="Previous day">"‹"</a>
            <span class="label">{page.title.clone()}</span>
            <a href=href(&page.next, None) aria-label="Next day">"›"</a>
        </nav>
        <div class="actions">
            {(!page.is_today).then(|| view! { <a class="ghost" href="/">"Today"</a> })}
            {
                let date = date.clone();
                move || zoom.get().map(|_| view! { <a class="ghost" href=href(&date, None)>"Whole day"</a> })
            }
            <span class="note">"Times in " {page.time_zone.clone()}</span>
        </div>
        <section class="card">
            <h2>"Energy per hour"</h2>
            <p class="sub">"Whole house, against the hour's limit. Tap an hour to zoom the charts below to it."</p>
            <HourBars hours=hours.clone() date=date.clone() zoom/>
        </section>
        {charts}
        <EventsCard page=page.clone() date cursor/>
    }
}

#[component]
fn PowerCard(m: Minutes, page: Day) -> impl IntoView {
    let s = &page.samples;
    let mut lines = vec![
        series("House", "s1", Kind::Line, &s.power, "kW", 2),
        series("Floors", "s2", Kind::Line, &s.floors, "kW", 2),
    ];
    let fill = page.ev.as_ref().map(|ev| {
        lines.push(series(
            "Floors + EV",
            "s3",
            Kind::Line,
            &ev.stacked,
            "kW",
            2,
        ));
        lines.push(series("EV", "s3", Kind::Readout, &ev.power, "kW", 2));
        Fill {
            class: "s3",
            lower: Arc::new(s.floors.clone()),
            upper: Arc::new(ev.stacked.clone()),
        }
    });
    view! {
        <section class="card">
            <h2>"Power"</h2>
            <p class="sub">"Moving average of the whole house, and the floors together."
                {page.ev.is_some().then_some(" The EV charger is stacked on the floors (shaded).")}
                " Tap a chart to read values; every chart follows."</p>
            <TimeChart frame=m.frame at=m.at hours=m.hours clock=m.clock
                series=lines fill unit="kW" min_top=1.0/>
        </section>
    }
}

#[component]
fn EvCard(m: Minutes, ev: EvSamples) -> impl IntoView {
    let lines = vec![
        series("Offered", "s3", Kind::Step, &ev.offered, "A", 0),
        series("Guard limit", "s2", Kind::Step, &ev.limit, "A", 0),
        series("Charger limit", "muted", Kind::Step, &ev.max, "A", 0),
    ];
    view! {
        <section class="card">
            <h2>"EV charger"</h2>
            <p class="sub">"The current the charger offers the car, the charger limit set in the Easee app, and the guard's limit (in observe mode: the limit it would have set; 0 A is a pause)."</p>
            <TimeChart frame=m.frame at=m.at hours=m.hours clock=m.clock series=lines
                shades=vec![shade("override", ev.overridden)] unit="A" min_top=16.0/>
            <div class="bands"><span><i class="sw override"></i>"Temporary limit from the app"</span></div>
        </section>
    }
}

#[component]
fn BudgetCard(m: Minutes, page: Day) -> impl IntoView {
    let s = &page.samples;
    let flags = |b: Band| s.band.iter().map(|x| *x == Some(b)).collect();
    let lines = vec![
        series("Used", "s1", Kind::Line, &s.used, "kWh", 2),
        series("Projected", "s2", Kind::Line, &s.projected, "kWh", 2),
        series("Limit", "ink", Kind::Step, &s.limit, "kWh", 2),
    ];
    let shades = vec![
        shade("shed", flags(Band::Shedding)),
        shade("unreachable", flags(Band::LimitUnreachable)),
        shade("stale", flags(Band::MeterStale)),
    ];
    view! {
        <section class="card">
            <h2>"Hour budget"</h2>
            <p class="sub">"Energy used so far in each hour, the projection for the hour, and the limit the guard acts on."</p>
            <TimeChart frame=m.frame at=m.at hours=m.hours clock=m.clock series=lines shades
                unit="kWh" min_top=1.0/>
            <div class="bands">
                <span><i class="sw shed"></i>"Shedding"</span>
                <span><i class="sw unreachable"></i>"Limit unreachable"</span>
                <span><i class="sw stale"></i>"Meter stale"</span>
            </div>
        </section>
    }
}

#[component]
fn ZonesCard(m: Minutes, page: Day) -> impl IntoView {
    let zones = page
        .zones
        .into_iter()
        .map(|z| {
            let lines = vec![
                series("Reported", "s1", Kind::Step, &z.reported, "°C", 1),
                series("Decision", "s2", Kind::Step, &z.decision, "°C", 1),
                series("Base", "muted", Kind::Step, &z.base, "°C", 1),
                series("Power", "muted", Kind::Readout, &z.power, "kW", 2),
            ];
            let m = m.clone();
            view! {
                <div class="zone-chart">
                    <h3>{z.name}</h3>
                    <TimeChart frame=m.frame at=m.at hours=m.hours clock=m.clock series=lines
                        shades=vec![shade("shed", z.shed)] unit="°C" min_top=10.0 short=true/>
                </div>
            }
        })
        .collect_view();
    view! {
        <section class="card">
            <h2>"Zones"</h2>
            <p class="sub">"Setpoints per floor. Shaded where the guard shed the zone (in observe mode: would have shed it)."</p>
            <div class="zone-charts">{zones}</div>
        </section>
    }
}

#[component]
fn EventsCard(page: Day, date: String, cursor: RwSignal<Option<i64>>) -> impl IntoView {
    if page.events.is_empty() {
        return view! {
            <section class="card"><h2>"Events"</h2><p class="empty">"No events this day."</p></section>
        }
        .into_any();
    }
    let groups = page
        .events
        .into_iter()
        .map(|g| {
            let link = href(&date, Some(g.hour));
            let rows = g
                .rows
                .into_iter()
                .map(|r| {
                    let at = r.at;
                    view! {
                        <tr class="row">
                            <td><a href=link.clone() on:click=move |_| cursor.set(Some(at))>{r.time}</a></td>
                            <td>{r.what}</td>
                            <td>{r.zone}</td>
                            <td class="detail">{r.detail}</td>
                        </tr>
                    }
                })
                .collect_view();
            view! {
                <tr class="hour" class:over=g.over>
                    <td><a href=link.clone()>{g.title}</a></td>
                    <td colspan="3">{g.summary}{g.over.then(|| view! { <span class="flag">"over the limit"</span> })}</td>
                </tr>
                {rows}
            }
        })
        .collect_view();
    view! {
        <section class="card">
            <h2>"Events"</h2>
            <p class="sub">"Every decision of the day, newest first, by hour (energy used / limit). Tap a time to zoom to its hour."</p>
            <div class="table-wrap">
                <table class="events">
                    <thead><tr><th>"Time"</th><th>"What"</th><th>"Zone"</th><th>"Details"</th></tr></thead>
                    <tbody>{groups}</tbody>
                </table>
            </div>
        </section>
    }
    .into_any()
}

/// Energy per hour: bars against each hour's limit; every hour is a link that zooms to it.
#[component]
fn HourBars(hours: Arc<Vec<HourBar>>, date: String, zoom: Memo<Option<usize>>) -> impl IntoView {
    let (Some(first), Some(last)) = (hours.first(), hours.last()) else {
        return view! { <p class="empty">"No hours."</p> }.into_any();
    };
    let x = Linear::time((first.start, last.end));
    let max = hours
        .iter()
        .flat_map(|h| [h.energy, h.allowed])
        .flatten()
        .fold(0.0, f64::max);
    let axis = value_axis(max * 1.05, 1.0);
    let y = axis.scale;
    let rects = hours
        .iter()
        .filter_map(|h| {
            let r = bar((h.start, h.end), h.energy?, x, y);
            let class = if h.over {
                "bar over"
            } else if h.estimated {
                "bar est"
            } else {
                "bar"
            };
            Some(view! {
                <rect class=class x=format!("{:.1}", r.x) y=format!("{:.1}", r.y)
                    width=format!("{:.1}", r.w) height=format!("{:.1}", r.h)/>
            })
        })
        .collect_view();
    let limits = hours
        .iter()
        .filter_map(|h| {
            let ly = format!("{:.1}", y.up(h.limit?));
            Some(view! {
                <line class="limit" x1=format!("{:.1}", x.at_time(h.start)) x2=format!("{:.1}", x.at_time(h.end))
                    y1=ly.clone() y2=ly/>
            })
        })
        .collect_view();
    let labels = axis
        .ticks
        .iter()
        .map(|t| view! { <span class="yl" style:top=format!("{:.2}%", t.pos)>{t.label.clone()}</span> })
        .collect_view();
    let grid = axis
        .ticks
        .iter()
        .map(|t| {
            let gy = format!("{:.1}", t.pos * 10.0);
            view! { <line class="gridline" x1="0" x2="1000" y1=gy.clone() y2=gy/> }
        })
        .collect_view();
    let slots = hours
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let left = x.at_time(h.start) / 10.0;
            let width = x.at_time(h.end) / 10.0 - left;
            let label = format!("{}: {}", h.title, h.summary);
            view! {
                <a class="slot" class:selected=move || zoom.get() == Some(i)
                    href=href(&date, Some(i)) aria-label=label.clone() title=label
                    style:left=format!("{left:.2}%") style:width=format!("{width:.2}%")></a>
            }
        })
        .collect_view();
    let x_labels = hours
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 3 == 0)
        .map(|(_, h)| {
            view! { <span class="xl" style:left=format!("{:.2}%", x.at_time(h.start) / 10.0)>{h.label.clone()}</span> }
        })
        .collect_view();
    let selected = {
        let hours = hours.clone();
        move || {
            zoom.get().map(|i| {
                let h = &hours[i];
                view! { <span class="time">{h.title.clone()}</span><span>{h.summary.clone()}</span> }
            })
        }
    };
    view! {
        <div class="chart">
            <div class="plot">
                <span class="unit">"kWh"</span>
                {labels}
                <svg viewBox="0 0 1000 1000" preserveAspectRatio="none" aria-hidden="true">
                    {grid}{rects}{limits}
                </svg>
                <div class="hit slots">{slots}</div>
                <div class="xaxis">{x_labels}</div>
            </div>
            <div class="legend">
                <span><i class="sw s1"></i>"Energy"</span>
                <span><i class="sw est"></i>"Estimated"</span>
                <span><i class="sw over"></i>"Over the limit"</span>
                <span><i class="sw limit"></i>"Limit"</span>
                {selected}
            </div>
        </div>
    }
    .into_any()
}
