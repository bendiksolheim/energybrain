# energybrain

Keeps a Norwegian home inside its grid-rent capacity tier (*kapasitetsledd*) by briefly
adjusting energy-hungry entities when an hour is heading over the limit, and restoring them
afterwards. Right now it talks MQTT to a Tibber Pulse (HAN port), Heatit Z-TRM3 thermostats via
Z-Wave JS UI, and HomeKit via Homebridge's Easy MQTT plugin.

It ships in **observe mode**: it computes and reports everything but writes nothing until you
switch it to active.

## How it decides

- **The bill.** The monthly capacity charge is set by the average of the three highest hours
  of the month, counting only each day's highest hour. The tier table is
  `docs/tier-table.md` (Tensio TS 2026).
- **The limit.** Each hour may use up to the target tier's bound (5 kWh for 2–5 kW). If
  today already had a higher hour, up to that level is free. The guard acts on that value
  minus a 0.3 kWh safety margin.
- **The projection.** Energy used so far this hour plus a 60-second average of the current
  power for the rest of the hour.
- **Shedding.** When the projection exceeds the limit, floors that are actually heating are
  lowered by 4 °C, in priority order (**lower number first**), until the expected saving
  covers the excess. A floor is restored, one every 2 minutes and most recent first, once it
  has been down at least 5 minutes and the projection with it back on stays 0.3 kWh under
  the limit.
- **You win.** If you raise a lowered floor manually, it's left alone for 2 hours.
  Any other change you make becomes that floor's new normal setpoint.
- **Safety.**
  - A stale meter (no power reading for 2 minutes) restores everything.
  - Leaving active mode, or stopping the service, restores every lowered floor first.
  - After a restart, a floor that was lowered is picked up again rather than flapped.
- **Unreachable hours.** If even lowering every floor can't save the hour (for example an
  EV charger at 11 kW), all floors are lowered and a `limit_unreachable` event is logged.
  Observe mode will show you how often that happens.

## Running it

The service runs as one container on the home server (amd64). Every GitHub release is
published to Docker Hub as `bendiksolheim/energybrain`, and [`docs/compose.yml`](docs/compose.yml)
runs it. [`docs/docker.md`](docs/docker.md) covers setup, upgrades, backups and how releases
are published.

```sh
# in a directory holding compose.yml, energybrain.toml and .env (see docs/docker.md)
docker compose up -d
docker compose logs -f
```

- **Storage.** The SQLite database lives in the `energybrain-data` volume (`/data`).
- **Stopping.** `docker compose stop` sends SIGTERM, which restores any lowered floors
  before exiting.
- **From source.** `docker compose -f deploy/docker-compose.yml up -d --build` builds the image
  from a checkout. Without Docker, run `cargo run --release -p energybrain -- path/to/config.toml`,
  or set `ENERGYBRAIN_CONFIG`.
