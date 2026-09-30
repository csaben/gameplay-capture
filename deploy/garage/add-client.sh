#!/usr/bin/env bash
# Create an S3 access key for one client machine and grant it the bucket.
#   ./add-client.sh <name>
# Prints the key and a config snippet for the client (cap-app) to paste.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
NAME="${1:-}"
[[ "$NAME" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || die "usage: $0 <name>  (letters, digits, . _ -)"
load_env

if garage key info "$NAME" >/dev/null 2>&1; then
  die "key '$NAME' already exists (garage key info --show-secret $NAME; or garage key delete --yes $NAME)"
fi
OUT="$(garage key create "$NAME")"
KEY_ID="$(sed -n 's/^Key ID: *//p' <<<"$OUT" | head -n1 | tr -d '[:space:]')"
SECRET="$(sed -n 's/^Secret key: *//p' <<<"$OUT" | head -n1 | tr -d '[:space:]')"
[[ -n "$KEY_ID" && -n "$SECRET" ]] || { echo "$OUT" >&2; die "could not parse garage key create output"; }
# HEAD (size check after upload) needs read; write includes delete.
garage bucket allow --read --write "$GARAGE_BUCKET" --key "$KEY_ID" >/dev/null

cat <<SNIPPET
Created key for '$NAME'
  key id: $KEY_ID
  secret: $SECRET

# --- paste into the client's config.toml ---
[upload]
target = "s3"
user_id = "$NAME"
endpoint = "http://$GARAGE_BIND_ADDR:$GARAGE_S3_PORT"
region = "garage"
bucket = "$GARAGE_BUCKET"
access_key = "$KEY_ID"
secret_key = "$SECRET"
allow_http = true   # plain HTTP inside the tailnet (WireGuard-encrypted)
# ---
SNIPPET
