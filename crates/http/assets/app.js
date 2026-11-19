// energybrain history page. Everything worth computing is done in Rust (/api/day, /api/month);
// this file fetches, maps the columns onto uPlot charts, and handles navigation.
"use strict";

const $ = (id) => document.getElementById(id);
const view = { kind: "day", date: null, month: null, zoom: null, data: null, today: null };
let charts = [];
let minuteCharts = [];

// ---- small helpers --------------------------------------------------------------------

function css(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** An element with text children only (never HTML), so data can't inject markup. */
function el(tag, attrs, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const c of children) {
    if (c == null) continue;
    e.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return e;
}

const secs = (rfc3339) => Date.parse(rfc3339) / 1000;
const num = (v, digits) => (v == null ? "–" : v.toFixed(digits));
const qty = (q, digits = 2) => (q == null ? "–" : `${num(q.value, digits)} ${q.unit}`);

function timeFmt(tz) {
  const f = new Intl.DateTimeFormat("en-GB", { timeZone: tz, hour: "2-digit", minute: "2-digit" });
  return (s) => f.format(new Date(s * 1000));
}

/** "YYYY-MM-DD" ± days, as calendar arithmetic (no time zone involved). */
function addDays(date, n) {
  const [y, m, d] = date.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d + n)).toISOString().slice(0, 10);
}

function addMonths(month, n) {
  const [y, m] = month.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1 + n, 1)).toISOString().slice(0, 7);
}

function longDate(date) {
  return new Intl.DateTimeFormat("en-GB", {
    timeZone: "UTC", weekday: "long", day: "numeric", month: "long", year: "numeric",
  }).format(new Date(date + "T12:00:00Z"));
}

function shortDate(date) {
  return new Intl.DateTimeFormat("en-GB", { timeZone: "UTC", weekday: "short", day: "numeric" })
    .format(new Date(date + "T12:00:00Z"));
}

function longMonth(month) {
  return new Intl.DateTimeFormat("en-GB", { timeZone: "UTC", month: "long", year: "numeric" })
    .format(new Date(month + "-15T12:00:00Z"));
}

const tip = $("tip");
function showTip(evt, ...lines) {
  tip.replaceChildren(...lines.map((l, i) => el(i === 0 ? "b" : "div", {}, l)));
  tip.hidden = false;
  const x = Math.min(evt.clientX + 12, window.innerWidth - tip.offsetWidth - 8);
  tip.style.left = `${x}px`;
  tip.style.top = `${evt.clientY + 12}px`;
}
function hideTip() { tip.hidden = true; }

// ---- chart plumbing -------------------------------------------------------------------

function destroyCharts() {
  for (const c of charts) c.destroy();
  charts = [];
  minuteCharts = [];
}

function axes(unit, tz) {
  const ink = css("--muted");
  const grid = { stroke: css("--grid"), width: 1 };
  return [
    { stroke: ink, grid, ticks: { show: false } },
    {
      stroke: ink, grid, ticks: { show: false }, size: 56, label: unit, labelSize: 16,
      values: (u, ticks) => ticks.map((v) => (Math.abs(v) >= 10 ? v.toFixed(0) : v.toFixed(1))),
    },
  ];
}

function line(label, stroke, unit, digits, extra = {}) {
  return {
    label, stroke, width: 2, points: { show: false },
    value: (u, v) => (v == null ? "–" : `${v.toFixed(digits)} ${unit}`),
    ...extra,
  };
}

/** Shades every run of points where `on(i)` holds, from that point to the next. */
function bandHook(at, on, color) {
  return (u) => {
    const { ctx } = u;
    const { top, height } = u.bbox;
    ctx.save();
    ctx.fillStyle = color;
    for (let i = 0; i < at.length; i++) {
      if (!on(i)) continue;
      const end = i + 1 < at.length ? at[i + 1] : at[i] + 60;
      const x0 = u.valToPos(at[i], "x", true);
      const x1 = u.valToPos(end, "x", true);
      ctx.fillRect(x0, top, Math.max(1, x1 - x0), height);
    }
    ctx.restore();
  };
}