- **Logs.** Set `ENERGYBRAIN_LOG_FORMAT=json` for JSON logs (the image's default) and
  `RUST_LOG=debug` for more detail.

## Configuration

[`config/example.toml`](config/example.toml) is the full reference: every key is commented. Unknown keys are rejected, and every error names the key at fault. The keys you are most likely to touch:

| Key | Default | Meaning |
|---|---|---|
| `mqtt.host` / `port` / `prefix` | – / 1883 / `energybrain` | Broker, and the prefix of our own topics |
| `ENERGYBRAIN_MQTT_USERNAME` / `_PASSWORD` | – | Credentials (env vars override the file) |
| `tariff.target_kw` | – | Tier to stay in; must be one of the tiers' `to_kw` |
| `tariff.safety_margin_kwh` | 0.3 | Headroom below the allowed hour |
| `meter.power_window_secs` | 60 | Averaging window for the projection |
| `meter.stale_after_secs` | 120 | No power for this long ⇒ restore everything |
| `guard.min_shed_minutes` | 5 | Minimum time a floor stays lowered |
| `guard.restore_stagger_minutes` | 2 | Minimum time between two restores |
| `zones.override_minutes` | 120 | How long raising a lowered floor keeps it out of shedding |
| `[[zone]] priority` | – | Shed order, **lower first** |
| `[[zone]] shed_delta` | 4.0 | How far a floor is lowered (°C), never below `min_setpoint` |
| `[[zone]] shed_allowed` | true | Default; HomeKit can switch it per zone at runtime |
| `[[zone]] nominal_kw` | 1.0 | Load guess until the floor's heating power has been learned |
| `[ev]` | (absent) | The EV charger (Easee); see below |
| `ENERGYBRAIN_EASEE_USERNAME` / `_PASSWORD` | – | Easee account (env vars override the file) |
| `ev.pause_allowed` | false | Whether the guard may pause charging; otherwise 6 A is its floor |

Each zone's heater power is learned from its thermostat's power reports and remembered. Its
kWh counter is logged per hour.

**EV charger.** With an `[ev]` section, energybrain polls the Easee charger through Easee's
cloud API every 30 s and lowers its current *before* it lowers any floor (down to 6 A, or a
pause if allowed). Every limit expires by itself after 10 minutes unless renewed, so a stopped
energybrain hands the charger back to the app. The app's "Charger limit" stays the ceiling, and
a "Temporary charging limit" set in the app keeps energybrain away from the charger for two
hours or until the car is unplugged. See [docs/easee.md](docs/easee.md) and PLAN.md §9.

## MQTT topics

| Topic | Direction | Payload |
|---|---|---|
| `tibber` | in | Raw Kaifa HDLC frames from the Pulse (its JSON status is ignored) |
| `zwave/<Room>/<Device>/67/1/setpoint/1` | in | Reported setpoint, °C |
| `…/67/1/setpoint/1/set` | **out** | Setpoint we write (active mode only) |
| `…/50/1/value/66049` / `…/50/1/value/65537` | in | Power (W) / energy counter (kWh) |
| `energybrain/mode` · `/set` | out · in | `off` \| `observe` \| `active` (retained) |
| `energybrain/active` · `/set` | out · in | `1` = active, `0` = observe (for a HomeKit switch) |
| `energybrain/peakguard/active` | out | `1` while the guard is shedding |
| `energybrain/zone/<id>/shed` | out | `1` while that floor is lowered |
| `energybrain/zone/<id>/shed_allowed` · `/set` | out · in | `1`/`0`, persisted |
| `energybrain/ev/limited` | out | `1` while the charger's current is limited (with `[ev]`) |
| `energybrain/ev/shed_allowed` · `/set` | out · in | `1`/`0`, persisted (with `[ev]`) |
| `energybrain/status` | out | Full status as JSON (same as `/api/state`) |

`/set` topics accept `1`/`0`, `true`/`false` or `on`/`off`. Status topics are retained and are
republished after every reconnect.

## HomeKit (Homebridge Easy MQTT)

Add these accessories to the `HomebridgeEasyMQTT` platform. The easiest way is through the
Homebridge Config UI. The key names below come from the plugin's config schema; the plugin
docs don't include a complete `config.json` example.

```json
{
  "info": { "name": "Energibrain aktiv", "type": "Switch" },
  "topicGetOn": "energybrain/active",
  "topicSetOn": "energybrain/active/set",
  "valueOn": "1",
  "valueOff": "0"
},
{
  "info": { "name": "Effektvakt", "type": "ContactSensor" },
  "topicGetContactSensorState": "energybrain/peakguard/active",
  "valueContactDetected": "0"
},
{
  "info": { "name": "Bathroom: shed allowed", "type": "Switch" },
  "topicGetOn": "energybrain/zone/bathroom/shed_allowed",
  "topicSetOn": "energybrain/zone/bathroom/shed_allowed/set",
  "valueOn": "1",
  "valueOff": "0"
}
```

- **The contact sensor** reads "closed" normally and "open" while floors are lowered, so you
  can attach a HomeKit notification to it.
- **Per-zone switches.** Add one `shed_allowed` switch per zone, and one for
  `energybrain/ev/shed_allowed` if you have a charger configured.
- **Broker settings.** Each accessory's `mqtt` block needs your broker settings, as for your
  other Easy MQTT accessories.

## HTTP

Open `http://server:8080/` in a browser for the **history page**: graphs of a day (energy per
hour against the limit, power, the hour budget, the EV charger's current and limits, each
floor's setpoints and sheds, and the events) and of a month (the highest hour per day, the top 3, the tier, and a day × hour
heatmap). Minute-level detail is kept for `store.sample_retention_days` (default 90); older
days still show their hourly energy and events.

| Endpoint | What |
|---|---|
| `GET /` | The history page |
| `GET /api/day?date=<YYYY-MM-DD>` | One local day for the page: hours, 1-minute samples (columnar), events. Default today |
| `GET /api/month?month=<YYYY-MM>` | One local month: day peaks, top 3, tier, day × hour energy. Default this month |
| `GET /healthz` | 200, or 503 with reasons (engine stalled, meter stale, MQTT down) |
| `GET /api/state` | Mode, hour used and projected, limit and its basis, top-3 days, tier, and per zone the base, decision and reasons |
| `GET /api/events?since=<RFC 3339>&limit=<n>` | Event log, newest first (max 1000) |
| `GET /metrics` | Prometheus text (`energybrain_*`), for Grafana later |

In the JSON (`/api/state`, `/api/events`, `energybrain/status`), every physical quantity
carries its unit: `"used": {"value": 1.5, "unit": "kWh"}`. Units are `kW`, `kWh`, `°C`,
`K`, `kr`, `kvar`, `kvarh`, `A` and `V`; timestamps are RFC 3339 UTC strings. To get the
bare number, use e.g. `jq '.used.value'`.

## Reviewing observe mode

Leave the service running in observe mode for at least two weeks, including some cold
evenings. Then review what it would have done. The history page (`http://server:8080/`) is
the quickest way: the month view shows which days come close to the target, and a day's
*Hour budget* and *Zones* charts show each would-shed against the projection and the limit
(shaded where the guard was shedding). The same data from the command line:

```sh
# What would it have done this week?
curl -s 'http://server:8080/api/events?since=2026-11-16T00:00:00Z&limit=1000' \
  | jq -r '.[] | [.at, .mode, .kind.type, (.kind.event.type // ""), (.kind.event.zone // .kind.zone // "")] | @tsv'

# Hours where even lowering every floor would not have been enough:
curl -s 'http://server:8080/api/events?limit=1000' \
  | jq '[.[] | select(.kind.event.type == "limit_unreachable")]'

# Right now:
curl -s http://server:8080/api/state | jq '{mode, guard, used, projected, limit, top_days, tier}'
```

What to look for:

- **`shed` events.** In observe mode each one is a floor that *would* have been lowered.
  Compare them with evenings you remember: was the house really heading over 5 kWh?
- **The `release` events that follow.** They should come minutes later, not seconds.
  Back-and-forth would point to the margins being too tight.
- **`limit_unreachable` events.** Frequent ones mean 2–5 kW isn't realistic. Raise `target_kw`
  to the next level.
- **`top_days` in `/api/state`.** This is where the month's bill stands right now.

## Go-live checklist

1. [ ] Two or more weeks in observe mode, with no `shed` you consider false.
2. [ ] Check the would-shed events against real evenings (EV charging, cooking, sauna).
3. [ ] Every zone's `shed_allowed` is how you want it (the guest units especially).
4. [ ] `/healthz` stays 200, and the logs show no repeated MQTT disconnects or malformed
       messages.
5. [ ] Turn on the **Energibrain aktiv** switch in HomeKit, or publish `active` to
       `energybrain/mode/set`.
6. [ ] On the first evening, watch a lowered floor go down by 4 °C and come back.
       To stop it at any time, switch it off: every lowered floor is restored immediately.

## Development

```sh
just check   # fmt, clippy -D warnings, all tests, 100 % line coverage gate
just test
just run     # with config/example.toml (connects to the real broker!)
just dev     # against a simulated house, at 6× speed, no home network needed
```

- **Running locally.** [docs/dev.md](docs/dev.md) explains how `just dev` runs energybrain
  against a simulated house (meter, thermostats, heat pump, EV) on accelerated time, and
  how to change the house while it runs.

- **Design.** [PLAN.md](PLAN.md) holds the design, and [CLAUDE.md](CLAUDE.md) the working
  rules and decision log.
- **No external services in tests.** Tests run offline: MQTT uses an embedded broker, SQLite
  uses temp files, and HTTP uses `oneshot`. The HAN fixtures are real frames with the meter
  serial replaced.
