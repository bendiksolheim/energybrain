# energybrain — working rules for Claude Code

The full design and milestone plan is in `PLAN.md`. Read it before starting work, and
keep it up to date if a decision changes.

## Commands
- `just check` — fmt check, clippy (`-D warnings`), all tests, coverage gate. Must be green before every commit.
- `just test` — `cargo test --workspace --all-features`
- `just cov` — `cargo llvm-cov --workspace --all-features --fail-under-lines 100 --ignore-filename-regex '(^|/)main\.rs$'`
- `just run` — run locally with `config/example.toml`

## Non-negotiables
- **100 % line coverage**, enforced. The only excluded file is `crates/app/src/main.rs`, which holds plumbing only. No coverage-exclusion attributes, no `cfg(coverage)`. If something can't be tested, refactor it.
- `energybrain-core` is pure: no tokio, no I/O, no reading the system clock. Time always comes in as an argument.
- Physical quantities use the unit newtypes (`Kw`, `Kwh`, `Celsius`, …), never bare `f64`, across module boundaries.
- Store timestamps in UTC. Local time (`Europe/Oslo` by default) is used only through `LocalCalendar`, for hour and day bucketing. Test DST days explicitly.
- No `_ =>` catch-all arms on `Input`, `Output` or `ProposalKind`.
- Never write setpoints in `Observe` or `Off` mode. One exception, agreed with the user: leaving
  `Active` first writes every zone we changed back to its base (the last act of `Active`), then
  switches mode. Every setpoint decision carries reasons.
- `unsafe_code = "forbid"`. Library crates use `thiserror`. No `unwrap()`/`expect()` in non-test code unless the invariant is documented in a comment right next to it.
- Don't add dependencies beyond those listed in PLAN.md §3 without recording why below.

## Workflow
- Work milestone by milestone (PLAN.md §5). Write tests together with the code.
- Commit at the end of each green milestone (or smaller green steps) with descriptive messages.
- M0 needs answers from the user (HAN payloads, Z-Wave topics, tariff table). Ask early, and record the answers in the section below and in `config/example.toml`.

## Environment facts (fill in during M0)
- MQTT broker: `192.168.1.172:1883`, no TLS, username + password. Credentials live only in a
  gitignored config file (`config/*.toml` except `example.toml` is ignored) — never commit them.
- HAN reader + topics + payload format: Tibber Pulse (local MQTT) on a **Kaifa MA304H3E**
  (3-phase, list id `KFM_001`, IT net: UL2 reads 0).
  Topic **`tibber`**, one complete HDLC frame (`7e … 7e`) per message:
  - 41 B list 1 every ~2.5 s: active import power (W, u32);
  - 123 B list 2 every 10 s: list id, meter id, type, P+, P−, Q+, Q− (W/var), IL1–3 (mA),
    UL1–3 (0.1 V);
  - list 3 hourly at hh:00:10: list 2 + meter clock + cumulative A+, A−, R+, R− (Wh);
  - ~400 B JSON `{"status":{…}}` from the Pulse itself — ignore.
  The DLMS header carries a 12-byte date-time in **local time with deviation unspecified**
  (`0x8000`); clock-status byte `0x00` in winter (DST bit `0x80` expected in summer).
- Zones (id, setpoint topic, power topic, nominal kW, priority): Heatit **Z-TRM3**, via Z-Wave JS UI
  in *value only* mode. Per device `zwave/<Room>/<Device>/`:
  `67/1/setpoint/1` (heating setpoint °C, plain number; write to `…/set`, echoed twice within 1 s),
  `50/1/value/66049` (power W, reported on change and roughly every 90 s, ~0.1 W when idle),
  `50/1/value/65537` (kWh total), `66/1/state` (1 = heating, 0 = idle), `64/1/mode` (1 = heat),
  `49/2` / `49/4` `Air_temperature` (room / floor sensor), `status` (`true` = alive).
  Devices: `Bad/Gulv_bad`, `Gang/Gulv_gang`, `Kjeller/Gulv_kjeller`, `Utleie/Gulv_utleie_1`,
  `Utleie/Gulv_utleie_2`, `Vaskerom/Varmekabel` (measured ~575 W when on).
  Shed priority (lower sheds first, configurable): 1 `bad`, 2 `vaskerom`, 3 `gang`,
  4 `utleie_1`, 5 `utleie_2`, 6 `kjeller` (already at 15 °C, little to gain). The Utleie
  zones are a rental unit, currently empty: shedding allowed, but must stay switchable per
  zone (HomeKit `shed_allowed`). Heater sizes unknown → learned from power reports
  (`nominal_kw` default 1 kW until learned). `shed_delta` 4 °C everywhere (Kjeller → 11 °C ok).
  `Stue/Kontakt_tak` is a ceiling-light switch: ignored.