/** The x range every minute chart shows: the zoom, or the whole day. */
function xRange() {
  return view.zoom || [view.data.from_s, view.data.to_s];
}

function zoomTo(range) {
  view.zoom = range;
  $("reset").hidden = !range;
  for (const c of minuteCharts) c.setScale("x", { min: xRange()[0], max: xRange()[1] });
}

function minuteChart(target, { hooks = {}, scales = {}, ...rest }, data) {
  const u = new uPlot({
    ...rest,
    width: target.clientWidth,
    tzDate: (ts) => uPlot.tzDate(new Date(ts * 1000), view.data.time_zone),
    cursor: {
      sync: { key: "day" },
      drag: { x: true, y: false, setScale: false },
    },
    scales: { ...scales, x: { time: true, range: () => xRange() } },
    hooks: {
      ...hooks,
      setSelect: [(u) => {
        if (u.select.width > 4) {
          const a = u.posToVal(u.select.left, "x");
          const b = u.posToVal(u.select.left + u.select.width, "x");
          zoomTo([a, b]);
        }
        u.setSelect({ left: 0, width: 0, top: 0, height: 0 }, false);
      }],
    },
  }, data, target);
  u.over.addEventListener("dblclick", () => zoomTo(null));
  charts.push(u);
  minuteCharts.push(u);
  return u;
}

// ---- status tiles ---------------------------------------------------------------------

function tile(k, v, d) {
  return el("div", { class: "tile" }, el("div", { class: "k" }, k), el("div", { class: "v" }, v),
    d ? el("div", { class: "d" }, d) : null);
}

function tierText(t) {
  const to = t.to ? `${num(t.to.value, 0)}` : "∞";
  return `${num(t.from.value, 0)}–${to} kW`;
}

async function loadStatus() {
  const box = $("status");
  try {
    const res = await fetch("api/state");
    if (!res.ok) throw new Error((await res.json()).error || res.statusText);
    const s = await res.json();
    const guard = s.guard.replaceAll("_", " ");
    box.replaceChildren(
      tile("Mode", s.mode),
      tile("Guard", guard),
      tile("This hour", `${qty(s.used)} of ${qty(s.limit.effective)}`,
        `projected ${qty(s.projected)}`),
      tile("Power", qty(s.power), s.freshness === "fresh" ? null : `meter ${s.freshness.replace("_", " ")}`),
      tile("Month", `${qty(s.top_average)} top-3 average`,
        `tier ${tierText(s.tier)} · ${num(s.tier.price.value, 0)} kr/month`),
    );
  } catch (e) {
    box.replaceChildren(tile("Status", "unavailable", String(e.message || e)));
  }
}

// ---- day ------------------------------------------------------------------------------

