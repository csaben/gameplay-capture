# Shared helpers for setup.sh / add-client.sh / down.sh. Source, don't run.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GARAGE_STATE_DIR="${GARAGE_STATE_DIR:-$HERE/state}"
ENV_FILE="$GARAGE_STATE_DIR/garage.env"
GARAGE_BUCKET="${GARAGE_BUCKET:-gameplay}"

die() { echo "error: $*" >&2; exit 1; }

load_env() {
  [[ -f "$ENV_FILE" ]] || die "$ENV_FILE not found; run ./setup.sh first (or set GARAGE_STATE_DIR)"
  # shellcheck disable=SC1090
  set -a; source "$ENV_FILE"; set +a
}

compose() {
  docker compose -f "$HERE/docker-compose.yml" --env-file "$ENV_FILE" -p "$GARAGE_PROJECT" "$@"
}

garage() {
  compose exec -T -e RUST_LOG=warn garage /garage "$@"
}
