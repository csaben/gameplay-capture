#!/usr/bin/env bash
# Bring up a single-node Garage with the S3 API bound to one address
# (default: this machine's Tailscale IPv4), create the layout and the bucket.
#
#   ./setup.sh                 # bind to `tailscale ip -4`, port 3900
#   ./setup.sh 127.0.0.1       # local testing only
#
# Env overrides:
#   GARAGE_S3_PORT   (3900)             published S3 port
#   GARAGE_STATE_DIR (./state)          config, secrets, metadata
#   GARAGE_DATA_DIR  ($STATE/data)      object data (put this on the big disk)
#   GARAGE_PROJECT   (gameplay-garage)  docker compose project name
#   GARAGE_BUCKET    (gameplay)
#   GARAGE_CAPACITY  (free space of the data dir, e.g. 2T)
# Safe to re-run: existing secrets, layout and bucket are kept.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BIND_ADDR="${1:-}"
if [[ -z "$BIND_ADDR" ]]; then
  command -v tailscale >/dev/null || die "tailscale not found; pass the bind address explicitly"
  BIND_ADDR="$(tailscale ip -4 | head -n1)"
  [[ -n "$BIND_ADDR" ]] || die "tailscale ip -4 returned nothing (is tailscale up?)"
fi
case "$BIND_ADDR" in
  0.0.0.0|::|"[::]") [[ "${GARAGE_ALLOW_ALL_INTERFACES:-}" == 1 ]] || die "refusing to bind to all interfaces ($BIND_ADDR)";;
esac

GARAGE_S3_PORT="${GARAGE_S3_PORT:-3900}"
GARAGE_PROJECT="${GARAGE_PROJECT:-gameplay-garage}"
GARAGE_DATA_DIR="${GARAGE_DATA_DIR:-$GARAGE_STATE_DIR/data}"

mkdir -p "$GARAGE_STATE_DIR/meta" "$GARAGE_DATA_DIR"
chmod 700 "$GARAGE_STATE_DIR"
GARAGE_STATE_DIR="$(cd "$GARAGE_STATE_DIR" && pwd)"
GARAGE_DATA_DIR="$(cd "$GARAGE_DATA_DIR" && pwd)"
ENV_FILE="$GARAGE_STATE_DIR/garage.env"

if [[ ! -f "$GARAGE_STATE_DIR/garage.toml" ]]; then
  echo "Rendering $GARAGE_STATE_DIR/garage.toml with fresh secrets"
  sed -e "s/__RPC_SECRET__/$(openssl rand -hex 32)/" \
      -e "s/__ADMIN_TOKEN__/$(openssl rand -base64 32 | tr -d '/+=')/" \
      -e "s/__METRICS_TOKEN__/$(openssl rand -base64 32 | tr -d '/+=')/" \
      "$HERE/garage.toml" > "$GARAGE_STATE_DIR/garage.toml"
  chmod 600 "$GARAGE_STATE_DIR/garage.toml"
fi

cat > "$ENV_FILE" <<ENV
GARAGE_PROJECT=$GARAGE_PROJECT
GARAGE_BIND_ADDR=$BIND_ADDR
GARAGE_S3_PORT=$GARAGE_S3_PORT
GARAGE_STATE_DIR=$GARAGE_STATE_DIR
GARAGE_DATA_DIR=$GARAGE_DATA_DIR
GARAGE_BUCKET=$GARAGE_BUCKET
GARAGE_UID=$(id -u)
GARAGE_GID=$(id -g)
ENV
chmod 600 "$ENV_FILE"

compose up -d

echo -n "Waiting for Garage"
for _ in $(seq 1 60); do
  if garage status >/dev/null 2>&1; then echo " up"; break; fi
  echo -n "."; sleep 1
done
garage status >/dev/null 2>&1 || { compose logs --tail 50; die "garage did not come up"; }

NODE_ID="$(garage node id -q 2>/dev/null | tail -n1 | cut -d@ -f1)"
[[ -n "$NODE_ID" ]] || die "could not read node id"
if garage status | grep -q "NO ROLE ASSIGNED"; then
  CAP="${GARAGE_CAPACITY:-$(df --output=avail -BG "$GARAGE_DATA_DIR" | tail -n1 | tr -dc '0-9')G}"
  echo "Assigning layout: node ${NODE_ID:0:16}, zone dc1, capacity $CAP"
  garage layout assign -z dc1 -c "$CAP" "$NODE_ID"
  CUR="$(garage layout show | sed -n 's/.*Current cluster layout version: \([0-9]*\).*/\1/p' | head -n1)"
  garage layout apply --version "$(( ${CUR:-0} + 1 ))"
fi

if ! garage bucket info "$GARAGE_BUCKET" >/dev/null 2>&1; then
  garage bucket create "$GARAGE_BUCKET"
fi

echo
echo "Garage S3 API: http://$BIND_ADDR:$GARAGE_S3_PORT (region garage, bucket $GARAGE_BUCKET)"
echo "Next: ./add-client.sh <machine-name> for each recording machine."
