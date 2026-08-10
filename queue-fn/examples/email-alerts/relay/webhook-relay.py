#!/usr/bin/env python3
"""Webhook ingress for the email-alerts function.

queue-fn's `POST /functions/:id/enqueue` takes a JSON *payload* and nothing
else. It does not forward request headers, so a provider's
`X-Hub-Signature-256` has nowhere to land, and it caps the payload at 4096
bytes because the value crosses a serial console as one command line. Neither
is a gap in queue-fn — an event bus should not be a webhook receiver — but it
does mean something has to sit in front. This is that something:

  provider  --HMAC-signed POST-->  relay  --compact envelope-->  queue-fn
                                     |
                                     +-- signing key from HeyoSecret

It does three jobs and no others:

1. **Verify.** Constant-time HMAC-SHA256 over the raw body, against a key read
   from HeyoSecret. An unsigned or wrongly-signed request never reaches the bus.
2. **Normalize.** Provider payloads are large and shaped however the provider
   felt; alerts are small and shaped one way. `normalize()` is the one function
   you edit per provider.
3. **Fan out.** One enqueue per alert, so a batch of five Alertmanager alerts
   becomes five invocations that scale independently — rather than one
   invocation that has to finish all five inside a 25-second exec budget.

Standard library only. Run it behind whatever already terminates TLS for you;
`http.server` is not an edge server and this binds to localhost by default.

    HEYOSECRET_TOKEN=… ./webhook-relay.py
"""

import hashlib
import hmac
import json
import os
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LISTEN_HOST = os.environ.get("RELAY_HOST", "127.0.0.1")
LISTEN_PORT = int(os.environ.get("RELAY_PORT", "9595"))
QFN_URL = os.environ.get("QFN_URL", "http://127.0.0.1:9494").rstrip("/")
QFN_FUNCTION = os.environ.get("QFN_FUNCTION", "email-alerts")
QFN_AUTH = os.environ.get("QFN_AUTH", "")  # `user:password` when QFN_INVOKE_AUTH is on
SECRETS_URL = os.environ.get("HEYOSECRET_URL", "http://127.0.0.1:4455").rstrip("/")
SECRETS_TOKEN = os.environ.get("HEYOSECRET_TOKEN", "")
SIGNING_KEY_PATH = os.environ.get("RELAY_SIGNING_KEY", "alerts/webhook/hmac")
SIGNATURE_HEADER = os.environ.get("RELAY_SIGNATURE_HEADER", "X-Signature-256")

# Refuse to read a body larger than this. queue-fn's ceiling applies to the
# *normalized envelope*, not to what the provider sent, but an unbounded read is
# how a webhook endpoint becomes a memory exhaustion primitive.
MAX_BODY_BYTES = 1_048_576

# queue-fn's hard payload ceiling. Enforced here as well as there, so an
# over-long alert body is truncated into a deliverable alert rather than
# rejected with a 413 that the provider will retry forever.
PAYLOAD_CEILING = 4096

# Labels that survive truncation first, because the routing table matches on
# them. Add yours here if a rule keys on a label this list does not name.
MAX_LABELS = int(os.environ.get("RELAY_MAX_LABELS", "12"))
KEEP_LABELS = [
    label.strip()
    for label in os.environ.get(
        "RELAY_KEEP_LABELS",
        "severity,env,environment,cluster,namespace,service,team,job,alertname,instance",
    ).split(",")
    if label.strip()
]

_key_cache = {"value": None, "fetched_at": 0.0}
KEY_TTL_SECS = float(os.environ.get("RELAY_KEY_TTL_SECS", "300"))