- Grid company + capacity tiers: **Tensio TS**. Tiers (kW, kr/month): 0–2 131, 2–5 233, 5–10 397,
  10–15 585, 15–20 775, 20–25 964, 25–50 1655, 50–75 2599, 75–100 3544, 100–150 5117,
  150–200 7003, 200–300 10147, 300–400 13925, 400–500 17697, >500 21473.
  Energy charge (later): day 06–22 37.79 øre/kWh, night 22–06 23.79 øre/kWh (no separate weekend
  rate in the table; source: `docs/tier-table.md`). Capacity basis assumed **hourly** (not confirmed with Tensio).
- Target tier: **2–5 kW** (limit 5 kWh/h − 0.3 margin). Observe mode reports hours where it
  is unreachable (EV charging) to judge whether the target is realistic.
- Runtime host: Docker on amd64. No metrics stack: Grafana/VictoriaMetrics dropped from
  compose; observe mode is reviewed via `/api/events`.
- Other big loads: Easee Charge Lite EV charger (WiFi/cloud API, not controlled in phase 1),
  heat pump (not controllable). Both show up only in the whole-house meter reading.
- HomeKit: Homebridge **Easy MQTT** plugin. Zigbee2MQTT also exists but is unrelated.

## Decision log
- M1: `energybrain::version()` exists so the coverage gate has lines to measure (llvm-cov fails
  an empty workspace).
- Clippy `unwrap_used`/`expect_used` are warn (→ error under `-D warnings`); `clippy.toml` allows
  them in tests. A documented-invariant `expect` needs a local `#[allow]` plus the comment.
- HAN: new pure crate `energybrain-han` parses Kaifa HDLC/DLMS frames (CRC-16 hand-rolled, no new
  dependency). It replaces the plain-number/JSON HAN parsing originally planned for the mqtt crate.
- Echo detection: a reported setpoint within ±0.25 °C of our last write is an echo **regardless
  of elapsed time**, until a different value is reported. The learned base per zone is persisted.
  (Prevents learning a shed value as base after a restart.)
- `ZonePower` is a last-known value (Z-Wave reports on change); `nominal_kw` is only a fallback
  until the first power report arrives.
- Hour rollover does not force-release sheds; the budget resets and normal hysteresis decides.
  Every release (any time) goes through the `restore_stagger`.
- Shedding uses a per-zone `shed_delta`, default 4 °C, clamped to `min_setpoint`.
- While shed: a user setpoint above the cap → override (default 2 h, zone not shed); at or below
  the cap → becomes the new base, no override. An expired override's value remains the base.
- No seeding of earlier peaks: history starts on deploy day (fewer known days ⇒ more conservative).
- Tier bands are half-open `[from, to)`.
- Meter timestamps: power readings (list 1/2) use **receive time** from the app's `Clock`
  (staleness stays meaningful and consistent with ticks); the cumulative register (list 3)
  uses the **meter's own clock** (hh:00:00, converted from local time via the calendar's zone),
  so the hour value is `Measured` rather than interpolated across a 10 s delay.
- Hour energy is upserted: `Meter::advance` emits an `Estimated` value at rollover; the
  register later supersedes it with `Measured`/`Interpolated`.
- `clippy.toml` disallows `Timestamp::now`, `Zoned::now`, `SystemTime::now` workspace-wide;
  only `SystemClock` may opt out with a local `#[allow]`.
- Hour limit simplified (user decision): target tier bound, relaxed to today's highest
  completed hour if higher, minus margin. No month-end relaxation, no "tier lost" mode.
- Zones learn `heating_kw` (EMA of power reports > 50 W, persisted) and record per-zone hourly
  kWh from the thermostat's counter (`50/1/value/65537`).
- Persisted guard event log + `GET /api/events` replace dashboards for reviewing observe mode.
- Kaifa frame date-times are local wall-clock time; the han crate resolves them with the
  configured zone, using the DST status bit to pick the right side of the fall-back hour.
- Kaifa list 3 (real capture 2026-11-15 22:00:10 local, 157 B): the meter clock reads hh:00:10,
  but the registers are the values captured at the hour shift (Norwegian HAN spec), so
  `Register::captured_at` moves a clock within the first minute of an hour back to hh:00:00.
- HAN fixtures in `tests/fixtures/han/` are real frames with the meter serial replaced by
  `1234567890123456` and the FCS recomputed. Never commit the real serial.
- **Tests live inside each crate** (`#[cfg(test)]` modules, e.g. `core/src/scenarios.rs`), not in
  `tests/` dirs: an integration-test binary links a second instrumented copy of the crate, and
  llvm-cov then reports functions only one binary calls as uncovered.
- Scenario/replay tests are a closed-loop Rust simulation (house + meter + echoing thermostats)
  instead of CSV traces: typed, readable, and Active-mode feedback is real. Snapshots via `insta`
  (`INSTA_UPDATE=new`, review the `.snap.new`, then rename) — no `cargo-insta` needed.