function renderDay(d) {
  const tz = d.time_zone;
  const hm = timeFmt(tz);
  d.from_s = secs(d.from);
  d.to_s = secs(d.to);
  const at = d.samples.at.map(secs);
  const s1 = css("--s1"), s2 = css("--s2"), ink = css("--ink-2"), muted = css("--muted");

  // Energy per hour: bars at mid-hour, estimated hours lighter, limit as a tick per hour.
  const h = d.hours;
  const mid = h.start.map((s) => secs(s) + 1800);
  const est = (i) => h.quality[i] === "estimated";
  const known = h.energy.values.map((v, i) => (est(i) ? null : v));
  const guessed = h.energy.values.map((v, i) => (est(i) ? v : null));
  const bars = uPlot.paths.bars({ size: [0.7, 24], radius: 0.2 });
  const top = Math.max(1, ...h.energy.values.filter((v) => v != null),
    ...h.allowed.values.filter((v) => v != null)) * 1.1;
  const hours = new uPlot({
    width: $("c-hours").clientWidth,
    height: 200,
    tzDate: (ts) => uPlot.tzDate(new Date(ts * 1000), tz),
    cursor: { sync: { key: "day" }, drag: { x: false, y: false }, points: { show: false } },
    scales: { x: { time: true, range: () => [d.from_s, d.to_s] }, y: { range: () => [0, top] } },
    axes: axes("kWh"),
    series: [
      { value: (u, v) => (v == null ? "–" : `${hm(v - 1800)}–${hm(v + 1800)}`) },
      line("Energy", s1, "kWh", 2, { paths: bars, fill: s1, width: 0 }),
      line("Estimated", css("--s1-soft"), "kWh", 2, { paths: bars, fill: css("--s1-soft"), width: 0 }),
      line("Allowed", ink, "kWh", 2, { paths: () => null }),
    ],
    hooks: {
      draw: [(u) => {
        const { ctx } = u;
        ctx.save();
        ctx.strokeStyle = ink;
        ctx.lineWidth = 2 * devicePixelRatio;
        h.allowed.values.forEach((v, i) => {
          if (v == null) return;
          const y = u.valToPos(v, "y", true);
          ctx.beginPath();
          ctx.moveTo(u.valToPos(mid[i] - 1800, "x", true), y);
          ctx.lineTo(u.valToPos(mid[i] + 1800, "x", true), y);
          ctx.stroke();
        });
        ctx.restore();
      }],
    },
  }, [mid, known, guessed, h.allowed.values], $("c-hours"));
  hours.over.addEventListener("click", () => {
    const i = hours.cursor.idx;
    if (i != null) zoomTo([mid[i] - 1800, mid[i] + 1800]);
  });
  charts.push(hours);
  $("t-hours").replaceChildren(table(["Hour", "Energy", "Quality", "Allowed", "Limit"],
    h.start.map((s, i) => [`${hm(secs(s))}`, num(h.energy.values[i], 2), h.quality[i] || "–",
      num(h.allowed.values[i], 2), num(h.limit.values[i], 2)]), [1, 3, 4]));

  const sm = d.samples;
  const container = (id) => {
    const c = $(id);
    c.replaceChildren();
    if (!at.length) c.append(el("div", { class: "empty" }, "No samples for this day."));
    return c;
  };

  // Power.
  const power = container("c-power");
  if (at.length) {
    minuteChart(power, {
      height: 220,
      axes: axes("kW"),
      series: [
        { value: (u, v) => (v == null ? "–" : hm(v)) },
        line("House", s1, "kW", 2),
        line("Floors", s2, "kW", 2),
      ],
      scales: { y: { range: (u, a, b) => [0, Math.max(1, b * 1.1)] } },
    }, [at, sm.power.values, sm.floors.values]);
  }

  // Hour budget, shaded by guard state.
  const budget = container("c-budget");
  if (at.length) {
    const g = sm.guard;
    minuteChart(budget, {
      height: 220,
      axes: axes("kWh"),
      series: [
        { value: (u, v) => (v == null ? "–" : hm(v)) },
        line("Used", s1, "kWh", 2),
        line("Projected", s2, "kWh", 2),
        line("Limit", ink, "kWh", 2, { width: 1.5, paths: uPlot.paths.stepped({ align: 1 }) }),
      ],
      scales: { y: { range: (u, a, b) => [0, Math.max(1, b * 1.1)] } },
      hooks: {
        drawClear: [
          bandHook(at, (i) => g[i] === "shedding", css("--shed")),
          bandHook(at, (i) => g[i] === "limit_unreachable", css("--unreachable")),
          bandHook(at, (i) => g[i] === "meter_stale", css("--stale")),
        ],
      },
    }, [at, sm.used.values, sm.projected.values, sm.limit.values]);
  }

  // One small chart per zone.
  const zones = container("c-zones");
  if (at.length) {
    const stepped = uPlot.paths.stepped({ align: 1 });
    for (const z of sm.zones) {
      const box = el("div");
      zones.append(el("div", {}, el("h3", {}, z.name), box));
      minuteChart(box, {
        height: 150,
        axes: axes("°C"),
        series: [
          { value: (u, v) => (v == null ? "–" : hm(v)) },
          line("Reported", s1, "°C", 1, { paths: stepped }),
          line("Decision", s2, "°C", 1, { paths: stepped }),
          line("Base", muted, "°C", 1, { width: 1, paths: stepped }),
          // Readout only: the zone's power at the cursor, in the legend.
          line("Power", "transparent", "kW", 2, { paths: () => null, scale: "kw" }),
        ],
        scales: { kw: { range: () => [0, 1] } },
        hooks: { drawClear: [bandHook(at, (i) => z.shed[i] === true, css("--shed"))] },
      }, [at, z.reported.values, z.decision.values, z.base.values, z.power.values]);
    }
  }

  // Events, newest hour first, each under a header with the hour's energy.
  $("t-events").replaceChildren(d.event_hours.length
    ? eventTable(d.event_hours.map((g) => {
      const from = secs(g.start), to = secs(g.end);
      const n = g.events.length;
      return {
        title: `${hm(from)}–${hm(to)}`,
        summary: `${num(h.energy.values[g.hour], 2)} / ${num(h.limit.values[g.hour], 2)} kWh · ${n} ${n === 1 ? "event" : "events"}`,
        over: g.over_limit,
        range: [from, to],
        rows: g.events.map((e) => {
          const [what, zone, detail] = describe(e);
          const t = secs(e.at);
          return { cells: [hm(t), what, zone || "–", detail], range: [t - 1800, t + 1800] };
        }),
      };
    }))
    : el("div", { class: "empty" }, "No events this day."));
  zoomTo(view.zoom);
}

