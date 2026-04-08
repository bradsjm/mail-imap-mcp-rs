#!/usr/bin/env bash
set -euo pipefail

IMAGE="greenmail/standalone:2.1.8"
NAME="mail-imap-mcp-rs-greenmail-inspector-test"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

EXTERNAL_ENDPOINT=0
if [[ -n "${GREENMAIL_HOST+x}" || -n "${GREENMAIL_SMTP_PORT+x}" || -n "${GREENMAIL_IMAP_PORT+x}" ]]; then
  EXTERNAL_ENDPOINT=1
fi

GREENMAIL_HOST="${GREENMAIL_HOST:-localhost}"
GREENMAIL_SMTP_PORT="${GREENMAIL_SMTP_PORT:-4025}"
GREENMAIL_IMAP_PORT="${GREENMAIL_IMAP_PORT:-4143}"
GREENMAIL_USER="${GREENMAIL_USER:-test@localhost}"
GREENMAIL_PASS="${GREENMAIL_PASS:-test}"
GREENMAIL_PRELOAD_DIR="${GREENMAIL_PRELOAD_DIR:-$REPO_ROOT/tests/fixtures/greenmail-preload}"

GREENMAIL_OPTS_DEFAULT="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=test:${GREENMAIL_PASS}@localhost -Dgreenmail.users.login=email -Dgreenmail.preload.dir=/greenmail-preload -Dgreenmail.verbose"
GREENMAIL_OPTS="${GREENMAIL_OPTS:-$GREENMAIL_OPTS_DEFAULT}"

started_local_container=0
GREENMAIL_TLS_DIR=""
GREENMAIL_CA_CERT=""
cleanup() {
  if [[ -n "$GREENMAIL_TLS_DIR" ]]; then
    rm -rf "$GREENMAIL_TLS_DIR" >/dev/null 2>&1 || true
  fi
  if [[ -n "$GREENMAIL_CA_CERT" ]]; then
    rm -f "$GREENMAIL_CA_CERT" >/dev/null 2>&1 || true
  fi
  if [[ "$started_local_container" -eq 1 ]]; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

probe_greenmail() {
  python3 - "$GREENMAIL_HOST" "$GREENMAIL_SMTP_PORT" "$GREENMAIL_IMAP_PORT" <<'PY'
import socket
import sys

host = sys.argv[1]
ports = [int(sys.argv[2]), int(sys.argv[3])]

for port in ports:
    try:
        with socket.create_connection((host, port), timeout=1.5):
            pass
    except Exception as exc:
        print(exc)
        sys.exit(1)
PY
}

make_temp_dir() {
  mktemp -d "${TMPDIR:-/tmp}/$1.XXXXXX"
}

make_temp_file() {
  mktemp "${TMPDIR:-/tmp}/$1.XXXXXX"
}

create_greenmail_tls_bundle() {
  GREENMAIL_TLS_DIR="$(make_temp_dir greenmail-tls)"
  local ca_cert_path="$GREENMAIL_TLS_DIR/test-ca-cert.pem"
  local ca_key_path="$GREENMAIL_TLS_DIR/test-ca-key.pem"
  local cert_path="$GREENMAIL_TLS_DIR/localhost-cert.pem"
  local csr_path="$GREENMAIL_TLS_DIR/localhost.csr"
  local key_path="$GREENMAIL_TLS_DIR/localhost-key.pem"
  local ext_path="$GREENMAIL_TLS_DIR/localhost-ext.cnf"
  local p12_path="$GREENMAIL_TLS_DIR/greenmail.p12"

  openssl req \
    -x509 \
    -newkey rsa:2048 \
    -keyout "$ca_key_path" \
    -out "$ca_cert_path" \
    -days 2 \
    -nodes \
    -subj "/CN=GreenMail Test CA" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" >/dev/null 2>&1

  openssl req \
    -newkey rsa:2048 \
    -keyout "$key_path" \
    -out "$csr_path" \
    -nodes \
    -subj "/CN=localhost" >/dev/null 2>&1

  cat >"$ext_path" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EOF

  openssl x509 \
    -req \
    -in "$csr_path" \
    -CA "$ca_cert_path" \
    -CAkey "$ca_key_path" \
    -CAcreateserial \
    -out "$cert_path" \
    -days 2 \
    -extfile "$ext_path" >/dev/null 2>&1

  openssl pkcs12 \
    -export \
    -inkey "$key_path" \
    -in "$cert_path" \
    -certfile "$ca_cert_path" \
    -out "$p12_path" \
    -name greenmail \
    -passout pass:changeit >/dev/null 2>&1

  GREENMAIL_CA_CERT="$ca_cert_path"
}

wait_for_greenmail() {
  local attempts=60
  local last_probe_error=""

  echo "Waiting for GreenMail on ${GREENMAIL_HOST}:${GREENMAIL_SMTP_PORT} and ${GREENMAIL_HOST}:${GREENMAIL_IMAP_PORT}"

  for _ in $(seq 1 "$attempts"); do
    if last_probe_error=$(probe_greenmail 2>&1); then
      return 0
    fi
    sleep 1
  done

  echo "GreenMail unreachable at ${GREENMAIL_HOST}:${GREENMAIL_IMAP_PORT} after ${attempts}s: ${last_probe_error}" >&2
  return 1
}

ensure_docker_available() {
  if ! command -v docker >/dev/null 2>&1; then
    cat >&2 <<EOF
docker is required to start GreenMail automatically.

Options:
  1) Install Docker (or provide a docker-compatible CLI on PATH)
  2) Use an externally managed GreenMail endpoint by setting one or more of:
     GREENMAIL_HOST, GREENMAIL_SMTP_PORT, GREENMAIL_IMAP_PORT
