# Running locally against a simulated house

`just dev` runs energybrain on your machine against a simulated house, with time running
six times faster: an hour takes ten minutes. The simulated house sends the same MQTT messages,
in the same formats, as the real Tibber Pulse and Z-TRM3 thermostats, so energybrain runs
unchanged. You don't need the home network.

The simulation is for watching how energybrain behaves when you change things. It doesn't
check that the guard is correct: the scenario tests in `crates/core/src/scenarios.rs` do.

## Before the first run

You need Apple's [`container`](https://github.com/apple/container) CLI (1.0 or later); the
dev broker is mosquitto in a container. The first `container system start` may ask to
install a kernel.

## Running

```sh
just dev        # start mosquitto, delete the dev database, run energybrain + the house
                # Ctrl-C stops both
just dev-down   # stop and remove the broker container
```

Every run starts from scratch, at `start` in `config/sim.toml`: the database
(`target/dev/energybrain.db`) is deleted, and the broker is recreated without retained
messages. energybrain therefore knows no earlier days, so the limit starts at the target
tier's bound (5 kWh − 0.3 margin).

To try other settings without touching the committed files, copy them. Files in `config/`
other than `example.toml`, `dev.toml` and `sim.toml` are gitignored:

```sh
cp config/sim.toml config/cold.toml   # edit it
just dev config/dev.toml config/cold.toml
```

## What you see

The terminal shows energybrain's log: sheds, releases, writes and warnings. Every simulated
minute the house adds a line of its own:

```
INFO energybrain_sim::driver: Tue 17:42 6.31 kW = floors 2.90 + background 1.47 + heat pump 1.00 + appliances 0.00 + ev 0.00 + extra 0.00; heating: bad, gang, kjeller
```

The HTTP endpoints are on `127.0.0.1:8080`. All their timestamps are simulated time (UTC):

```sh
curl -s localhost:8080/api/state | jq '{mode, guard, used, projected, limit, tier}'
curl -s localhost:8080/api/state | jq '.zones[] | {id, base, decision, shed, reasons}'
curl -s 'localhost:8080/api/events?limit=20' \
  | jq -r '.[] | [.at, .kind.type, (.kind.event.type // ""), (.kind.event.zone // .kind.zone // "")] | @tsv'
```

On MQTT:

```sh
just dev-sub                 # everything energybrain publishes (energybrain/#)
just dev-sub sim/state       # the house: power by load, floor temperatures, setpoints
just dev-sub '#'             # everything, including binary HAN frames on `tibber`
```

## Time

energybrain measures every duration with the simulated clock, so all of them speed up. At
the default `factor = 6`:

| Setting (config/dev.toml) | Simulated | Real |
|---|---|---|
| one hour (the capacity tariff's unit) | 60 min | 10 min |
| `settle_secs` | 90 s | 15 s |
| `min_shed_minutes` | 5 min | 50 s |
| `restore_stagger_minutes` | 2 min | 20 s |
| `stale_after_secs` | 120 s | 20 s |
| `override_minutes` | 120 min | 20 min |

A higher `factor` gets through an evening in minutes; `factor = 60` makes an hour one
minute long. Above about 60×, MQTT's real round trip starts to show in simulated time
(100 ms real is 6 s simulated), so writes and echoes look a few seconds late.

## Changing the house while it runs

| Command | Effect |
|---|---|
| `just sim ev 7.4` | the car charges at 7.4 kW until `just sim ev off` |
| `just sim load 2.5` | 2.5 kW of extra load (sauna, heater, …); `just sim load 0` removes it |
| `just sim meter pause` | the meter goes silent (`resume` brings it back) |
| `just sim outdoor -20` | outdoor temperature: more heat pump, floors lose heat faster |
| `just sim zone/bad/setpoint 28` | someone turns the Bad thermostat to 28 °C on its panel |

`just dev-pub <topic> <value>` publishes anything else, for example energybrain's own
control topics.

## Things to try

- **The evening peak.** With the defaults, dinner (16–19, oven and kettle at random) and
  the floors push the hour past 5 kWh. Watch `bad` get lowered first, then `vaskerom`,
  and the floors come back one at a time once the hour has room.
- **An unreachable limit.** `just sim ev 7.4`: shedding every floor still can't keep the
  hour under the limit. energybrain reports `limit_unreachable` once. The daily session at
  22:00 in `sim.toml` does the same.
- **A stale meter.** `just sim meter pause` while floors are lowered. After
  `stale_after_secs`, about 20 s real, every floor is restored, one at a time, and
  `/healthz` turns 503. `just sim meter resume` brings the guard back.
- **A user override.** While `bad` is lowered, `just sim zone/bad/setpoint 28`. energybrain
  sees a setpoint above its cap, treats it as an override and leaves `bad` alone for
  `override_minutes`.
- **Observe mode.** `initial_mode = "observe"` in `sim.toml`, or live with
  `just dev-pub energybrain/mode/set observe`. The same `shed` and `release` events appear,
  with `"mode": "observe"`, and the log shows the setpoints energybrain would have
  written. No setpoint is actually changed.
- **Protecting a zone.** `just dev-pub energybrain/zone/bad/shed_allowed/set 0`, and the
  next floor in priority order is lowered instead.
- **Tuning the guard.** Change the margins, `min_shed_minutes` or `restore_stagger_minutes`
  in `config/dev.toml` and run the same evening again. With the same `seed`, the house makes
  the same random choices (noise, appliance starts), so the evenings are comparable.

## How the house is modelled

`config/sim.toml` documents every setting. In short:

- **Floors.** Each floor is one thermal mass. Its heater switches 0.5 °C either side of the
  setpoint, so a floor lowered by 4 °C stays off until it has really cooled, which takes
  hours. Heater sizes are guesses, except Vaskerom's measured ~575 W.
- **The rest of the house.** Background use by hour of day with random noise, a heat pump
  that draws more the colder it is outside, appliances started at random within their
  hours, and EV sessions.
- **The meter.** Power every 2.5 s, details every 10 s, and the hourly register at hh:00:10
  holding the value from the hour shift, all in the Kaifa frame format.
- **The thermostats.** Setpoint writes are echoed twice, power is reported on change and
  every 90 s, and the energy counter every 5 min.

Random choices follow `seed` and are drawn once per simulated minute, so two runs with the
same settings see the same appliances at the same times. They don't match to the second,
because energybrain's writes reach the house after a real-time delay.