- Guard credit (observe mode): a shed zone still drawing is credited its whole saving since the
  shed; when such a zone is released its saving is **banked** for the rest of the hour, otherwise
  observe mode re-sheds everything after the first release.
- `Engine::new(config, persisted, now)` and `Input::SetMode { at, mode }` take time explicitly;
  outputs gained `WouldWrite`, `PersistZoneBase`, `PersistMode`, `PersistShedAllowed`.
- Core derives `serde::Serialize` (serde is pure) on status/event types so MQTT
  `energybrain/status`, HTTP `/api/state` and the stored event log share one JSON shape, pinned by
  insta snapshots in `core/src/io.rs`. Units serialize as bare numbers, timestamps as RFC 3339 UTC.
- `rumqttc`/`rumqttd` are used with `default-features = false` (no TLS; the broker is plain TCP).
- M7 MQTT tests: **embedded `rumqttd`** (in-process, free port, with username/password auth)
  — no Docker or testcontainers needed. Built from a TOML snippet since `rumqttd::Config` is serde.
- MQTT retained status topics are published only when their payload changes and all re-sent on
  every `ConnAck`. Setpoint writes go to `<setpoint>/set`, QoS 1, not retained.
- Coverage gotcha: llvm-cov's *summary* (what `--fail-under-lines` checks) sums each function's
  own line coverage, so a line holding only a `?` error branch after a closure (e.g. `})?;` closing
  a `query_map` closure) counts as missed even when `--show-missing-lines`/HTML show nothing. Fix
  by making the closure a named function so the `?` shares a line with covered code. To find such
  lines, export JSON (`cargo llvm-cov report --json`) and look for per-function zero regions.
- Upgraded to **jiff 1.0** (released 2026-11-15) across all crates: `Zoned` → `ZonedDateTime`,
  `to_zoned` → `to_zoned_datetime`. Keep every crate on the same jiff major.
- Store: per-zone state (base, last write, learned power, shed permission, override) lives in one
  `zones` row instead of separate `zone_writes`/`overrides` tables. Events are stored as their
  JSON plus indexed `at`/`kind`/`zone` columns.
- `/metrics` is rendered by hand in Prometheus text format from the latest `Status` plus write
  counters (`energybrain-http::Metrics`) instead of using `metrics` +
  `metrics-exporter-prometheus`: those need a process-global recorder, which is awkward to test
  and adds two dependencies. Metric names are unchanged from PLAN.md.
- `/healthz` is 503 with reasons when: no status yet, the last status is older than 30 s (engine
  loop stalled), the meter is stale/has no data, or MQTT is disconnected.
- App: `run()` creates the MQTT link itself (subscriptions come from the translator); MQTT
  events reach the `select!` loop through a forwarding task (cancel-safe), and the store runs on
  its own thread fed by a channel. `/api/events` opens a separate SQLite connection per query.
- Graceful stop (SIGTERM/Ctrl-C): `Engine::shutdown` restores shed zones in Active mode without
  changing the persisted mode, then MQTT is flushed (disconnect + wait), and the store thread and
  HTTP server are stopped.
- Deployment: `deploy/Dockerfile` (rust:1.100 build → distroless cc nonroot, `/data` volume,
  `/config/energybrain.toml`), `deploy/docker-compose.yml` (credentials from `deploy/.env`,
  both gitignored). The image can't be built locally (no Docker on the dev Mac), so CI builds
  it. jiff's `tzdb-bundle-always` embeds the tz database so `Europe/Oslo` resolves in distroless.
- Releases: publishing a GitHub release runs `.github/workflows/release.yml`, which reuses
  `ci.yml` (`workflow_call`) as a gate, then pushes `deploy/Dockerfile` (amd64 only) to Docker Hub
  as `<DOCKER_USERNAME>/energybrain` with tags `X.Y.Z`, `X.Y` and `latest` (not for
  pre-releases). Needs repo secrets `DOCKER_USERNAME` and `DOCKER_PASSWORD` (same names as the
  user's other Docker Hub projects). Running
  the published image is documented in `docs/docker.md` with `docs/compose.yml`.
- Toolchain is **pinned** to an exact version in `rust-toolchain.toml` (now 1.100.0); CI installs
  it with `rustup toolchain install` and the Dockerfile uses the matching `rust:X.Y` image. A
  floating `stable` let CI pick up Rust 1.100 (new `clippy::assert_is_empty`) while local was
  1.97, so `just check` failed only on CI. Bump all three together, deliberately.
  `rust-version` in `Cargo.toml` stays the MSRV (1.97). Empty-collection asserts use
  `assert_eq!(x, Vec::<T>::new())`. After switching toolchains run a full `cargo llvm-cov clean`
  (not `--workspace`): stale test binaries from the old rustc are otherwise merged into the
  report and show up as hundreds of missed lines. 1.100 also maps implicit `else`/`?` branches
  to lines more precisely, which exposed 4 real gaps (fixed).