const RELEASE = {
  below_limit: "projection back below the limit",
  meter_stale: "meter stale",
  not_sheddable: "no longer sheddable (switched off or overridden)",
  guard_off: "guard turned off",
};

/** [what, zone, details] for one stored event. */
function describe(e) {
  const k = e.kind || {};
  const would = e.mode !== "active";
  const hm = timeFmt(view.data.time_zone);
  if (k.type === "guard") {
    const g = k.event || {};
    if (g.type === "shed") {
      return [would ? "Would shed" : "Shed", g.zone,
        `projected ${qty(g.projected)}, limit ${qty(g.limit)}, load ${qty(g.load)}`];
    }
    if (g.type === "release") {
      return [would ? "Would release" : "Released", g.zone, RELEASE[g.reason] || g.reason];
    }
    if (g.type === "limit_unreachable") {
      return ["Limit unreachable", null,
        `projected ${qty(g.projected)}, limit ${qty(g.limit)}, shedding saves ${qty(g.available)}`];
    }
  }
  if (k.type === "override") return ["Override", k.zone, `${qty(k.setpoint, 1)} until ${hm(secs(k.until))}`];
  if (k.type === "override_expired") return ["Override ended", k.zone, ""];
  if (k.type === "mode_changed") return ["Mode changed", null, `${k.from} → ${k.to}`];
  return [k.type || "event", null, JSON.stringify(k)];
}

function table(head, rows, numeric = [], onRow = null) {
  const t = el("table", {},
    el("thead", {}, el("tr", {}, ...head.map((h, i) => el("th", numeric.includes(i) ? { class: "num" } : {}, h)))),
    el("tbody", {}, ...rows.map((r, ri) => el("tr", onRow ? { class: "clickable", onclick: () => onRow(ri) } : {},
      ...r.map((c, i) => el("td", numeric.includes(i) ? { class: "num" } : {}, String(c)))))));
  return el("div", { class: "table-wrap" }, t);
}

