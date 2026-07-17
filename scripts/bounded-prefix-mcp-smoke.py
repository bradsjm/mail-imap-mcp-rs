#!/usr/bin/env python3
"""Exercise bounded-prefix message retrieval against a strict scripted TLS IMAP peer."""

import json
import os
import queue
import selectors
import shlex
import socket
import ssl
import subprocess
import sys
import threading
import time


USER = "bounded-prefix-user"
PASSWORD = "bounded-prefix-pass"
MESSAGE_ID = "imap:default:INBOX:123:42"
IO_TIMEOUT = 10.0
MAX_IMAP_LINE = 8192


def message_prefix():
    base = (
        b"Subject: Build Alert\r\n"
        b"MIME-Version: 1.0\r\n"
        b'Content-Type: multipart/mixed; boundary="b"\r\n'
        b"\r\n"
        b"--b\r\n"
        b"Content-Type: text/plain\r\n"
        b"\r\n"
        b"Build failed.\r\n"
        b"--b\r\n"
        b'Content-Type: text/plain; name="summary.txt"\r\n'
        b'Content-Disposition: attachment; filename="summary.txt"\r\n'
        b"\r\n"
    )
    if len(base) > 256:
        raise RuntimeError(f"fixture headers exceed fetch budget: {len(base)} bytes")
    return base + (b"x" * (256 - len(base)))


PREFIX = message_prefix()


def send_line(stream, line):
    stream.write(line.encode("ascii") + b"\r\n")
    stream.flush()


def read_command(stream):
    line = stream.readline(MAX_IMAP_LINE + 1)
    if not line:
        return None
    if len(line) > MAX_IMAP_LINE or not line.endswith(b"\n"):
        raise RuntimeError("IMAP command is overlong or unterminated")
    try:
        text = line.rstrip(b"\r\n").decode("ascii")
    except UnicodeDecodeError as exc:
        raise RuntimeError("IMAP command is not ASCII") from exc
    parts = text.split(" ", 1)
    if len(parts) != 2 or not parts[0] or not parts[1]:
        raise RuntimeError(f"malformed IMAP command: {text!r}")
    return parts[0], parts[1]


def reject_forbidden(command):
    upper = command.upper()
    if "BODYSTRUCTURE" in upper:
        raise RuntimeError(f"forbidden BODYSTRUCTURE request: {command}")
    if "BODY[" in upper or "BODY.PEEK[" in upper:
        expected = "UID FETCH 42 BODY.PEEK[]<0.256>"
        if upper != expected:
            raise RuntimeError(f"unexpected or unbounded body fetch: {command!r}; expected {expected!r}")


def require_login(command):
    try:
        fields = shlex.split(command)
    except ValueError as exc:
        raise RuntimeError(f"malformed LOGIN command: {command!r}") from exc
    if fields != ["LOGIN", USER, PASSWORD]:
        raise RuntimeError(f"unexpected LOGIN command: {command!r}")


def serve_imap(listener, context, failures, completed):
    try:
        listener.settimeout(IO_TIMEOUT)
        raw, _ = listener.accept()
        raw.settimeout(IO_TIMEOUT)
        with context.wrap_socket(raw, server_side=True) as connection:
            connection.settimeout(IO_TIMEOUT)
            stream = connection.makefile("rwb", buffering=0)
            send_line(stream, "* OK bounded-prefix fixture ready")
            stage = 0
            expected = (
                "LOGIN",
                'LIST "" "INBOX"',
                "EXAMINE INBOX",
                "UID FETCH 42 (UID RFC822.SIZE)",
                "UID FETCH 42 BODY.PEEK[]<0.256>",
                "UID FETCH 42 FLAGS",
            )
            while True:
                item = read_command(stream)
                if item is None:
                    if stage < len(expected):
                        raise RuntimeError(f"IMAP connection closed at stage {stage}, expected {expected[stage]!r}")
                    return
                tag, command = item
                reject_forbidden(command)
                upper = command.upper()
                if stage == 0:
                    require_login(command)
                    send_line(stream, f"{tag} OK LOGIN completed")
                    stage += 1
                elif stage == 1 and upper == expected[1]:
                    send_line(stream, '* LIST (\\HasNoChildren) "/" "INBOX"')
                    send_line(stream, f"{tag} OK LIST completed")
                    stage += 1
                elif stage == 2 and upper in ("EXAMINE INBOX", 'EXAMINE "INBOX"'):
                    send_line(stream, "* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)")
                    send_line(stream, "* 1 EXISTS")
                    send_line(stream, "* 0 RECENT")
                    send_line(stream, "* OK [UIDVALIDITY 123] stable identifiers")
                    send_line(stream, "* OK [UIDNEXT 43] next uid")
                    send_line(stream, f"{tag} OK [READ-ONLY] EXAMINE completed")
                    stage += 1
                elif stage == 3 and upper == expected[3]:
                    send_line(stream, "* 1 FETCH (UID 42 RFC822.SIZE 1024)")
                    send_line(stream, f"{tag} OK UID FETCH completed")
                    stage += 1
                elif stage == 4 and upper == expected[4]:
                    stream.write(b"* 1 FETCH (UID 42 BODY[]<0> {256}\r\n")
                    stream.write(PREFIX)
                    stream.write(b")\r\n")
                    send_line(stream, f"{tag} OK UID FETCH completed")
                    stage += 1
                elif stage == 5 and upper == expected[5]:
                    send_line(stream, "* 1 FETCH (UID 42 FLAGS ())")
                    send_line(stream, f"{tag} OK UID FETCH completed")
                    stage += 1
                    completed.set()
                elif stage == len(expected) and upper == "NOOP":
                    send_line(stream, f"{tag} OK NOOP completed")
                elif stage == len(expected) and upper == "LOGOUT":
                    send_line(stream, "* BYE logging out")
                    send_line(stream, f"{tag} OK LOGOUT completed")
                    return
                else:
                    wanted = expected[stage] if stage < len(expected) else "NOOP, LOGOUT, or close"
                    raise RuntimeError(f"unexpected IMAP command at stage {stage}: {command!r}; expected {wanted!r}")
    except Exception as exc:
        failures.put(exc)
    finally:
        listener.close()


