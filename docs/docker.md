# Running energybrain with Docker Compose

Each GitHub release publishes an image to Docker Hub as `bendiksolheim/energybrain`
(amd64 only). [`compose.yml`](compose.yml) in this folder runs it.

## Tags

| Tag | Points at |
|---|---|
Releases are numbered 1, 2, 3, … with no minor or patch part.

| Tag | Points at |
|---|---|
| `3` | Exactly that release |
| `4-rc.1` | A pre-release |
| `latest` | The newest release that isn't a pre-release |

Pin a number such as `3` in production so an upgrade only happens when you change the tag.

## First start

On the server, make a directory with three files:

```sh
mkdir -p ~/energybrain && cd ~/energybrain

# 1. The compose file
curl -fsSLo compose.yml \
  https://raw.githubusercontent.com/bendiksolheim/energybrain/main/docs/compose.yml

# 2. The configuration: start from the example and edit it for your house
curl -fsSLo energybrain.toml \
  https://raw.githubusercontent.com/bendiksolheim/energybrain/main/config/example.toml

# 3. The broker credentials (kept out of the config file)
cat > .env <<'ENV'
ENERGYBRAIN_MQTT_USERNAME=...
ENERGYBRAIN_MQTT_PASSWORD=...
ENV
chmod 600 .env

docker compose up -d
docker compose logs -f
```

Check that the config keeps the container paths the compose file expects:

- `[http] listen = "0.0.0.0:8080"`. With `127.0.0.1` the published port can't reach it.
- `[store] path = "/data/energybrain.db"`, so the database ends up on the volume.

Every key is described in [`config/example.toml`](../config/example.toml). Unknown keys and
bad values stop startup with the key named in the log, so `docker compose logs` is the first
place to look if the container keeps restarting.

## Is it running?

```sh
curl -s http://localhost:8080/healthz              # 200, or 503 with the reasons
curl -s http://localhost:8080/api/state | jq .     # mode, this hour, limit, zones
```

The history page is at `http://<host>:8080/`. Its minute graphs fill in from the first start
on; `store.sample_retention_days` (default 90) sets how long they are kept, at about 0.5 MB a
day in `/data`.

`/healthz` returns 503 until the first meter reading arrives and whenever MQTT is
disconnected. If it stays 503, check the broker address and credentials, and that the Tibber
Pulse publishes to `[meter] topic`.

The service starts in **observe mode** and writes no setpoints. See the
[README](../README.md#reviewing-observe-mode) for how to review it, and the go-live checklist
before switching it to active.

## Day to day

| Task | Command |
|---|---|
| Logs | `docker compose logs -f` |
| Upgrade | Change the tag in `compose.yml` (or keep `latest`), then `docker compose pull && docker compose up -d` |
| Stop | `docker compose stop` (SIGTERM restores any lowered floors first) |
| Config change | Edit `energybrain.toml`, then `docker compose restart` |
| Back up the database | `docker compose cp energybrain:/data/energybrain.db ./backup.db`, ideally while stopped |

- **The data volume.** `energybrain-data` holds the SQLite database: the month's peak hours,
  each floor's learned base setpoint and heater power, the mode and the event log. Removing it
  (`docker compose down -v`) starts the month's peak history over, so don't.
- **A directory instead of a volume.** The container runs as the distroless `nonroot` user
  (uid 65532). If you swap the named volume for a bind mount such as `./data:/data`, make that
  directory writable for it first: `sudo chown 65532:65532 data`.

## Building from source instead

[`deploy/docker-compose.yml`](../deploy/docker-compose.yml) builds the image from a checkout
instead of pulling it:

```sh
docker compose -f deploy/docker-compose.yml up -d --build
```

## Publishing a release (maintainers)

`.github/workflows/release.yml` runs when a GitHub release is published: it runs the full
`just check` gate, then builds `deploy/Dockerfile` and pushes the tags above. It needs, under
*Settings → Secrets and variables → Actions*:

- the **secret** `DOCKER_USERNAME`: the Docker Hub account that owns the repository;
- the **secret** `DOCKER_PASSWORD`: a Docker Hub access token with *Read & Write* scope
  (the account password works too, but a token can be revoked on its own).

Tag releases `v<N>`, one higher than the last (`v1`, `v2`, …); the `v` is optional. Mark a
release as a pre-release and give it a suffix (`v4-rc.1`) to publish it without moving
`latest`. Any other tag name fails the workflow before anything is built.
