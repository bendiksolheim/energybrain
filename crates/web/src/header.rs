//! The live header: the running hour against its limit, the status tiles and the zones.

use energybrain_view::status::{HourBudget, StatusBar, Tile, Tone, ZoneChip, ZoneState};
use leptos::prelude::*;

use crate::api::latest_status;
use crate::in_browser;

/// CSS class for a tone.
pub fn tone(t: Tone) -> &'static str {
    match t {
        Tone::Normal => "normal",
        Tone::Warn => "warn",
        Tone::Alert => "alert",
    }
}

#[component]
pub fn Header() -> impl IntoView {
    let status = Resource::new(|| (), |()| latest_status());
    // Pushed by `/api/stream` once the page runs in the browser; until then (and on the
    // server) the header shows the status the page was rendered with.
    let live = RwSignal::new(None::<StatusBar>);
    in_browser(move || subscribe(live.write_only()));
    view! {
        <header class="top">
            <Suspense fallback=|| view! { <Brand status=None/><p class="empty">"Loading…"</p> }>
                {move || live.get().map(|s| Ok(Some(s))).or_else(|| status.get()).map(|r| match r {
                    Ok(Some(s)) => view! { <Brand status=Some(s.clone())/><Status status=s/> }.into_any(),
                    Ok(None) => view! { <Brand status=None/><p class="empty">"No status from the engine yet."</p> }.into_any(),
                    Err(e) => view! { <Brand status=None/><p class="empty">"Status unavailable: " {e.to_string()}</p> }.into_any(),
                })}
            </Suspense>
        </header>
    }
}

/// Feeds every status from `/api/stream` into `live`. `EventSource` reconnects by itself.
#[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
fn subscribe(live: WriteSignal<Option<StatusBar>>) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;

    let Ok(source) = web_sys::EventSource::new("/api/stream") else {
        return;
    };
    let on_status =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            if let Some(bar) = e
                .data()
                .as_string()
                .and_then(|t| serde_json::from_str(&t).ok())
            {
                live.set(Some(bar));
            }
        });
    let _ = source.add_event_listener_with_callback("status", on_status.as_ref().unchecked_ref());
    // The header lives as long as the page: keep the listener (and the connection) for good.
    on_status.forget();
}

#[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
fn subscribe(_: WriteSignal<Option<StatusBar>>) {}

#[component]
fn Brand(status: Option<StatusBar>) -> impl IntoView {
    let (mode, updated) = status.map_or_else(Default::default, |s| {
        (s.mode, format!("updated {}", s.updated))
    });
    view! {
        <div class="brand">
            <h1>"energybrain"</h1>
            <span class="pill">{mode}</span>
            <span class="updated">{updated}</span>
        </div>
    }
}

#[component]
fn Status(status: StatusBar) -> impl IntoView {
    let StatusBar {
        hour, tiles, zones, ..
    } = status;
    view! {
        <Budget hour/>
        <div class="tiles">{tiles.into_iter().map(|t| view! { <TileView tile=t/> }).collect_view()}</div>
        <ul class="zones">{zones.into_iter().map(|z| view! { <Zone zone=z/> }).collect_view()}</ul>
    }
}

#[component]
fn Budget(hour: HourBudget) -> impl IntoView {
    // Fill and marker as a share of the larger of limit and projection, so both fit.
    let scale = hour
        .limit
        .max(hour.projected.unwrap_or(0.0))
        .max(hour.used)
        .max(0.001);
    let pct = |v: f64| format!("{:.1}%", (v / scale * 100.0).clamp(0.0, 100.0));
    view! {
        <section class=format!("budget {}", tone(hour.tone))>
            <div class="budget-head">
                <span class="k">"This hour · " {hour.label}</span>
                <span class="v">{hour.text}</span>
            </div>
            <div class="bar" role="img" aria-label="Energy used this hour against the limit">
                <div class="used" style:width=pct(hour.used)></div>
                {hour.projected.map(|p| view! { <div class="projected" style:left=pct(p)></div> })}
                <div class="limit" style:left=pct(hour.limit)></div>
            </div>
            <div class="d">{hour.detail}</div>
        </section>
    }
}

#[component]
fn TileView(tile: Tile) -> impl IntoView {
    view! {
        <div class=format!("tile {}", tone(tile.tone))>
            <div class="k">{tile.key}</div>
            <div class="v">{tile.value}</div>
            {tile.detail.map(|d| view! { <div class="d">{d}</div> })}
        </div>
    }
}

fn state_class(s: ZoneState) -> &'static str {
    match s {
        ZoneState::Idle => "idle",
        ZoneState::Heating => "heating",
        ZoneState::Shed => "shed",
        ZoneState::Overridden => "overridden",
    }
}

fn state_text(s: ZoneState) -> &'static str {
    match s {
        ZoneState::Idle => "idle",
        ZoneState::Heating => "heating",
        ZoneState::Shed => "shed",
        ZoneState::Overridden => "override",
    }
}

#[component]
fn Zone(zone: ZoneChip) -> impl IntoView {
    view! {
        <li class=format!("zone {}", state_class(zone.state))>
            <span class="dot" aria-hidden="true"></span>
            <span class="name">{zone.name}</span>
            <span class="state">{state_text(zone.state)}{zone.excluded.then_some(" · not shed")}</span>
            <span class="setpoint">{zone.setpoint}{zone.base.map(|b| view! { <small>{b}</small> })}</span>
            <span class="power">{zone.power}</span>
        </li>
    }
}
