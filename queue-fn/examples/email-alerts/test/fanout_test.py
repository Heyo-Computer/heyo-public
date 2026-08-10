#!/usr/bin/env python3
"""Exercise fanout.py against a fake HeyoSecret and a fake SMTP server.

    python3 test/fanout_test.py

Standard library only, and no VM, no NATS, and no real mail server involved —
the point is that the routing table and the delivery/retry decisions can be
checked in a second, which is the part you will actually be editing. What it
cannot cover is the serial-console exec path and JetStream redelivery; those
need the real end-to-end run in the README.
"""
import base64, json, os, socket, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
EX = os.path.join(HERE, os.pardir, "guest", "fanout.py")

SMTP = {"host": "127.0.0.1", "port": None, "tls": "none",
        "from": "Alerts <alerts@example.com>", "subject_prefix": "[ALERT]",
        "connect_timeout_secs": 5}
ROUTING = {
    "version": 1,
    "rules": [
        {"name": "pager", "match": {"severity": ["critical", "page"]},
         "to": ["oncall@example.com", "pager@example.com"]},
        {"name": "prod-warnings", "match": {"severity": "warning", "labels": {"env": "prod"}},
         "to": ["sre@example.com", "ONCALL@example.com"]},
    ],
    "default": ["catchall@example.com"],
    "max_recipients": 25, "retry_undelivered": "never", "unrouted": "drop",
}

TOKEN = "test-token"
received = []          # (mailfrom, [rcpt], data)
smtp_behaviour = {"mode": "ok"}


# --- fake HeyoSecret --------------------------------------------------------
class SecretHandler(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def do_POST(self):
        if self.headers.get("authorization") != "Bearer " + TOKEN:
            self.send_response(401); self.end_headers(); self.wfile.write(b'{"error":"nope"}'); return
        n = int(self.headers["content-length"])
        req = json.loads(self.rfile.read(n))
        table = {"alerts/smtp/relay": SMTP, "alerts/routing/email-alerts": ROUTING}
        if req["path"] not in table:
            self.send_response(404); self.end_headers(); self.wfile.write(b'{"error":"missing"}'); return
        body = json.dumps({
            "path": req["path"], "version": 1, "status": "active",
            "valueBase64": base64.b64encode(json.dumps(table[req["path"]]).encode()).decode(),
            "createdAt": "2026-01-01T00:00:00Z", "metadata": {},
        }).encode()
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)


# --- fake SMTP --------------------------------------------------------------
def smtp_server(sock):
    while True:
        try:
            conn, _ = sock.accept()
        except OSError:
            return
        threading.Thread(target=smtp_session, args=(conn,), daemon=True).start()


def smtp_session(conn):
    f = conn.makefile("rwb")
    sent = 0
    try:
        f.write(b"220 fake ESMTP\r\n"); f.flush()
        if smtp_behaviour["mode"] == "drop_first_session":
            # One connection dies right after the greeting, then behave.
            smtp_behaviour["mode"] = "ok"
            conn.close(); return
        if smtp_behaviour["mode"] == "drop_always":
            conn.close(); return
        mailfrom, rcpts = None, []
        while True:
            line = f.readline()
            if not line: return
            cmd = line.decode("utf-8", "replace").strip()
            up = cmd.upper()
            if up.startswith("EHLO") or up.startswith("HELO"):
                f.write(b"250-fake\r\n250 SIZE 10240000\r\n")
            elif up.startswith("MAIL FROM"):
                mailfrom = cmd; f.write(b"250 OK\r\n")
            elif up.startswith("RCPT TO"):
                addr = cmd.split("<", 1)[1].split(">", 1)[0].lower()
                if smtp_behaviour["mode"] == "reject_one" and addr == "pager@example.com":
                    f.write(b"550 5.1.1 no such mailbox\r\n")
                elif smtp_behaviour["mode"] == "defer_one" and addr == "pager@example.com":
                    f.write(b"450 4.2.1 greylisted\r\n")
                else:
                    rcpts.append(addr); f.write(b"250 OK\r\n")
            elif up == "DATA":
                f.write(b"354 go\r\n"); f.flush()
                chunks = []
                while True:
                    l = f.readline()
                    if l in (b".\r\n", b".\n", b""): break
                    chunks.append(l)
                received.append((mailfrom, list(rcpts), b"".join(chunks)))
                rcpts = []
                sent += 1
                if smtp_behaviour["mode"] == "drop_after_one" and sent >= 1:
                    conn.close(); return
                f.write(b"250 queued\r\n")
            elif up == "QUIT":
                f.write(b"221 bye\r\n"); f.flush(); conn.close(); return
            elif up == "RSET":
                rcpts = []; f.write(b"250 OK\r\n")
            else:
                f.write(b"250 OK\r\n")
            f.flush()
    except (OSError, IndexError):
        pass
    finally:
        try: conn.close()
        except OSError: pass


