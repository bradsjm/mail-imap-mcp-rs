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

GREENMAIL_HOST="${GREENMAIL_HOST:-127.0.0.1}"
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
        /-----END CERTIFICATE-----/ { capture=0 }
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

assert_operation_wrapper() {
  local description="$1"
  local json="$2"
  local kind="$3"

  assert_json "$description" "$json" '
    def envelope_ok:
      (.isError != true)
      and ((.structuredContent.summary // .summary) | type == "string")
      and (((.structuredContent.meta // .meta).now_utc | type) == "string")
      and (((.structuredContent.meta // .meta).duration_ms | type) == "number");
    (.structuredContent.data // .data) as $data
    | envelope_ok
      and (($data.status | type) == "string")
      and (($data.issues | type) == "array")
      and (($data.operation.operation_id | type) == "string")
      and ($data.operation.kind == $kind)
      and (($data.operation.state | type) == "string")
      and (($data.operation.done | type) == "boolean")
      and (($data.operation.cancel_supported | type) == "boolean")
      and ((($data.operation.created_at == null) or (($data.operation.created_at | type) == "string")))
      and ((($data.operation.started_at == null) or (($data.operation.started_at | type) == "string")))
      and ((($data.operation.finished_at == null) or (($data.operation.finished_at | type) == "string")))
      and (($data.operation.progress.total_units | type) == "number")
      and (($data.operation.progress.completed_units | type) == "number")
      and (($data.operation.progress.failed_units | type) == "number")
      and (($data.operation.progress.remaining_units | type) == "number")
      and ((($data.operation.progress.current_mailbox == null) or (($data.operation.progress.current_mailbox | type) == "string")))
      and (($data.operation.progress.phase | type) == "string")
      and (
        ($data.operation.done == true and (($data.result | type) == "object") and (($data | has("next_action")) | not))
        or
        ($data.operation.done == false and ($data.result == null) and ($data.next_action.tool == "imap_get_operation") and ($data.next_action.arguments.operation_id == $data.operation.operation_id))
      )
  ' --arg kind "$kind"
}

wait_for_terminal_operation_json() {
  local description="$1"
  local json="$2"
  local max_attempts="${3:-30}"
  local operation_id
  local kind
  local done

  operation_id=$(printf '%s\n' "$json" | jq -r '(.structuredContent.data // .data).operation.operation_id // empty')
  if [[ -z "$operation_id" ]]; then
    echo "Missing operation_id while waiting for ${description}" >&2
    printf '%s\n' "$json" >&2
    exit 1
  fi

  kind=$(printf '%s\n' "$json" | jq -r '(.structuredContent.data // .data).operation.kind // empty')
  if [[ -z "$kind" ]]; then
    echo "Missing operation kind while waiting for ${description}" >&2
    printf '%s\n' "$json" >&2
    exit 1
  fi

  assert_operation_wrapper "${description} operation wrapper" "$json" "$kind"
  done=$(printf '%s\n' "$json" | jq -r '(.structuredContent.data // .data).operation.done')
  if [[ "$done" == "true" ]]; then
    printf '%s\n' "$json"
    return 0
  fi

  local polled_json=""
  for _ in $(seq 1 "$max_attempts"); do
    sleep 1
    polled_json=$(run_inspector \
      --method tools/call \
      --tool-name imap_get_operation \
      --tool-arg "operation_id=${operation_id}")
    assert_operation_wrapper "${description} polled operation wrapper" "$polled_json" "$kind"
    done=$(printf '%s\n' "$polled_json" | jq -r '(.structuredContent.data // .data).operation.done')
    if [[ "$done" == "true" ]]; then
      printf '%s\n' "$polled_json"
      return 0
    fi
  done

  echo "Timed out waiting for terminal operation state for ${description}" >&2
  printf '%s\n' "$polled_json" >&2
  exit 1
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
      "imap_update_message_flags",
      "imap_manage_mailbox",
      "imap_get_operation",
      "imap_cancel_operation"
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
    and (($schema.properties | has("snippet_max_chars")))
    and ($schema.properties.snippet_max_chars.minimum == 50)
    and ($schema.properties.snippet_max_chars.maximum == 500)
    and (($schema.properties | has("include_snippet") | not))
'
assert_tool_schema "imap_get_message" "get_message parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("message_id") != null)
    and (($schema.properties | has("account_id") | not))
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
    and (($schema.properties | has("account_id") | not))
    and (($schema.properties | has("message_id")))
    and (($schema.properties | has("max_bytes")))
    and ($schema.properties.max_bytes.minimum == 1024)
    and ($schema.properties.max_bytes.maximum == 1000000)
'
assert_tool_schema "imap_apply_to_messages" "apply_to_messages parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("message_ids") != null)
    and (($schema.required // []) | index("action") != null)
    and (($schema.properties | has("account_id") | not))
    and (($schema.properties | has("message_ids")))
    and ($schema.properties.message_ids.minItems == 1)
    and ($schema.properties.message_ids.maxItems == 250)
    and ($schema.properties.message_ids.items.type == "string")
    and (($schema.properties | has("action")))
    and ($schema.properties.action.enum == ["move", "copy", "delete"])
    and (($schema.properties | has("destination_mailbox")))
    and (($schema.properties | has("dry_run") | not))
'
assert_tool_schema "imap_update_message_flags" "update_message_flags parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("message_ids") != null)
    and (($schema.required // []) | index("operation") != null)
    and (($schema.required // []) | index("flags") != null)
    and (($schema.properties | has("account_id") | not))
    and (($schema.properties | has("message_ids")))
    and ($schema.properties.message_ids.minItems == 1)
    and ($schema.properties.message_ids.maxItems == 250)
    and ($schema.properties.message_ids.items.type == "string")
    and (($schema.properties | has("operation")))
    and ($schema.properties.operation.enum == ["add", "remove", "replace"])
    and (($schema.properties | has("flags")))
    and ($schema.properties.flags.minItems == 1)
    and ($schema.properties.flags.maxItems == 32)
    and ($schema.properties.flags.items.type == "string")
    and (($schema.properties | has("dry_run") | not))
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
assert_tool_schema "imap_get_operation" "get_operation parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("operation_id") != null)
    and (($schema.properties | has("operation_id")))
'
assert_tool_schema "imap_cancel_operation" "cancel_operation parameter contract" '
  .tools[] | select(.name == $name) | .inputSchema as $schema
  | ($schema.type == "object")
    and (($schema.required // []) | index("operation_id") != null)
    and (($schema.properties | has("operation_id")))
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
    and (($data.next_action.arguments | has("include_snippet") | not))
    and (($data.next_action.arguments | has("snippet_max_chars") | not))
'

echo "Checking imap_search_messages output contract"
SEARCH_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_search_messages \
  --tool-arg account_id=default \
  --tool-arg mailbox=INBOX \
  --tool-arg limit=2 \
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
    and (($data.next_action.arguments | has("include_snippet") | not))
    and (($data.next_action.arguments | has("snippet_max_chars") | not))
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

MAILBOX_BASE="Inspector MCP $(date +%s)"
MAILBOX_COPY="${MAILBOX_BASE}/Copied"
MAILBOX_CREATE="${MAILBOX_BASE}/Child"
MAILBOX_RENAME="${MAILBOX_BASE}/Renamed"

echo "Checking imap_manage_mailbox output contracts"
MANAGE_COPY_CREATE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=create \
  --tool-arg "mailbox=${MAILBOX_COPY}")
MANAGE_COPY_CREATE_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_manage_mailbox create copy mailbox" "$MANAGE_COPY_CREATE_JSON")
assert_json "imap_manage_mailbox create copy mailbox terminal result contract" "$MANAGE_COPY_CREATE_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | ($result.status == "ok")
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.action == "create")
    and ($result.mailbox == $mailbox)
' --arg mailbox "$MAILBOX_COPY"

MANAGE_CREATE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=create \
  --tool-arg "mailbox=${MAILBOX_CREATE}")
MANAGE_CREATE_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_manage_mailbox create" "$MANAGE_CREATE_JSON")
assert_json "imap_manage_mailbox create terminal result contract" "$MANAGE_CREATE_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | ($result.status == "ok")
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.action == "create")
    and ($result.mailbox == $mailbox)
    and (($result.destination_mailbox == null) or ($result.destination_mailbox == ""))
' --arg mailbox "$MAILBOX_CREATE"

echo "Checking imap_apply_to_messages output contracts"
APPLY_COPY_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg "message_ids=[\"${MESSAGE_ID}\"]" \
  --tool-arg action=copy \
  --tool-arg "destination_mailbox=${MAILBOX_COPY}")
APPLY_COPY_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_apply_to_messages copy" "$APPLY_COPY_JSON")
assert_json "imap_apply_to_messages copy terminal result contract" "$APPLY_COPY_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | (($result.status == "ok") or ($result.status == "partial"))
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.action == "copy")
    and ($result.matched == 1)
    and ($result.attempted == 1)
    and ((($result.succeeded + $result.failed) == 1))
    and (($result.results | type) == "array")
    and (($result.results | length) == 1)
    and (($result.results[0].message_id | type) == "string")
    and (($result.results[0].issues | type) == "array")
    and (($result.results[0].source_mailbox | type) == "string")
    and ($result.results[0].destination_mailbox == $mailbox)
    and (($result.results[0].flags == null) or (($result.results[0].flags | type) == "array"))
' --arg mailbox "$MAILBOX_COPY"

COPIED_MESSAGE_SEARCH_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_search_messages \
  --tool-arg account_id=default \
  --tool-arg "mailbox=${MAILBOX_COPY}" \
  --tool-arg limit=1)
assert_json "imap_search_messages follow-up next_action for single copied message" "$COPIED_MESSAGE_SEARCH_JSON" '
  (.structuredContent.data // .data) as $data
  | ($data.status == "ok")
    and (($data.messages | type) == "array")
    and (($data.messages | length) == 1)
    and ($data.has_more == false)
    and ($data.next_action.tool == "imap_get_message")
    and ($data.next_action.arguments.message_id == $data.messages[0].message_id)
    and (($data.next_action.arguments | has("account_id")) | not)
'
COPIED_MESSAGE_ID=$(printf '%s\n' "$COPIED_MESSAGE_SEARCH_JSON" | jq -r '(.structuredContent.data // .data).messages[0].message_id // empty')
if [[ -z "$COPIED_MESSAGE_ID" ]]; then
  echo "Failed to capture copied message id from ${MAILBOX_COPY}" >&2
  printf '%s\n' "$COPIED_MESSAGE_SEARCH_JSON" >&2
  exit 1
fi

APPLY_DELETE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg "message_ids=[\"${COPIED_MESSAGE_ID}\"]" \
  --tool-arg action=delete)
APPLY_DELETE_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_apply_to_messages delete" "$APPLY_DELETE_JSON")
assert_json "imap_apply_to_messages delete terminal result contract" "$APPLY_DELETE_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | (($result.status == "ok") or ($result.status == "partial"))
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.action == "delete")
    and ($result.matched == 1)
    and ($result.attempted == 1)
    and ((($result.succeeded + $result.failed) == 1))
    and (($result.results | type) == "array")
    and (($result.results | length) == 1)
    and (($result.results[0].issues | type) == "array")
    and (($result.results[0].source_mailbox | type) == "string")
    and (($result.results[0].destination_mailbox == null) or ($result.results[0].destination_mailbox == ""))
'

echo "Checking imap_update_message_flags output contracts"
UPDATE_FLAGS_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_update_message_flags \
  --tool-arg "message_ids=[\"${MESSAGE_ID}\"]" \
  --tool-arg operation=add \
  --tool-arg 'flags=["\\Seen"]')
UPDATE_FLAGS_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_update_message_flags" "$UPDATE_FLAGS_JSON")
assert_json "imap_update_message_flags terminal result contract" "$UPDATE_FLAGS_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | (($result.status == "ok") or ($result.status == "partial"))
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.operation == "add")
    and ($result.matched == 1)
    and ($result.attempted == 1)
    and (($result.succeeded | type) == "number")
    and (($result.failed | type) == "number")
    and (($result.results | type) == "array")
    and (($result.results | length) == 1)
    and ((($result.results[0].status == "ok") or ($result.results[0].status == "partial") or ($result.results[0].status == "failed")))
    and (($result.results[0].issues | type) == "array")
    and (($result.results[0].source_mailbox | type) == "string")
    and (($result.results[0].flags == null) or (($result.results[0].flags | type) == "array"))
'

MANAGE_RENAME_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=rename \
  --tool-arg "mailbox=${MAILBOX_CREATE}" \
  --tool-arg "destination_mailbox=${MAILBOX_RENAME}")
MANAGE_RENAME_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_manage_mailbox rename" "$MANAGE_RENAME_JSON")
assert_json "imap_manage_mailbox rename terminal result contract" "$MANAGE_RENAME_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | ($result.status == "ok")
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.action == "rename")
    and ($result.mailbox == $mailbox)
    and ($result.destination_mailbox == $destination)
' --arg mailbox "$MAILBOX_CREATE" --arg destination "$MAILBOX_RENAME"

MANAGE_DELETE_JSON=$(run_inspector \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=delete \
  --tool-arg "mailbox=${MAILBOX_RENAME}")
MANAGE_DELETE_TERMINAL_JSON=$(wait_for_terminal_operation_json "imap_manage_mailbox delete" "$MANAGE_DELETE_JSON")
assert_json "imap_manage_mailbox delete terminal result contract" "$MANAGE_DELETE_TERMINAL_JSON" '
  (.structuredContent.data // .data).result as $result
  | (($result.status == "ok") or ($result.status == "failed"))
    and (($result.issues | type) == "array")
    and ($result.account_id == "default")
    and ($result.action == "delete")
    and ($result.mailbox == $mailbox)
    and (($result.destination_mailbox == null) or ($result.destination_mailbox == ""))
' --arg mailbox "$MAILBOX_RENAME"

echo "Checking write-path policy enforcement over MCP"
export MAIL_IMAP_WRITE_ENABLED="false"

expect_failure_with_text "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true" \
  --method tools/call \
  --tool-name imap_apply_to_messages \
  --tool-arg "message_ids=[\"${MESSAGE_ID}\"]" \
  --tool-arg action=delete

expect_failure_with_text "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true" \
  --method tools/call \
  --tool-name imap_update_message_flags \
  --tool-arg "message_ids=[\"${MESSAGE_ID}\"]" \
  --tool-arg operation=add \
  --tool-arg 'flags=["\\Seen"]'

expect_failure_with_text "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true" \
  --method tools/call \
  --tool-name imap_manage_mailbox \
  --tool-arg account_id=default \
  --tool-arg action=create \
  --tool-arg "mailbox=${MAILBOX_BASE}/Disabled"

echo "MCP inspector GreenMail integration checks passed"
