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

assert_json() {
  local description="$1"
  local json="$2"
  local jq_program="$3"
  shift 3

  if ! printf '%s\n' "$json" | jq -e "$@" "$jq_program" >/dev/null; then
    echo "Assertion failed: ${description}" >&2
    printf '%s\n' "$json" >&2
    exit 1
  fi
}

assert_tool_schema() {
  local tool_name="$1"
  local description="$2"
  local jq_program="$3"

  if ! printf '%s\n' "$TOOLS_JSON" | jq -e --arg name "$tool_name" "$jq_program" >/dev/null; then
    echo "Schema assertion failed for ${tool_name}: ${description}" >&2
    printf '%s\n' "$TOOLS_JSON" >&2
    exit 1
  fi
}

echo "Checking MCP tool discovery and parameter contracts"
TOOLS_JSON=$(run_inspector --method tools/list)
assert_json "all expected tools are listed" "$TOOLS_JSON" '
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
'
assert_json "all published input schemas remain client-safe" "$TOOLS_JSON" '
  def unsafe_node:
    .. | objects | select(
      has("oneOf")
      or has("anyOf")
      or has("allOf")
      or has("not")
      or has("if")
      or has("then")
      or has("else")
      or has("dependentSchemas")
      or has("dependentRequired")
      or has("patternProperties")
      or has("unevaluatedProperties")
      or has("propertyNames")
      or ((has("additionalProperties")) and (.additionalProperties != false))
    );
  (.tools | all(.inputSchema.type == "object" and (.inputSchema.properties | type == "object")))
  and (([.tools[] | .inputSchema | unsafe_node] | length) == 0)
'

