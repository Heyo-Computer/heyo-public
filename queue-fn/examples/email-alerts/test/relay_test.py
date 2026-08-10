#!/usr/bin/env python3
"""Exercise webhook-relay.py against a fake HeyoSecret and a fake queue-fn.

    python3 test/relay_test.py

Covers the two things worth being sure of before this faces the internet:
signature verification refuses everything it should, and normalization fits a
real provider payload inside queue-fn's 4096-byte ceiling without losing the
labels the routing table matches on.
"""
import base64, hashlib, hmac, json, os, sys, threading, time, urllib.error, urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
RELAY = os.path.join(HERE, os.pardir, "relay", "webhook-relay.py")
KEY = b"\x01" * 32
TOKEN = "test-token"
enqueued = []
secret_available = {"ok": True}
qfn_behaviour = {"mode": "ok"}


class SecretHandler(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        if not secret_available["ok"]:
            self.send_response(503); self.end_headers(); self.wfile.write(b"{}"); return
        n = int(self.headers["content-length"]); json.loads(self.rfile.read(n))
        body = json.dumps({"path": "alerts/webhook/hmac", "version": 1, "status": "active",
                           "valueBase64": base64.b64encode(KEY).decode(),
                           "createdAt": "2026-01-01T00:00:00Z", "metadata": {}}).encode()
        self.send_response(200); self.send_header("content-length", str(len(body)))
        self.send_header("content-type", "application/json"); self.end_headers(); self.wfile.write(body)


class QfnHandler(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers["content-length"]); req = json.loads(self.rfile.read(n))
        if qfn_behaviour["mode"] == "reject":
            self.send_response(413); self.end_headers(); self.wfile.write(b'{"error":"too big"}'); return
        enqueued.append((self.path, req["payload"], self.headers.get("authorization")))
        body = json.dumps({"invocation_id": f"inv-{len(enqueued)}"}).encode()
        self.send_response(202); self.send_header("content-length", str(len(body)))
        self.send_header("content-type", "application/json"); self.end_headers(); self.wfile.write(body)


def post(body, sign=True, header="X-Signature-256", tamper=False):
    raw = json.dumps(body).encode() if not isinstance(body, bytes) else body
    req = urllib.request.Request(f"http://127.0.0.1:{RELAY_PORT}/webhook", data=raw,
                                 method="POST", headers={"content-type": "application/json"})
    if sign:
        sig = hmac.new(KEY, raw, hashlib.sha256).hexdigest()
        if tamper:
            sig = ("0" if sig[0] != "0" else "1") + sig[1:]
        req.add_header(header, "sha256=" + sig)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, json.load(resp)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


FAILS = []
def check(name, cond, detail=""):
    print(("  ok   " if cond else "  FAIL ") + name + (f"  {detail}" if not cond and detail else ""))
    if not cond: FAILS.append(name)


if __name__ == "__main__":
    s = ThreadingHTTPServer(("127.0.0.1", 0), SecretHandler)
    threading.Thread(target=s.serve_forever, daemon=True).start()
    q = ThreadingHTTPServer(("127.0.0.1", 0), QfnHandler)
    threading.Thread(target=q.serve_forever, daemon=True).start()

    import socket
    probe = socket.socket(); probe.bind(("127.0.0.1", 0)); RELAY_PORT = probe.getsockname()[1]; probe.close()

    env = dict(os.environ)
    env.update({"HEYOSECRET_TOKEN": TOKEN, "HEYOSECRET_URL": f"http://127.0.0.1:{s.server_address[1]}",
                "QFN_URL": f"http://127.0.0.1:{q.server_address[1]}", "RELAY_PORT": str(RELAY_PORT),
                "QFN_AUTH": "admin:hunter2", "RELAY_KEY_TTL_SECS": "0"})
    import subprocess
    proc = subprocess.Popen([sys.executable, RELAY], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    for _ in range(60):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{RELAY_PORT}/healthz", timeout=1); break
        except Exception:
            time.sleep(0.1)
    else:
        print(proc.stderr.read()); sys.exit("relay never came up")

    try:
        print("1. valid signature -> enqueued")
        enqueued.clear()
        code, out = post({"id": "a1", "severity": "critical", "title": "Disk full",
                          "body": "db-01 at 94%", "labels": {"env": "prod"}})
        check("202", code == 202, str(code))
        check("one enqueued", len(enqueued) == 1, str(enqueued))
        check("hits the right function", enqueued[0][0] == "/functions/email-alerts/enqueue", enqueued[0][0])
        check("basic auth forwarded", enqueued[0][2] == "Basic " + base64.b64encode(b"admin:hunter2").decode())
        check("envelope preserved", enqueued[0][1]["title"] == "Disk full", str(enqueued[0][1]))

        print("2. missing / wrong / tampered signature -> 401, nothing enqueued")
        enqueued.clear()
        for label, kw in [("unsigned", {"sign": False}), ("tampered", {"tamper": True}),
                          ("wrong header", {"header": "X-Other"})]:
            code, _ = post({"title": "x"}, **kw)
            check(f"401 ({label})", code == 401, str(code))
        check("nothing reached queue-fn", enqueued == [], str(enqueued))

        print("3. alertmanager batch -> one enqueue per alert")
        enqueued.clear()
        code, out = post({"alerts": [
            {"status": "firing", "fingerprint": "f1",
             "labels": {"alertname": "HighCPU", "severity": "critical", "env": "prod"},
             "annotations": {"summary": "CPU 98%", "description": "node-3 pinned",
                             "runbook_url": "https://rb/cpu"}},
            {"status": "resolved", "fingerprint": "f2",
             "labels": {"alertname": "DiskFull", "severity": "warning", "env": "prod"},
             "annotations": {"summary": "Disk recovered"}},
        ]})
        check("202", code == 202, str(code))
        check("two invocations", len(enqueued) == 2, str(len(enqueued)))
        check("severity mapped", enqueued[0][1]["severity"] == "critical", str(enqueued[0][1]))
        check("fingerprint is the alert id", enqueued[0][1]["id"] == "f1", str(enqueued[0][1]))
        check("runbook url carried", enqueued[0][1]["url"] == "https://rb/cpu", str(enqueued[0][1]))
        check("resolved marked", enqueued[1][1]["severity"] == "resolved", str(enqueued[1][1]))
        check("resolved in title", enqueued[1][1]["title"].startswith("RESOLVED:"), str(enqueued[1][1]))
        check("source tagged", enqueued[0][1]["source"] == "alertmanager", str(enqueued[0][1]))
        check("two invocation ids returned", len(out["invocation_ids"]) == 2, str(out))

        print("4. oversized provider payload -> normalized under queue-fn's 4096 ceiling")
        enqueued.clear()
        code, out = post({"alerts": [{"status": "firing", "fingerprint": "big",
                                      "labels": {**{f"label{i}": "v" * 300 for i in range(40)},
                                                 "severity": "critical", "env": "prod"},
                                      "annotations": {"summary": "S" * 500,
                                                      "description": "D" * 60000}}]})
        check("202", code == 202, str(code))
        size = len(json.dumps(enqueued[0][1]).encode())
        check("under 4096 bytes", size <= 4096, f"{size} bytes")
        check("routing fields survived", enqueued[0][1]["severity"] == "critical"
              and enqueued[0][1]["labels"].get("env") == "prod", str(enqueued[0][1])[:300])
        check("labels trimmed to 12", len(enqueued[0][1]["labels"]) <= 12,
              str(len(enqueued[0][1]["labels"])))
        check("body truncated not dropped", enqueued[0][1]["body"].startswith("DDD"))

        print("4b. routing-relevant labels survive trimming regardless of sort order")
        enqueued.clear()
        code, out = post({"alerts": [{"status": "firing", "fingerprint": "t",
                                      "labels": {**{f"aaa{i}": "v" for i in range(40)},
                                                 "team": "payments", "severity": "critical",
                                                 "env": "prod", "service": "checkout"},
                                      "annotations": {"summary": "s"}}]})
        got = enqueued[0][1]["labels"]
        check("team survived", got.get("team") == "payments", str(got))
        check("service survived", got.get("service") == "checkout", str(got))
        check("env survived", got.get("env") == "prod", str(got))

        print("5. secret store down -> 503, no unverified pass-through")
        enqueued.clear(); secret_available["ok"] = False
        code, out = post({"title": "x"})
        secret_available["ok"] = True
        check("503", code == 503, str(code))
        check("nothing enqueued", enqueued == [], str(enqueued))

        print("6. queue-fn rejects everything -> 502 so the provider retries")
        enqueued.clear(); qfn_behaviour["mode"] = "reject"
        code, out = post({"title": "x", "severity": "critical"})
        qfn_behaviour["mode"] = "ok"
        check("502", code == 502, str(code))

        print("7. payload with no alerts -> 200, provider is not put in a retry loop")
        code, out = post({"something": "else"})
        check("200", code == 200, str(code))
        check("enqueued 0", out.get("enqueued") == 0, str(out))

        print("8. body larger than the read cap -> 413 without reading it all")
        code, out = post(b'{"title":"' + b"x" * 2_000_000 + b'"}')
        check("413", code == 413, str(code))
    finally:
        proc.terminate(); proc.wait(timeout=10)

    print()
    print("FAILURES:", FAILS if FAILS else "none")
    sys.exit(1 if FAILS else 0)