/** The events table: one header row per hour, its events below. Every row zooms on click. */
function eventTable(groups) {
  const head = ["Time", "What", "Zone", "Details"];
  const row = (cls, range, cells) => el("tr", { class: cls, onclick: () => zoomTo(range) }, ...cells);
  const body = groups.flatMap((g) => [
    row(g.over ? "clickable hour over" : "clickable hour", g.range, [
      el("td", {}, g.title),
      el("td", { colspan: "3" }, g.summary, g.over ? el("span", { class: "flag" }, "over the limit") : null),
    ]),
    ...g.rows.map((r) => row("clickable", r.range, r.cells.map((c) => el("td", {}, String(c))))),
  ]);
  return el("div", { class: "table-wrap" }, el("table", { class: "events" },
    el("thead", {}, el("tr", {}, ...head.map((h) => el("th", {}, h)))),
    el("tbody", {}, ...body)));
}

// ---- month ----------------------------------------------------------------------------

function renderMonth(m) {
  const target = m.target.value;
  $("m-tiles").replaceChildren(
    tile("Top-3 average", qty(m.top_average), `target ${qty(m.target, 0)}`),
    tile("Tier", tierText(m.tier), `${num(m.tier.price.value, 0)} kr/month`),
    ...m.top_days.map((p, i) => tile(`Top day ${i + 1}`, qty(p.energy),
      `${shortDate(p.date)}, ${timeFmt(m.time_zone)(secs(p.hour))}`)),
  );

  const days = m.days;
  const x = days.date.map((d) => secs(d + "T12:00:00Z"));
  const top = days.peak.values.map((v, i) => (days.top[i] ? v : null));
  const rest = days.peak.values.map((v, i) => (days.top[i] ? null : v));
  const bars = uPlot.paths.bars({ size: [0.7, 24], radius: 0.2 });
  const peak = Math.max(target, ...days.peak.values.filter((v) => v != null)) * 1.15;
  const s1 = css("--s1");
  const c = $("c-peaks");
  c.replaceChildren();
  const hm = timeFmt(m.time_zone);
  const u = new uPlot({
    width: c.clientWidth,
    height: 240,
    tzDate: (ts) => uPlot.tzDate(new Date(ts * 1000), "UTC"),
    cursor: { drag: { x: false, y: false }, points: { show: false } },
    scales: {
      x: { time: true, range: () => [x[0] - 43200, x[x.length - 1] + 43200] },
      y: { range: () => [0, peak] },
    },
    axes: axes("kWh"),
    series: [
      { value: (u, v, s, i) => (i == null ? "–" : shortDate(days.date[i])) },
      line("Top 3", s1, "kWh", 2, { paths: bars, fill: s1, width: 0 }),
      line("Other days", css("--other"), "kWh", 2, { paths: bars, fill: css("--other"), width: 0 }),
      line("Target", css("--ink-2"), "kWh", 2, { width: 1.5 }),
      line("Top-3 average", css("--s2"), "kWh", 2, { width: 1.5 }),
    ],
  }, [x, top, rest, x.map(() => target), x.map(() => m.top_average.value)], c);
  u.over.addEventListener("click", () => {
    const i = u.cursor.idx;
    if (i != null) go({ kind: "day", date: days.date[i] });
  });
  charts.push(u);
  $("t-days").replaceChildren(table(["Day", "Highest hour", "At", "Total"],
    days.date.map((d, i) => [shortDate(d), num(days.peak.values[i], 2),
      days.peak_hour[i] ? hm(secs(days.peak_hour[i])) : "–", num(days.energy.values[i], 1)]), [1, 3]));

  // Heatmap: day rows × local hour columns, binned against the target.
  const steps = [0.2, 0.4, 0.6, 0.8, 1.0, 1.2].map((f) => f * target);
  const bin = (v) => 1 + steps.filter((s) => v >= s).length;
  const heat = $("heat");
  heat.style.gridTemplateColumns = "auto repeat(24, minmax(12px, 1fr))";
  const cells = [el("div")];
  for (let hr = 0; hr < 24; hr++) cells.push(el("div", { class: "hour" }, hr % 3 === 0 ? String(hr).padStart(2, "0") : ""));
  days.date.forEach((d, i) => {
    cells.push(el("div", { class: "day" }, shortDate(d)));
    days.by_hour[i].values.forEach((v, hr) => {
      const cell = el("div", { class: v == null ? "cell none" : "cell" });
      if (v != null) cell.style.background = `var(--heat-${bin(v)})`;
      const label = `${String(hr).padStart(2, "0")}:00`;
      cell.addEventListener("pointermove", (evt) => showTip(evt, v == null ? "no data" : `${v.toFixed(2)} kWh`, `${shortDate(d)}, ${label}`));
      cell.addEventListener("pointerleave", hideTip);
      cell.addEventListener("click", () => go({ kind: "day", date: d }));
      cells.push(cell);
    });
  });
  heat.replaceChildren(...cells);
  const fmt = (v) => num(v, 1);
  const labels = [`< ${fmt(steps[0])}`, ...steps.slice(0, -1).map((s, i) => `${fmt(s)}–${fmt(steps[i + 1])}`),
    `≥ ${fmt(steps[steps.length - 1])} kWh`];
  $("heat-scale").replaceChildren(...labels.map((l, i) =>
    el("span", {}, Object.assign(el("i", { class: "sw" }), { style: `background: var(--heat-${i + 1})` }), l)),
  el("span", {}, `Target: ${fmt(target)} kWh per hour.`));
}