def run(payload, extra_env=None, token=TOKEN):
    env = dict(os.environ)
    env.update({
        "QFN_PAYLOAD_B64": base64.b64encode(json.dumps(payload).encode()).decode() if payload is not None else "",
        "QFN_INVOCATION_ID": "0000018f1234-0000000a",
        "QFN_FUNCTION_ID": "email-alerts",
        "QFN_ATTEMPT": "1",
        "QFN_SOURCE": "invoke",
        "ALERT_SECRETS_TOKEN": token,
        "ALERT_SECRETS_URL": f"http://127.0.0.1:{SECRET_PORT}",
        "ALERT_DEADLINE_SECS": "12",
        "ALERT_CACHE_TTL_SECS": "0",
    })
    env.update(extra_env or {})
    p = subprocess.run([sys.executable, EX], capture_output=True, text=True, env=env, timeout=40)
    lines = [l for l in p.stdout.splitlines() if l.strip()]
    assert len(lines) == 1, f"expected exactly one output line, got {len(lines)}:\n{p.stdout}\n{p.stderr}"
    return p.returncode, json.loads(lines[0])


FAILS = []
def check(name, cond, detail=""):
    print(("  ok   " if cond else "  FAIL ") + name + (f"  {detail}" if not cond and detail else ""))
    if not cond: FAILS.append(name)