assert_tool_schema "imap_list_accounts" "no-input contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.properties | length) == 0)
    and ((($schema.required // []) | length) == 0)
'
assert_tool_schema "imap_list_mailboxes" "account_id parameter contract" '
  def has_type($schema; $type):
    ($schema.type == $type) or (($schema.type | type) == "array" and ($schema.type | index($type) != null));
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.properties | has("account_id")))
    and has_type($schema.properties.account_id; "string")
    and ($schema.properties.account_id.pattern == "^[A-Za-z0-9_-]+$")
    and ((($schema.required // []) | index("account_id")) == null)
'
assert_tool_schema "imap_search_messages" "search parameter contract" '
  def has_type($schema; $type):
    ($schema.type == $type) or (($schema.type | type) == "array" and ($schema.type | index($type) != null));
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("mailbox") != null)
    and (($schema.properties | has("account_id")))
    and (($schema.properties | has("mailbox")))
    and (($schema.properties | has("cursor")))
    and (($schema.properties | has("query")))
    and (($schema.properties | has("from")))
    and (($schema.properties | has("to")))
    and (($schema.properties | has("subject")))
    and (($schema.properties | has("unread_only")))
    and (($schema.properties | has("last_days")))
    and ($schema.properties.last_days.minimum == 1)
    and ($schema.properties.last_days.maximum == 365)
    and (($schema.properties | has("start_date")))
    and ($schema.properties.start_date.pattern == "^\\d{4}-\\d{2}-\\d{2}$")
    and (($schema.properties | has("end_date")))
    and ($schema.properties.end_date.pattern == "^\\d{4}-\\d{2}-\\d{2}$")
    and has_type($schema.properties.limit; "integer")
    and ($schema.properties.limit.minimum == 1)
    and ($schema.properties.limit.maximum == 50)
    and has_type($schema.properties.include_snippet; "boolean")
    and (($schema.properties | has("snippet_max_chars")))
    and ($schema.properties.snippet_max_chars.minimum == 50)
    and ($schema.properties.snippet_max_chars.maximum == 500)
'
assert_tool_schema "imap_get_message" "get_message parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("message_id") != null)
    and (($schema.properties | has("account_id")))
    and (($schema.properties | has("message_id")))
    and (($schema.properties | has("body_max_chars")))
    and ($schema.properties.body_max_chars.minimum == 100)
    and ($schema.properties.body_max_chars.maximum == 20000)
    and (($schema.properties | has("include_headers")))
    and (($schema.properties | has("include_all_headers")))
    and (($schema.properties | has("include_html")))
    and (($schema.properties | has("extract_attachment_text")))
    and (($schema.properties | has("attachment_text_max_chars")))
    and ($schema.properties.attachment_text_max_chars.minimum == 100)
    and ($schema.properties.attachment_text_max_chars.maximum == 50000)
'
assert_tool_schema "imap_get_message_raw" "get_message_raw parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("message_id") != null)
    and (($schema.properties | has("account_id")))
    and (($schema.properties | has("message_id")))
    and (($schema.properties | has("max_bytes")))
    and ($schema.properties.max_bytes.minimum == 1024)
    and ($schema.properties.max_bytes.maximum == 1000000)
'
assert_tool_schema "imap_apply_to_messages" "apply_to_messages parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("selector") != null)
    and (($schema.required // []) | index("action") != null)
    and (($schema.properties | has("account_id")))
    and (($schema.properties | has("action")))
    and ($schema.properties.action.enum == ["move", "copy", "delete", "update_flags"])
    and (($schema.properties | has("destination_mailbox")))
    and (($schema.properties | has("destination_account_id")))
    and (($schema.properties | has("add_flags")))
    and ($schema.properties.add_flags.items.type == "string")
    and (($schema.properties | has("remove_flags")))
    and ($schema.properties.remove_flags.items.type == "string")
    and (($schema.properties | has("max_messages")))
    and ($schema.properties.max_messages.minimum == 1)
    and ($schema.properties.max_messages.maximum == 1000)
    and (($schema.properties | has("dry_run")))
    and (($schema.properties.selector."$ref" | type) == "string")
    and (($schema["$defs"].MessageSelectorInput.properties | has("message_ids")))
    and (($schema["$defs"].MessageSelectorInput.properties | has("search")))
    and ($schema["$defs"].MessageSelectorInput.properties.message_ids.items.type == "string")
    and (($schema["$defs"].MessageSelectorInput.properties.search."$ref" | type) == "string")
    and (($schema["$defs"].SearchSelectorInput.properties | has("mailbox")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("cursor")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("query")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("from")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("to")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("subject")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("unread_only")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("last_days")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("start_date")))
    and (($schema["$defs"].SearchSelectorInput.properties | has("end_date")))
    and ($schema["$defs"].SearchSelectorInput.properties.last_days.minimum == 1)
    and ($schema["$defs"].SearchSelectorInput.properties.last_days.maximum == 365)
    and ($schema["$defs"].SearchSelectorInput.properties.start_date.pattern == "^\\d{4}-\\d{2}-\\d{2}$")
    and ($schema["$defs"].SearchSelectorInput.properties.end_date.pattern == "^\\d{4}-\\d{2}-\\d{2}$")
'
assert_tool_schema "imap_manage_mailbox" "manage_mailbox parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("action") != null)
    and (($schema.required // []) | index("mailbox") != null)
    and (($schema.properties | has("account_id")))
    and (($schema.properties | has("action")))
    and ($schema.properties.action.enum == ["create", "rename", "delete"])
    and (($schema.properties | has("mailbox")))
    and (($schema.properties | has("destination_mailbox")))
'

echo "Checking imap_list_accounts output contract"
LIST_ACCOUNTS_JSON=$(run_inspector --method tools/call --tool-name imap_list_accounts)
assert_json "imap_list_accounts output contract" "$LIST_ACCOUNTS_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and (($data.accounts | type) == "array")
    and (($data.accounts | length) > 0)
    and ($data.accounts[0].account_id == "default")
    and (($data.accounts[0].host | type) == "string")
    and (($data.accounts[0].port | type) == "number")
    and (($data.accounts[0].secure | type) == "boolean")
    and (($data.next_action.instruction | type) == "string")
    and ($data.next_action.tool == "imap_list_mailboxes")
    and ($data.next_action.arguments.account_id == "default")
'

echo "Checking imap_list_mailboxes output contract"
MAILBOXES_JSON=$(run_inspector --method tools/call --tool-name imap_list_mailboxes --tool-arg account_id=default)
assert_json "imap_list_mailboxes output contract" "$MAILBOXES_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and (($data.mailboxes | type) == "array")
    and ((($data.mailboxes // []) | map(.name)) | index("INBOX") != null)
    and (($data.mailboxes[0].name | type) == "string")
    and (($data.mailboxes[0].delimiter == null) or (($data.mailboxes[0].delimiter | type) == "string"))
    and (($data.next_action.instruction | type) == "string")
    and ($data.next_action.tool == "imap_search_messages")
    and ($data.next_action.arguments.account_id == "default")
    and (($data.next_action.arguments.mailbox | type) == "string")
    and ($data.next_action.arguments.limit == 10)
    and ($data.next_action.arguments.include_snippet == false)
'

echo "Checking imap_search_messages output contract"
SEARCH_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_search_messages \
  --tool-arg account_id=default \
  --tool-arg mailbox=INBOX \
  --tool-arg limit=2 \
  --tool-arg include_snippet=true \
  --tool-arg snippet_max_chars=120)
assert_json "imap_search_messages first page output contract" "$SEARCH_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and (($data.next_action.instruction | type) == "string")
    and ($data.next_action.tool == "imap_search_messages")
    and ($data.account_id == "default")
    and ($data.mailbox == "INBOX")
    and (($data.total | type) == "number")
    and (($data.attempted | type) == "number")
    and (($data.returned | type) == "number")
    and (($data.failed | type) == "number")
    and (($data.messages | type) == "array")
    and (($data.messages | length) == $data.returned)
    and (($data.messages | length) == 2)
    and ($data.has_more == true)
    and (($data.next_cursor | type) == "string")
    and ($data.next_action.arguments.account_id == "default")
    and ($data.next_action.arguments.mailbox == "INBOX")
    and ($data.next_action.arguments.cursor == $data.next_cursor)
    and ($data.next_action.arguments.limit == 2)
    and ($data.next_action.arguments.include_snippet == false)
    and (($data.messages[0].message_id | type) == "string")
    and (($data.messages[0].message_uri | type) == "string")
    and (($data.messages[0].message_raw_uri | type) == "string")
    and ($data.messages[0].mailbox == "INBOX")
    and (($data.messages[0].uidvalidity | type) == "number")
    and (($data.messages[0].uid | type) == "number")
    and (($data.messages[0].date | type) == "string")
    and (($data.messages[0].from | type) == "string")
    and (($data.messages[0].subject | type) == "string")
    and (($data.messages[0].flags | type) == "array")
    and (($data.messages[0].snippet | type) == "string")
'

MESSAGE_ID=$(printf '%s\n' "$SEARCH_JSON" | jq -r '(.structuredContent.data // .data).messages[0].message_id // empty')
if [[ -z "$MESSAGE_ID" ]]; then
  echo "Failed to capture search message id" >&2
  exit 1
fi

ATTACHMENT_SEARCH_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_search_messages \
  --tool-arg account_id=default \
  --tool-arg mailbox=INBOX \
  --tool-arg 'subject=Build Alert' \
  --tool-arg limit=1)
ATTACHMENT_MESSAGE_ID=$(printf '%s\n' "$ATTACHMENT_SEARCH_JSON" | jq -r '(.structuredContent.data // .data).messages[0].message_id // empty')
if [[ -z "$ATTACHMENT_MESSAGE_ID" ]]; then
  echo "No attachment-bearing message found for contract checks" >&2
  exit 1
fi

echo "Checking imap_get_message output contract"
GET_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_get_message \
  --tool-arg account_id=default \
  --tool-arg "message_id=${MESSAGE_ID}" \
  --tool-arg body_max_chars=500 \
  --tool-arg include_headers=true \
  --tool-arg include_all_headers=true \
  --tool-arg include_html=true)
assert_json "imap_get_message base output contract" "$GET_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and (($data.message.message_id | type) == "string")
    and (($data.message.message_uri | type) == "string")
    and (($data.message.message_raw_uri | type) == "string")
    and (($data.message.mailbox | type) == "string")
    and (($data.message.uidvalidity | type) == "number")
    and (($data.message.uid | type) == "number")
    and (($data.message.date | type) == "string")
    and (($data.message.from | type) == "string")
    and (($data.message.subject | type) == "string")
    and (($data.message.headers | type) == "array")
    and (($data.message.headers | length) > 0)
    and (($data.message.body_text | type) == "string")
    and (($data.message.body_html == null) or (($data.message.body_html | type) == "string"))
    and (($data.message.attachments | type) == "array")
'

GET_ATTACHMENT_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_get_message \
  --tool-arg account_id=default \
  --tool-arg "message_id=${ATTACHMENT_MESSAGE_ID}" \
  --tool-arg body_max_chars=500 \
  --tool-arg include_headers=true \
  --tool-arg extract_attachment_text=true \
  --tool-arg attachment_text_max_chars=1000)
assert_json "imap_get_message attachment output contract" "$GET_ATTACHMENT_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.message.attachments | type) == "array")
    and (($data.message.attachments | length) > 0)
    and (($data.message.attachments[0].filename | type) == "string")
    and (($data.message.attachments[0].content_type | type) == "string")
    and (($data.message.attachments[0].size_bytes | type) == "number")
    and (($data.message.attachments[0].part_id | type) == "string")
    and ($data.message.attachments[0] | has("extracted_text"))
'

echo "Checking imap_get_message_raw output contract"
RAW_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_get_message_raw \
  --tool-arg account_id=default \
  --tool-arg "message_id=${MESSAGE_ID}" \
  --tool-arg max_bytes=200000)
assert_json "imap_get_message_raw output contract" "$RAW_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and (($data.message_id | type) == "string")
    and (($data.message_uri | type) == "string")
    and (($data.message_raw_uri | type) == "string")
    and (($data.size_bytes | type) == "number")
    and (($data.raw_source_base64 | type) == "string")
    and (($data.raw_source_base64 | length) > 0)
    and ($data.raw_source_encoding == "base64")
'

echo "Checking imap_apply_to_messages output contracts"
APPLY_COPY_DRY_RUN_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg account_id=default \
  --tool-arg "selector={\"message_ids\":[\"${MESSAGE_ID}\"]}" \
  --tool-arg action=copy \
  --tool-arg destination_mailbox=Archive \
  --tool-arg dry_run=true)
assert_json "imap_apply_to_messages copy dry-run output contract" "$APPLY_COPY_DRY_RUN_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and ($data.action == "copy")
    and ($data.dry_run == true)
    and ($data.matched == 1)
    and ($data.attempted == 0)
    and (($data.results | type) == "array")
    and (($data.results | length) == 1)
    and ($data.results[0].status == "planned")
    and (($data.results[0].message_id | type) == "string")
    and (($data.results[0].issues | type) == "array")
    and (($data.results[0].source_mailbox | type) == "string")
    and ($data.results[0].destination_mailbox == "Archive")
    and ($data.results[0].destination_account_id == "default")
'

APPLY_UPDATE_FLAGS_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg account_id=default \
  --tool-arg "selector={\"message_ids\":[\"${MESSAGE_ID}\"]}" \
  --tool-arg action=update_flags \
  --tool-arg 'add_flags=["\\Seen"]')
assert_json "imap_apply_to_messages update_flags output contract" "$APPLY_UPDATE_FLAGS_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and (($data.status == "ok") or ($data.status == "partial"))
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and ($data.action == "update_flags")
    and ($data.dry_run == false)
    and ($data.matched == 1)
    and ($data.attempted == 1)
    and (($data.succeeded | type) == "number")
    and (($data.failed | type) == "number")
    and (($data.results | type) == "array")
    and (($data.results | length) == 1)
    and ((($data.results[0].status == "ok") or ($data.results[0].status == "partial")))
    and (($data.results[0].issues | type) == "array")
    and (($data.results[0].source_mailbox | type) == "string")
    and (($data.results[0].flags | type) == "array")
'

MAILBOX_BASE="Inspector MCP $(date +%s)"
MAILBOX_CREATE="${MAILBOX_BASE}/Child"
MAILBOX_RENAME="${MAILBOX_BASE}/Renamed"

echo "Checking imap_manage_mailbox output contracts"
MANAGE_CREATE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=create \
  --tool-arg "mailbox=${MAILBOX_CREATE}")
assert_json "imap_manage_mailbox create output contract" "$MANAGE_CREATE_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and ($data.action == "create")
    and ($data.mailbox == $mailbox)
    and (($data.destination_mailbox == null) or ($data.destination_mailbox == ""))
' --arg mailbox "$MAILBOX_CREATE"

MANAGE_RENAME_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=rename \
  --tool-arg "mailbox=${MAILBOX_CREATE}" \
  --tool-arg "destination_mailbox=${MAILBOX_RENAME}")
assert_json "imap_manage_mailbox rename output contract" "$MANAGE_RENAME_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and ($data.action == "rename")
    and ($data.mailbox == $mailbox)
    and ($data.destination_mailbox == $destination)
' --arg mailbox "$MAILBOX_CREATE" --arg destination "$MAILBOX_RENAME"

MANAGE_DELETE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=delete \
  --tool-arg "mailbox=${MAILBOX_RENAME}")
assert_json "imap_manage_mailbox delete output contract" "$MANAGE_DELETE_JSON" '
  def envelope_ok:
    (.isError != true)
    and ((.structuredContent.summary // .summary) | type == "string")
    and (((.structuredContent.meta // .meta).now_utc | type) == "string")
    and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
  (.structuredContent.data // .data) as $data
  | envelope_ok
    and ($data.status == "ok")
    and (($data.issues | type) == "array")
    and ($data.account_id == "default")
    and ($data.action == "delete")
    and ($data.mailbox == $mailbox)
    and (($data.destination_mailbox == null) or ($data.destination_mailbox == ""))
' --arg mailbox "$MAILBOX_RENAME"

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