// ---- navigation -----------------------------------------------------------------------

function readHash() {
  const p = new URLSearchParams(location.hash.slice(1));
  if (p.has("month")) return { kind: "month", month: p.get("month") };
  return { kind: "day", date: p.get("day") };
}

function go(next) {
  const p = next.kind === "month" ? `month=${next.month || ""}` : `day=${next.date || ""}`;
  if (location.hash.slice(1) !== p) location.hash = p;
  else load();
}

async function load(keepZoom = false) {
  const next = readHash();
  if (!keepZoom || next.kind !== view.kind || next.date !== view.date) view.zoom = null;
  Object.assign(view, next);
  const isDay = view.kind === "day";
  $("tab-day").setAttribute("aria-selected", String(isDay));
  $("tab-month").setAttribute("aria-selected", String(!isDay));
  $("day").hidden = !isDay;
  $("month").hidden = isDay;
  $("reset").hidden = !(isDay && view.zoom);
  const url = isDay ? `api/day${view.date ? `?date=${view.date}` : ""}`
    : `api/month${view.month ? `?month=${view.month}` : ""}`;
  $("note").textContent = "Loading…";
  try {
    const res = await fetch(url);
    const body = await res.json();
    if (!res.ok) throw new Error(body.error || res.statusText);
    view.data = body;
    view.today = body.today;
    if (isDay) view.date = body.date; else view.month = body.month;
    $("label").textContent = isDay ? longDate(body.date) : longMonth(body.month);
    destroyCharts();
    if (isDay) renderDay(body); else renderMonth(body);
    $("note").textContent = `Times in ${body.time_zone}.`;
  } catch (e) {
    $("note").textContent = `Could not load: ${e.message || e}`;
  }
}

function step(n) {
  if (view.kind === "day") go({ kind: "day", date: addDays(view.date, n) });
  else go({ kind: "month", month: addMonths(view.month, n) });
}

$("tab-day").addEventListener("click", () => go({ kind: "day", date: view.kind === "day" ? view.date : null }));
$("tab-month").addEventListener("click", () => go({ kind: "month", month: view.date ? view.date.slice(0, 7) : null }));
$("prev").addEventListener("click", () => step(-1));
$("next").addEventListener("click", () => step(1));
$("today").addEventListener("click", () => go({ kind: view.kind }));
$("reset").addEventListener("click", () => zoomTo(null));
window.addEventListener("hashchange", () => load());

let resizeTimer;
window.addEventListener("resize", () => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => view.data && load(true), 200);
});

// Today (and this month) keep filling in: refresh every minute, keeping the zoom.
setInterval(() => {
  loadStatus();
  const live = view.kind === "day" ? view.date === view.today : view.today && view.today.startsWith(view.month);
  if (live) load(true);
}, 60000);

loadStatus();
load();
