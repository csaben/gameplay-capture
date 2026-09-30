# Garage on the tailnet box (Phase 1)

A single-node [Garage](https://garagehq.deuxfleurs.fr/) (S3-compatible, written in Rust) running in
Docker on the Ubuntu training box. Clients upload segments straight to it with `cap-upload`
(`Target::S3`), each machine with its own access key. The S3 API is published **only on the
Tailscale IP**. Nothing is reachable from the LAN or the internet.

| File | Purpose |
|---|---|
| `docker-compose.yml` | The Garage container (`dxflrs/garage:v2.4.1`), S3 port published on one address |
| `garage.toml` | Config template. `setup.sh` renders it with fresh secrets into `$GARAGE_STATE_DIR` |
| `setup.sh [bind-addr]` | Renders config, starts the container, assigns the layout, creates bucket `gameplay` |
| `add-client.sh <name>` | Creates one access key per client machine and prints a config snippet |
| `down.sh [--purge]` | Stops the container. `--purge` also deletes the state and data dirs (all objects) |

## Setup

Prerequisites: Docker with the compose plugin, `openssl`, and Tailscale up on this box.

```bash
cd deploy/garage
# Put object data on the big disk; metadata and secrets stay in ./state (git-ignored, mode 700).
GARAGE_DATA_DIR=/data/garage ./setup.sh          # binds to $(tailscale ip -4):3900
```

Settings you can override with environment variables (they are saved to `state/garage.env`, so
`add-client.sh` and `down.sh` pick them up later):

| Variable | Default | Meaning |
|---|---|---|
| first argument | `tailscale ip -4` | Address the S3 API is published on. `0.0.0.0` is refused |
| `GARAGE_S3_PORT` | `3900` | Published S3 port |
| `GARAGE_STATE_DIR` | `./state` | Rendered `garage.toml`, env file, LMDB metadata |
| `GARAGE_DATA_DIR` | `$GARAGE_STATE_DIR/data` | Object data |
| `GARAGE_CAPACITY` | free space on the data disk | Layout capacity (used for balancing only) |
| `GARAGE_BUCKET` | `gameplay` | Bucket name |
| `GARAGE_PROJECT` | `gameplay-garage` | Compose project name |

The RPC port (3901) and admin API (3903) are not published. Manage the node with
`docker compose -p gameplay-garage exec garage /garage ...`, or source `common.sh` and use its
`garage` function. `setup.sh` is idempotent: running it again keeps the secrets, layout and
bucket.

The container runs as your UID, so the state dirs can be deleted without sudo. It uses
`restart: unless-stopped`. Docker can publish on the Tailscale IP only after `tailscaled` has
brought up `tailscale0`. If the container fails to start after a reboot, add a systemd drop-in
for `docker.service` with `After=tailscaled.service` and `Wants=tailscaled.service`, or run
`./setup.sh` again.

## Adding a client machine

```bash
./add-client.sh gaming-pc
```

The script prints the key ID and secret, plus a snippet to paste into the client config:

```toml
[upload]
target = "s3"
user_id = "gaming-pc"                  # becomes raw/<user_id>/... in the bucket
endpoint = "http://100.x.y.z:3900"
region = "garage"
bucket = "gameplay"
access_key = "GK..."
secret_key = "..."
allow_http = true
```

This maps onto `cap_upload::Target::S3 { endpoint, region, bucket, access_key, secret_key,
allow_http }` with `UploadConfig { user_id, .. }`. Plain HTTP is fine here: the traffic stays
inside WireGuard. Each key gets read and write on the bucket. Read is needed for the HEAD size
check after upload. Keys are per machine, so revoking one machine means
`garage key delete --yes <name>`. In Phase 1 every key can read the whole bucket. That is fine
for your own machines, and Phase 2 (ingest-api + presigned URLs) removes keys from clients
entirely.

The shard pipeline on this box can use its own key (`./add-client.sh pipeline`) against
`http://<tailscale-ip>:3900`. If `ingest-api` runs here too, create an `ingest-api` key for it.

## Check that clients connect directly

Run this from each client machine:

```bash
tailscale ping <this-box-name-or-100.x.y.z>
```

You want `pong from ... via <ip>:<port>` (a direct path). `via DERP(xxx)` means traffic is
relayed, which will be slow for gigabytes of video. Fix NAT/firewall (UDP 41641) until it is
direct. `tailscale status` shows `direct` or `relay` per peer. Then check the S3 endpoint from
the client:

```bash
curl -sI http://<tailscale-ip>:3900/    # any HTTP response (e.g. 403) means it's reachable
```

## Local test instance (what the integration tests use)

```bash
export GARAGE_STATE_DIR=$PWD/state-test GARAGE_S3_PORT=39000 GARAGE_PROJECT=gcap-test GARAGE_CAPACITY=10G
./setup.sh 127.0.0.1
./add-client.sh itest            # -> key id / secret
GARAGE_TEST_ENDPOINT=http://127.0.0.1:39000 GARAGE_TEST_KEY_ID=GK... GARAGE_TEST_SECRET=... \
  cargo test -p cap-upload --test garage
./down.sh --purge
```

## Backups and durability

`replication_factor = 1`: there is one copy on one disk. Raw segments can be rebuilt into
shards, but they cannot be recaptured. Back up `GARAGE_DATA_DIR` together with
`state/meta` (Garage snapshots metadata every 6 h into `meta/snapshots`), or add nodes later.
Moving to R2 (Phase 2) only changes the endpoint and credentials.