EOF
    exit 1
  fi
}

if [[ "$EXTERNAL_ENDPOINT" -eq 0 ]] && probe_greenmail >/dev/null 2>&1; then
  EXTERNAL_ENDPOINT=1
  echo "Detected running GreenMail endpoint on default host/ports"
fi

if [[ "$EXTERNAL_ENDPOINT" -eq 1 ]]; then
  echo "Using externally managed GreenMail endpoint"
else
  ensure_docker_available

  if [[ ! -d "$GREENMAIL_PRELOAD_DIR" ]]; then
    echo "missing preload fixture directory: $GREENMAIL_PRELOAD_DIR" >&2
    exit 1
  fi

  create_greenmail_tls_bundle
  GREENMAIL_OPTS="$GREENMAIL_OPTS -Dgreenmail.tls.keystore.file=/greenmail-tls/greenmail.p12 -Dgreenmail.tls.keystore.password=changeit -Dgreenmail.tls.key.password=changeit"

  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker pull "$IMAGE"

  docker run -d --rm --name "$NAME" \
    -e GREENMAIL_OPTS="$GREENMAIL_OPTS" \
    -v "$GREENMAIL_PRELOAD_DIR:/greenmail-preload:ro" \
    -v "$GREENMAIL_TLS_DIR:/greenmail-tls:ro" \
    -p "$GREENMAIL_SMTP_PORT:3025" \
    -p "$GREENMAIL_IMAP_PORT:3993" \
    "$IMAGE"

  started_local_container=1
fi

wait_for_greenmail

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for inspector assertions" >&2
  exit 1
fi

if ! command -v npx >/dev/null 2>&1; then
  echo "npx is required for inspector execution" >&2
  exit 1
fi

if ! command -v openssl >/dev/null 2>&1; then
  echo "openssl is required to extract the GreenMail test certificate" >&2
  exit 1
fi

cd "$REPO_ROOT"

echo "Building server binary"
cargo build --quiet

SERVER_BIN="$REPO_ROOT/target/debug/mail-imap-mcp-rs"

if [[ -z "$GREENMAIL_CA_CERT" ]]; then
  GREENMAIL_CA_CERT="$(make_temp_file greenmail-ca-cert)"
  openssl s_client \
    -showcerts \
    -connect "${GREENMAIL_HOST}:${GREENMAIL_IMAP_PORT}" \
    -servername "${GREENMAIL_HOST}" \
    </dev/null 2>/dev/null \
    | awk '
        /-----BEGIN CERTIFICATE-----/ { capture=1 }
        capture { print }
        /-----END CERTIFICATE-----/ { exit }
      ' >"$GREENMAIL_CA_CERT"
fi

if [[ ! -s "$GREENMAIL_CA_CERT" ]]; then
  echo "failed to extract GreenMail TLS certificate from ${GREENMAIL_HOST}:${GREENMAIL_IMAP_PORT}" >&2
  exit 1
fi

export MAIL_IMAP_DEFAULT_HOST="$GREENMAIL_HOST"
export MAIL_IMAP_DEFAULT_PORT="$GREENMAIL_IMAP_PORT"
export MAIL_IMAP_DEFAULT_SECURE="true"
export MAIL_IMAP_DEFAULT_USER="$GREENMAIL_USER"
export MAIL_IMAP_DEFAULT_PASS="$GREENMAIL_PASS"
export MAIL_IMAP_CA_CERT_PATH="$GREENMAIL_CA_CERT"
export MAIL_IMAP_WRITE_ENABLED="true"

run_inspector() {
  npx --yes @modelcontextprotocol/inspector "$SERVER_BIN" --cli "$@"
}

expect_failure_with_text() {
  local expected_text="$1"
  shift
  set +e
  local output
  output=$(run_inspector "$@" 2>&1)
  local exit_code=$?
  set -e

  if [[ "$exit_code" -eq 0 ]]; then
    echo "Expected inspector call to fail" >&2
    echo "$output" >&2
    exit 1
  fi

  if [[ "$output" != *"$expected_text"* ]]; then
    echo "Inspector failure did not include expected text: ${expected_text}" >&2
    echo "$output" >&2
    exit 1
  fi
}

