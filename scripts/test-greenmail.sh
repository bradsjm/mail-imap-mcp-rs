#!/usr/bin/env bash
set -euo pipefail

IMAGE="greenmail/standalone:2.1.8"
NAME="mail-imap-mcp-rs-greenmail-test"
GREENMAIL_HOST="${GREENMAIL_HOST:-localhost}"
GREENMAIL_IMAPS_PORT="${GREENMAIL_IMAPS_PORT:-3993}"
GREENMAIL_USER="${GREENMAIL_USER:-test@localhost}"
GREENMAIL_PASS="${GREENMAIL_PASS:-test}"
GREENMAIL_PRELOAD_DIR="${GREENMAIL_PRELOAD_DIR:-$(pwd)/tests/fixtures/greenmail-preload}"

GREENMAIL_OPTS_DEFAULT="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=test:${GREENMAIL_PASS}@localhost -Dgreenmail.users.login=email -Dgreenmail.preload.dir=/greenmail-preload -Dgreenmail.verbose"
GREENMAIL_OPTS="${GREENMAIL_OPTS:-$GREENMAIL_OPTS_DEFAULT}"

use_external=0
if [[ "${1:-}" == "--external" ]]; then
  use_external=1
fi

if [[ "$use_external" -eq 0 ]]; then
  if [[ ! -d "$GREENMAIL_PRELOAD_DIR" ]]; then
    echo "missing preload fixture directory: $GREENMAIL_PRELOAD_DIR" >&2
    exit 1
  fi

  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker pull "$IMAGE"

  docker run -d --rm --name "$NAME" \
    -e GREENMAIL_OPTS="$GREENMAIL_OPTS" \
    -v "$GREENMAIL_PRELOAD_DIR:/greenmail-preload:ro" \
    -p 3025:3025 \
    -p 3110:3110 \
    -p 3143:3143 \
    -p 3465:3465 \
    -p 3993:3993 \
    -p 3995:3995 \
    "$IMAGE"

  cleanup() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  }
  trap cleanup EXIT
fi

GREENMAIL_HOST="$GREENMAIL_HOST" \
GREENMAIL_IMAPS_PORT="$GREENMAIL_IMAPS_PORT" \
GREENMAIL_USER="$GREENMAIL_USER" \
GREENMAIL_PASS="$GREENMAIL_PASS" \
cargo test greenmail -- --ignored --nocapture
