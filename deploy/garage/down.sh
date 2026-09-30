#!/usr/bin/env bash
# Stop Garage. With --purge also delete the state and data dirs (all objects!).
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
load_env
compose down
if [[ "${1:-}" == "--purge" ]]; then
  rm -rf "$GARAGE_DATA_DIR" "$GARAGE_STATE_DIR"
  echo "Removed $GARAGE_DATA_DIR and $GARAGE_STATE_DIR"
fi