if __name__ == "__main__":
    secret_srv = ThreadingHTTPServer(("127.0.0.1", 0), SecretHandler)
    SECRET_PORT = secret_srv.server_address[1]
    threading.Thread(target=secret_srv.serve_forever, daemon=True).start()

    ssock = socket.socket(); ssock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ssock.bind(("127.0.0.1", 0)); ssock.listen(8)
    SMTP["port"] = ssock.getsockname()[1]
    threading.Thread(target=smtp_server, args=(ssock,), daemon=True).start()
    time.sleep(0.2)

    print("1. critical alert -> pager rule, two recipients")
    received.clear()
    code, out = run({"id": "disk-full-db01", "severity": "critical", "title": "Disk 94% on db-01",
                     "body": "line one\nline two", "source": "prometheus",
                     "labels": {"env": "prod", "service": "db"}, "url": "https://runbook"})
    check("exit 0", code == 0, f"got {code} / {out}")
    check("status sent", out["status"] == "sent", str(out.get("status")))
    check("2 delivered", len(out["delivered"]) == 2, str(out["delivered"]))
    check("2 messages received", len(received) == 2, str(len(received)))
    check("matched pager rule", out["matched_rules"] == ["pager"], str(out.get("matched_rules")))
    msg = received[0][2].decode()
    check("subject has severity", "[ALERT] CRITICAL: Disk 94% on db-01" in msg, msg[:200])
    check("deterministic message-id", "<qfn-0000018f1234-0000000a-" in msg)
    check("threading header", "In-Reply-To: <alert-" in msg)
    check("auto-submitted", "Auto-Submitted: auto-generated" in msg)
    check("body carried through", "line one" in msg and "line two" in msg)
    check("runbook in body", "https://runbook" in msg)

    print("2. same invocation id -> same Message-ID per recipient (dedupe on redelivery)")
    first_ids = sorted(l for l in received[0][2].decode().splitlines() if l.startswith("Message-ID"))
    received.clear()
    code, out = run({"id": "disk-full-db01", "severity": "critical", "title": "Disk 94% on db-01"},
                    extra_env={"QFN_ATTEMPT": "2"})
    second_ids = sorted(l for l in received[0][2].decode().splitlines() if l.startswith("Message-ID"))
    check("message-id stable across attempts", first_ids == second_ids, f"{first_ids} vs {second_ids}")

    print("3. warning+prod -> union of matching rules, deduped case-insensitively")
    received.clear()
    code, out = run({"severity": "warning", "title": "Latency up", "labels": {"env": "prod"}})
    check("exit 0", code == 0, str(code))
    check("2 recipients (deduped ONCALL/oncall)", out["resolved"] == 2, str(out.get("delivered")))

    print("4. unmatched severity -> default route")
    received.clear()
    code, out = run({"severity": "info", "title": "FYI"})
    check("default matched", out["matched_rules"] == ["default"], str(out.get("matched_rules")))
    check("catchall got it", out["delivered"] == ["catchall@example.com"], str(out["delivered"]))

    print("5. permanent 5xx for one recipient -> rejected, others still sent, exit 0")
    smtp_behaviour["mode"] = "reject_one"; received.clear()
    code, out = run({"severity": "critical", "title": "Partial reject"})
    check("exit 0", code == 0, str(code))
    check("status sent_with_rejections", out["status"] == "sent_with_rejections", out["status"])
    check("one rejected", list(out["rejected"]) == ["pager@example.com"], str(out["rejected"]))
    check("one delivered", out["delivered"] == ["oncall@example.com"], str(out["delivered"]))
    smtp_behaviour["mode"] = "ok"

    print("6. connection dies mid-fanout -> reconnect, everyone still gets it, no duplicates")
    smtp_behaviour["mode"] = "drop_first_session"; received.clear()
    t0 = time.monotonic()
    code, out = run({"severity": "critical", "title": "Mid-flight drop"})
    smtp_behaviour["mode"] = "ok"
    check("exit 0", code == 0, f"{code} {out.get('status')} {out.get('error','')}")
    check("both delivered", len(out["delivered"]) == 2, str(out))
    check("no duplicate sends", len(received) == 2, f"{len(received)} messages")

    print("6b. server that always drops mid-session -> backs off, does not spin")
    smtp_behaviour["mode"] = "drop_always"; received.clear()
    t0 = time.monotonic()
    code, out = run({"severity": "critical", "title": "Always drops"},
                    extra_env={"ALERT_DEADLINE_SECS": "8"})
    elapsed = time.monotonic() - t0
    smtp_behaviour["mode"] = "ok"
    sessions = sum(1 for e in out["log"] if e["event"] == "smtp_connected")
    check("exit 75", code == 75, str(code))
    check("finished inside the budget", elapsed < 14, f"{elapsed:.1f}s")
    check("backed off rather than spinning", sessions <= 6, f"{sessions} sessions in {elapsed:.1f}s")

    print("7. SMTP unreachable, nobody delivered -> 75 so JetStream retries cleanly")
    port_was = SMTP["port"]; SMTP["port"] = 9  # discard
    received.clear()
    code, out = run({"severity": "critical", "title": "Down"}, extra_env={"ALERT_DEADLINE_SECS": "5"})
    SMTP["port"] = port_was
    check("exit 75", code == 75, str(code))
    check("status no_delivery", out["status"] == "no_delivery", out["status"])
    check("nothing sent", received == [], str(received))

    print("8. malformed payloads -> 65, headed for the DLQ")
    for bad, label in [(None, "empty"), ({"nope": 1}, "no title/body")]:
        code, out = run(bad)
        check(f"exit 65 ({label})", code == 65, f"{code} {out.get('status')}")
        check(f"status bad_payload ({label})", out["status"] == "bad_payload", out["status"])
    env = dict(os.environ); env.update({
        "QFN_PAYLOAD_B64": "!!!not-base64!!!", "QFN_INVOCATION_ID": "x", "QFN_ATTEMPT": "1",
        "ALERT_SECRETS_TOKEN": TOKEN, "ALERT_SECRETS_URL": f"http://127.0.0.1:{SECRET_PORT}"})
    p = subprocess.run([sys.executable, EX], capture_output=True, text=True, env=env, timeout=20)
    check("exit 65 (bad base64)", p.returncode == 65, str(p.returncode))

    print("9. wrong secrets token -> 69, retry is clean")
    code, out = run({"severity": "critical", "title": "x"}, token="wrong")
    check("exit 69", code == 69, str(code))
    check("status secrets_error", out["status"] == "secrets_error", out["status"])
    check("no token in output", "wrong" not in json.dumps(out), json.dumps(out)[:200])

    print("10. no recipients at all -> exit 0 by default, 65 when unrouted=fail")
    saved_rules, saved_default = ROUTING["rules"], ROUTING["default"]
    ROUTING["rules"], ROUTING["default"] = [], []
    code, out = run({"severity": "info", "title": "Nowhere"})
    check("exit 0 (drop)", code == 0, str(code))
    check("status no_recipients", out["status"] == "no_recipients", out["status"])
    ROUTING["unrouted"] = "fail"
    code, out = run({"severity": "info", "title": "Nowhere"})
    check("exit 65 (fail)", code == 65, str(code))
    ROUTING["unrouted"] = "drop"; ROUTING["rules"], ROUTING["default"] = saved_rules, saved_default

    print("11. max_recipients caps the fan-out")
    ROUTING["rules"] = [{"name": "wide", "match": {},
                         "to": [f"u{i}@example.com" for i in range(10)]}]
    ROUTING["max_recipients"] = 3
    received.clear()
    code, out = run({"severity": "info", "title": "Wide"})
    check("capped at 3", out["resolved"] == 3, str(out["resolved"]))
    check("3 sent", len(received) == 3, str(len(received)))
    ROUTING["rules"], ROUTING["max_recipients"] = saved_rules, 25

    print("12. output is always a single line with no raw newlines")
    received.clear()
    code, out = run({"severity": "critical", "title": "a\nb\nc", "body": "x\ny\n" * 50})
    check("single line, parsed", isinstance(out, dict))
    check("log array present", isinstance(out.get("log"), list) and out["log"])
    check("duration recorded", isinstance(out.get("duration_ms"), int))

    print("13. secret cache: TTL>0 means one fetch across two runs")
    calls = {"n": 0}
    orig = SecretHandler.do_POST
    def counting(self):
        calls["n"] += 1
        return orig(self)
    SecretHandler.do_POST = counting
    try:
        os.path.exists("/dev/shm") and os.system("rm -f /dev/shm/email-alerts-secrets.json")
        run({"severity": "info", "title": "cache a"}, extra_env={"ALERT_CACHE_TTL_SECS": "60"})
        after_first = calls["n"]
        run({"severity": "info", "title": "cache b"}, extra_env={"ALERT_CACHE_TTL_SECS": "60"})
        check("second run hit the cache", calls["n"] == after_first,
              f"{after_first} then {calls['n']}")
        mode = oct(os.stat("/dev/shm/email-alerts-secrets.json").st_mode & 0o777)
        check("cache is 0600", mode == "0o600", mode)
    finally:
        SecretHandler.do_POST = orig
        os.system("rm -f /dev/shm/email-alerts-secrets.json")

    print()
    print("FAILURES:", FAILS if FAILS else "none")
    sys.exit(1 if FAILS else 0)