def send_mcp(child, message):
    if child.stdin is None:
        raise RuntimeError("MCP stdin is unavailable")
    child.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    child.stdin.flush()


def receive_mcp(child, response_id, failures):
    if child.stdout is None:
        raise RuntimeError("MCP stdout is unavailable")
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + IO_TIMEOUT
    try:
        while True:
            if not failures.empty():
                raise failures.get()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(f"timed out waiting for MCP response {response_id}")
            if not selector.select(min(remaining, 0.1)):
                continue
            line = child.stdout.readline()
            if not line:
                raise RuntimeError(f"MCP server exited before response {response_id} (status {child.poll()})")
            try:
                response = json.loads(line)
            except json.JSONDecodeError as exc:
                raise RuntimeError(f"invalid JSON from MCP server: {line.rstrip()!r}") from exc
            if response.get("id") != response_id:
                continue
            if "error" in response:
                raise RuntimeError(f"MCP response {response_id} failed: {json.dumps(response['error'], separators=(',', ':'))}")
            if "result" not in response:
                raise RuntimeError(f"MCP response {response_id} has no result")
            return response["result"]
    finally:
        selector.close()


def stop_child(child, preserve_error):
    if child.stdin is not None:
        try:
            child.stdin.close()
        except (BrokenPipeError, OSError):
            pass
    try:
        status = child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        child.terminate()
        try:
            status = child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            status = child.wait(timeout=3)
    if not preserve_error and status != 0:
        raise RuntimeError(f"MCP server exited with status {status}")


def main():
    if len(sys.argv) != 4:
        raise RuntimeError("usage: bounded-prefix-mcp-smoke.py SERVER_BIN CERT_PATH KEY_PATH")
    server_bin, cert_path, key_path = map(os.path.abspath, sys.argv[1:])
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certfile=cert_path, keyfile=key_path)
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    port = listener.getsockname()[1]
    failures = queue.Queue()
    completed = threading.Event()
    thread = threading.Thread(
        target=serve_imap, args=(listener, context, failures, completed), daemon=True
    )
    thread.start()

    env = os.environ.copy()
    for key in tuple(env):
        if key.startswith("MAIL_IMAP_") and key.endswith("_HOST"):
            del env[key]
    env.update({
        "MAIL_IMAP_DEFAULT_HOST": "127.0.0.1",
        "MAIL_IMAP_DEFAULT_PORT": str(port),
        "MAIL_IMAP_DEFAULT_SECURE": "true",
        "MAIL_IMAP_DEFAULT_USER": USER,
        "MAIL_IMAP_DEFAULT_PASS": PASSWORD,
        "MAIL_IMAP_CA_CERT_PATH": cert_path,
        "MAIL_IMAP_MESSAGE_FETCH_BUDGET_BYTES": "256",
        "MAIL_IMAP_CONNECT_TIMEOUT_MS": "5000",
        "MAIL_IMAP_GREETING_TIMEOUT_MS": "5000",
        "MAIL_IMAP_SOCKET_TIMEOUT_MS": "5000",
        "MAIL_IMAP_READ_SESSION_CACHE_TTL_SECONDS": "1",
    })
    child = subprocess.Popen(
        [server_bin], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=None,
        text=True, encoding="utf-8", bufsize=1, env=env,
    )
    active_error = False
    try:
        send_mcp(child, {
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "bounded-prefix-smoke", "version": "1.0"}},
        })
        initialized = receive_mcp(child, 1, failures)
        if initialized.get("protocolVersion") != "2025-03-26":
            raise RuntimeError(f"unexpected MCP protocol version: {initialized.get('protocolVersion')!r}")
        send_mcp(child, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        send_mcp(child, {
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "imap_get_message", "arguments": {
                "message_id": MESSAGE_ID, "body_mode": "text", "body_max_chars": 500,
                "attachment_mode": "extract_text",
            }},
        })
        result = receive_mcp(child, 2, failures)
        if not isinstance(result, dict) or result.get("isError") is True:
            raise RuntimeError(f"imap_get_message failed: {json.dumps(result, separators=(',', ':'))}")
        if not completed.wait(2):
            if not failures.empty():
                raise failures.get()
            raise RuntimeError("scripted IMAP server did not observe the bounded FETCH")
        print(json.dumps(result, separators=(",", ":")))
    except Exception:
        active_error = True
        raise
    finally:
        stop_child(child, active_error)
        thread.join(2)
        if not active_error and thread.is_alive():
            raise RuntimeError("scripted IMAP server did not stop")
        if not active_error and not failures.empty():
            raise failures.get()


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(f"bounded-prefix MCP smoke failed: {exc}", file=sys.stderr)
        sys.exit(1)