def log(**fields):
    fields["ts"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    sys.stderr.write(json.dumps(fields) + "\n")
    sys.stderr.flush()


def signing_key():
    """The HMAC key from HeyoSecret, cached briefly.

    Cached because a webhook endpoint gets hit far more often than the key
    rotates, and because a secret store that is briefly down should not turn
    into dropped alerts. The TTL bounds how long a rotated key keeps the old one
    working.
    """
    now = time.time()
    if _key_cache["value"] is not None and now - _key_cache["fetched_at"] < KEY_TTL_SECS:
        return _key_cache["value"]

    body = json.dumps({"path": SIGNING_KEY_PATH}).encode()
    req = urllib.request.Request(
        SECRETS_URL + "/v1/secrets/read",
        data=body,
        method="POST",
        headers={
            "content-type": "application/json",
            "authorization": "Bearer " + SECRETS_TOKEN,
        },
    )
    with urllib.request.urlopen(req, timeout=5) as resp:
        payload = json.load(resp)

    import base64

    key = base64.b64decode(payload["valueBase64"], validate=True)
    _key_cache.update(value=key, fetched_at=now)
    return key


def signature_ok(raw_body, header_value):
    """Constant-time comparison against `sha256=<hex>`.

    `compare_digest` rather than `==`: a byte-at-a-time comparison leaks the
    prefix length of a valid signature, and forging one signature is enough.
    """
    if not header_value:
        return False
    provided = header_value.strip()
    if provided.startswith("sha256="):
        provided = provided[len("sha256=") :]
    expected = hmac.new(signing_key(), raw_body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(provided, expected)


# --- Normalization ----------------------------------------------------------


def _truncate(value, limit):
    text = str(value)
    return text if len(text) <= limit else text[: limit - 1] + "…"


def _envelope(alert_id, severity, title, body, source, labels, url, ts):
    envelope = {
        "id": _truncate(alert_id, 128) if alert_id else None,
        "severity": _truncate(severity or "info", 32),
        "title": _truncate(title or "alert", 200),
        "source": _truncate(source or "webhook", 64),
        "ts": ts or int(time.time()),
    }
    if url:
        envelope["url"] = _truncate(url, 400)
    if labels:
        # Labels are the part that grows without bound — a Kubernetes alert can
        # carry thirty of them, and they have to fit inside a 4096-byte payload
        # alongside everything else. Trimming alphabetically would be arbitrary
        # in exactly the wrong way: the routing table matches on labels, so a
        # rule keyed on `team` would quietly stop matching the moment an alert
        # picked up enough labels sorting before it. Keep the ones routing is
        # likely to use first, then fill the rest in a stable order.
        trimmed = {}
        ordered = [k for k in KEEP_LABELS if k in labels]
        ordered += sorted(k for k in labels if k not in KEEP_LABELS)
        for key in ordered[:MAX_LABELS]:
            trimmed[_truncate(key, 40)] = _truncate(labels[key], 120)
        if len(labels) > MAX_LABELS:
            log(event="labels_trimmed", kept=len(trimmed), dropped=len(labels) - len(trimmed))
        envelope["labels"] = trimmed
    envelope = {k: v for k, v in envelope.items() if v is not None}

    # Body last, sized to whatever budget is left. The routing fields must
    # survive intact; the prose is what gets cut.
    if body:
        overhead = len(json.dumps(envelope).encode()) + len('"body":"",')
        envelope["body"] = _truncate(body, max(0, PAYLOAD_CEILING - overhead - 64))
    return envelope


def normalize(payload):
    """Provider payload -> list of alert envelopes.

    **This is the function you edit.** Two shapes are handled out of the box:
    Prometheus Alertmanager, and anything that already looks like an envelope.
    A batch becomes several envelopes, one per alert, because each one routes
    independently.
    """
    # Alertmanager: {"alerts": [{"status", "labels", "annotations", ...}]}
    if isinstance(payload, dict) and isinstance(payload.get("alerts"), list):
        envelopes = []
        for alert in payload["alerts"]:
            labels = dict(alert.get("labels") or {})
            annotations = alert.get("annotations") or {}
            resolved = alert.get("status") == "resolved"
            severity = "resolved" if resolved else labels.get("severity", "warning")
            title = annotations.get("summary") or labels.get("alertname") or "alert"
            if resolved:
                title = "RESOLVED: " + title
            envelopes.append(
                _envelope(
                    alert_id=alert.get("fingerprint") or labels.get("alertname"),
                    severity=severity,
                    title=title,
                    body=annotations.get("description"),
                    source="alertmanager",
                    labels=labels,
                    url=annotations.get("runbook_url") or alert.get("generatorURL"),
                    ts=None,
                )
            )
        return envelopes

    # Already an envelope (or close enough): pass it through the same sizing.
    if isinstance(payload, dict) and (payload.get("title") or payload.get("body")):
        return [
            _envelope(
                alert_id=payload.get("id"),
                severity=payload.get("severity"),
                title=payload.get("title"),
                body=payload.get("body"),
                source=payload.get("source"),
                labels=payload.get("labels"),
                url=payload.get("url"),
                ts=payload.get("ts"),
            )
        ]

    return []


# --- Enqueue ----------------------------------------------------------------


def enqueue(envelope):
    """Hand one alert to queue-fn. Async, so the provider is not held open.

    `enqueue` rather than `invoke`: a webhook sender wants a fast 2xx, and a
    synchronous invoke would block it through a cold start. The delivery outcome
    lands in the dashboard and, on repeated failure, in the DLQ.
    """
    body = json.dumps({"payload": envelope}).encode()
    req = urllib.request.Request(
        f"{QFN_URL}/functions/{QFN_FUNCTION}/enqueue",
        data=body,
        method="POST",
        headers={"content-type": "application/json"},
    )
    if QFN_AUTH:
        import base64

        req.add_header(
            "authorization",
            "Basic " + base64.b64encode(QFN_AUTH.encode()).decode(),
        )
    with urllib.request.urlopen(req, timeout=10) as resp:
        return json.load(resp).get("invocation_id")


class Handler(BaseHTTPRequestHandler):
    server_version = "queue-fn-webhook-relay"

    def _reply(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        pass  # structured logging only; the default writes to stderr unstructured

    def do_GET(self):
        if self.path == "/healthz":
            self._reply(200, {"status": "ok"})
        else:
            self._reply(404, {"error": "not found"})

    def do_POST(self):
        if self.path.rstrip("/") not in ("", "/webhook"):
            self._reply(404, {"error": "not found"})
            return

        length = int(self.headers.get("content-length") or 0)
        if length <= 0 or length > MAX_BODY_BYTES:
            self._reply(413, {"error": "body missing or too large"})
            return
        raw = self.rfile.read(length)

        try:
            if not signature_ok(raw, self.headers.get(SIGNATURE_HEADER)):
                log(event="signature_rejected", remote=self.client_address[0])
                self._reply(401, {"error": "bad signature"})
                return
        except (urllib.error.URLError, OSError, ValueError) as e:
            # The signing key is unreachable. Refusing is the only safe answer —
            # accepting unverified webhooks because the secret store is down is
            # how an outage becomes an incident.
            log(event="signing_key_unavailable", error=str(e))
            self._reply(503, {"error": "cannot verify signatures right now"})
            return

        try:
            payload = json.loads(raw)
        except ValueError:
            self._reply(400, {"error": "body is not JSON"})
            return

        envelopes = normalize(payload)
        if not envelopes:
            # A 200 on purpose: the provider did nothing wrong, and a 4xx would
            # put it into a retry loop over a payload shape we simply ignore.
            log(event="nothing_to_route")
            self._reply(200, {"enqueued": 0, "note": "no alerts in this payload"})
            return

        enqueued, failed = [], []
        for envelope in envelopes:
            try:
                enqueued.append(enqueue(envelope))
            except urllib.error.HTTPError as e:
                failed.append(f"HTTP {e.code}: {e.read().decode('utf-8', 'replace')[:120]}")
            except (urllib.error.URLError, OSError) as e:
                failed.append(str(e))

        log(event="relayed", enqueued=len(enqueued), failed=len(failed))
        if failed and not enqueued:
            # Nothing reached the bus, so let the provider retry the whole batch.
            self._reply(502, {"error": "queue-fn rejected every alert", "detail": failed[:3]})
        else:
            self._reply(202, {"enqueued": len(enqueued), "invocation_ids": enqueued,
                              "failed": failed})


def main():
    if not SECRETS_TOKEN:
        sys.exit("webhook-relay: set HEYOSECRET_TOKEN")
    server = ThreadingHTTPServer((LISTEN_HOST, LISTEN_PORT), Handler)
    log(event="listening", host=LISTEN_HOST, port=LISTEN_PORT,
        function=QFN_FUNCTION, signature_header=SIGNATURE_HEADER)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log(event="stopping")
        server.shutdown()


if __name__ == "__main__":
    main()
