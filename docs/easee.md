# Easee: manual checks (PLAN.md §9, E0)

Before energybrain controls the charger, check three things by hand against Easee's cloud API:

1. that your account can read the charger;
2. which value each of the app's two limits changes;
3. whether the I-Pace resumes after a pause.

You need `curl` and `jq`. Everything runs in one shell, and the password is read without
echoing it, so it doesn't end up in your shell history. Nothing here is stored in the repo.

## 1. Log in and find the charger

```sh
read -r -p 'Easee user (phone or e-mail): ' EASEE_USER
read -r -s -p 'Easee password: ' EASEE_PASS; echo

TOKEN=$(jq -n --arg u "$EASEE_USER" --arg p "$EASEE_PASS" '{userName: $u, password: $p}' |
  curl -sS -H 'Content-Type: application/json' -d @- \
    https://api.easee.com/api/accounts/login | jq -r .accessToken)
unset EASEE_PASS
echo "token: ${TOKEN:0:12}…"

# Your chargers. Rate limited to 2 per minute, so run it once.
curl -sS -H "Authorization: Bearer $TOKEN" https://api.easee.com/api/chargers |
  jq '.[] | {id, name}'
```

`id` is the serial number (for example `EH123456`). Set it for the rest of the session:

```sh
SERIAL=EH123456
```

## 2. Read the values energybrain will use

```sh
obs() {
  curl -sS -H "Authorization: Bearer $TOKEN" \
    "https://api.easee.com/state/$SERIAL/observations?ids=47,48,109,114,120,96,250" |
  jq -r '.observations[] | "\(.id)\t\(.value)\t\(.timestamp)"' |
  sed -e 's/^47\t/47  max current (A)        \t/' \
      -e 's/^48\t/48  dynamic current (A)    \t/' \
      -e 's/^109\t/109 op mode                \t/' \
      -e 's/^114\t/114 offered to car (A)     \t/' \
      -e 's/^120\t/120 power (kW)             \t/' \
      -e 's/^96\t/96  reason for no current  \t/' \
      -e 's/^250\t/250 connected to cloud     \t/'
}
obs
```

Op mode: 0 offline, 1 disconnected, 2 awaiting start, 3 charging, 4 completed, 5 error,
6 ready to charge, 7 awaiting authentication, 8 de-authenticating.

This endpoint allows 100 requests per 5 minutes, so running `obs` by hand is fine.
Observations are only reported on change, so a value can be a few seconds behind the app.

## 3. Which app control changes which value

The plan assumes "Charger limit" is the max current (47) and "Temporary charging limit"
is the dynamic current (48). To check:

1. Run `obs` and note 47 and 48.
2. Change **Charger limit** in the app (for example 15 → 14 A), wait ~10 s, and run
   `obs`. Which value followed? Set it back.
3. Set a **Temporary charging limit** (for example 8 A) and run `obs`. Which value
   followed? Does the app ask for a duration, or show when the limit ends?
4. Remove the temporary limit (or unplug the car) and run `obs`. How does 48 read now:
   missing, `0`, `32` or the same as 47?

Please send me the answers to 2–4. Step 4 decides how energybrain tells "no limit" apart
from "the user set a limit".

## 4. Does the I-Pace resume after a pause?

With the car plugged in and charging (op mode 3), set the dynamic current the same way
energybrain will, with a TTL in minutes:

```sh
limit() {  # limit <amps> <minutes>
  curl -sS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"amps\": $1, \"minutes\": $2}" \
    "https://api.easee.com/api/chargers/$SERIAL/commands/set_dynamic_charger_current" |
  jq '{status, wasAccepted, resultCode, comment}'
}

limit 0 5     # pause for 5 minutes
obs           # 120 power should drop to ~0, op mode 4 or 6
limit 10 5    # back to 10 A
obs           # after ~30 s: op mode 3, power ~2.3 kW
```

- Repeat a few times. Also let the TTL run out once instead of setting 10 A, to check
  that the limit really clears by itself.
- Then do it once after the car has been parked for an hour or more without charging. A
  sleeping car is the case that usually fails.
- If it always resumes, `pause_allowed = true` is safe. If it ever doesn't, energybrain
  stays at 6 A as its lowest setting.

When you're done, clear the limit so the app's setting applies again:

```sh
limit 32 1
```

## Notes
- The access token expires (see `expiresIn` in the login response). If calls start
  returning 401, run the login again.
- Status `0` (Sent) from the command is normal: it means "sent to the charger". `2` is
  Executed, `3` Rejected.

## Enabling the charger in energybrain

Once the checks above look right, uncomment the `[ev]` section in your config (see
`config/example.toml`) and set the serial. Give the account through the environment rather
than the file:

```sh
ENERGYBRAIN_EASEE_USERNAME=…  ENERGYBRAIN_EASEE_PASSWORD=…
```

Start in observe mode and watch the `charger_limit` / `charger_release` events in
`/api/events` while the car charges, before switching to active (PLAN.md §9, go-live).