echo "Checking MCP tool discovery"
TOOLS_JSON=$(run_inspector --method tools/list)
printf '%s\n' "$TOOLS_JSON" | jq -e '
  .tools | map(.name) as $names
  | [
      "imap_list_accounts",
      "imap_list_mailboxes",
      "imap_search_messages",
      "imap_get_message",
      "imap_get_message_raw",
      "imap_apply_to_messages",
      "imap_manage_mailbox"
    ]
  | all(. as $tool | ($names | index($tool) != null))
' >/dev/null

echo "Checking imap_list_accounts"
LIST_ACCOUNTS_JSON=$(run_inspector --method tools/call --tool-name imap_list_accounts)
printf '%s\n' "$LIST_ACCOUNTS_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ((($data.accounts // []) | map(.account_id)) | index("default") != null)
' >/dev/null

echo "Checking imap_list_mailboxes"
MAILBOXES_JSON=$(run_inspector --method tools/call --tool-name imap_list_mailboxes --tool-arg account_id=default)
printf '%s\n' "$MAILBOXES_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and ((($data.mailboxes // []) | map(.name)) | index("INBOX") != null)
' >/dev/null

echo "Checking imap_search_messages"
SEARCH_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_search_messages \
  --tool-arg account_id=default \
  --tool-arg mailbox=INBOX \
  --tool-arg limit=5)
printf '%s\n' "$SEARCH_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and (($data.messages | length) > 0)
' >/dev/null

MESSAGE_ID=$(printf '%s\n' "$SEARCH_JSON" | jq -r '(.structuredContent.data // .data).messages[0].message_id // empty')
if [[ -z "$MESSAGE_ID" ]]; then
  echo "No message_id returned from imap_search_messages" >&2
  exit 1
fi

echo "Checking imap_get_message"
GET_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_get_message \
  --tool-arg account_id=default \
  --tool-arg "message_id=${MESSAGE_ID}")
printf '%s\n' "$GET_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and ($data.message.message_id != null)
    and ($data.message.subject != null)
' >/dev/null

echo "Checking imap_get_message_raw"
RAW_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_get_message_raw \
  --tool-arg account_id=default \
  --tool-arg "message_id=${MESSAGE_ID}" \
  --tool-arg max_bytes=200000)
printf '%s\n' "$RAW_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and (($data.size_bytes // 0) > 0)
    and (($data.raw_source_base64 // "") | length > 0)
' >/dev/null

echo "Checking imap_apply_to_messages dry run"
APPLY_DRY_RUN_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg account_id=default \
  --tool-arg "selector={\"message_ids\":[\"${MESSAGE_ID}\"]}" \
  --tool-arg action=update_flags \
  --tool-arg 'add_flags=["\\Seen"]' \
  --tool-arg dry_run=true)
printf '%s\n' "$APPLY_DRY_RUN_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and ($data.dry_run == true)
    and ($data.matched == 1)
    and (($data.results | length) == 1)
    and ($data.results[0].status == "planned")
' >/dev/null

MAILBOX_BASE="Inspector MCP $(date +%s)"
MAILBOX_CREATE="${MAILBOX_BASE}/Child"
MAILBOX_RENAME="${MAILBOX_BASE}/Renamed"

echo "Checking imap_manage_mailbox create"
MANAGE_CREATE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=create \
  --tool-arg "mailbox=${MAILBOX_CREATE}")
printf '%s\n' "$MANAGE_CREATE_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and ($data.action == "create")
' >/dev/null

echo "Checking imap_manage_mailbox rename"
MANAGE_RENAME_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=rename \
  --tool-arg "mailbox=${MAILBOX_CREATE}" \
  --tool-arg "destination_mailbox=${MAILBOX_RENAME}")
printf '%s\n' "$MANAGE_RENAME_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and ($data.action == "rename")
' >/dev/null

echo "Checking imap_manage_mailbox delete"
MANAGE_DELETE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=delete \
  --tool-arg "mailbox=${MAILBOX_RENAME}")
printf '%s\n' "$MANAGE_DELETE_JSON" | jq -e '
  (.structuredContent.data // .data) as $data
  | (.isError != true)
    and ($data.status == "ok")
    and ($data.action == "delete")
' >/dev/null

echo "Checking write-path policy enforcement over MCP"
export MAIL_IMAP_WRITE_ENABLED="false"

expect_failure_with_text "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true" \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg account_id=default \
  --tool-arg "selector={\"message_ids\":[\"${MESSAGE_ID}\"]}" \
  --tool-arg action=update_flags \
  --tool-arg 'add_flags=["\\Seen"]'

expect_failure_with_text "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true" \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=create \
  --tool-arg "mailbox=${MAILBOX_BASE}/Disabled"

echo "MCP inspector GreenMail integration checks passed"
