#!/usr/bin/env bash
set -euo pipefail

IMAGE="greenmail/standalone:1.6.15"
NAME="mail-imap-mcp-rs-greenmail-test"

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker pull "$IMAGE"

docker run -d --rm --name "$NAME" \
  -e GREENMAIL_OPTS='-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.auth.disabled -Dgreenmail.verbose' \
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

RUN_GREENMAIL_TESTS=1 GREENMAIL_HOST=localhost GREENMAIL_IMAPS_PORT=3993 cargo test greenmail -- --nocapture
