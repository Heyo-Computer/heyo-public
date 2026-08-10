#!/usr/bin/env python3
"""Fan one alert out to the recipients its routing table names.

Runs inside the microVM as a queue-fn function. Baked into the image at
`/opt/email-alerts/fanout.py` — never fetched at cold start, because a function
that installs itself turns every scale-from-zero into a network install.

Python 3 standard library only, for the same reason: `smtplib`, `email`, and
`urllib` ship with the interpreter, so the image needs `python3` and nothing
else.

Configuration is *not* in the function spec. The spec carries exactly one
credential — a bearer token for HeyoSecret — and everything that matters (SMTP
host, password, who gets paged) is read from the secret store at invocation
time. A queue-fn spec is persisted to `queue-fn-state.json` and returned
verbatim by `GET /functions/:id`, so anything in it is readable by anyone who
can reach the admin API. One rotatable token there is a much smaller blast
radius than an SMTP password.

## Output is one line

Not because it has to be — the driver's serial reader collects every line
between its markers and concatenates them, so multi-line output survives. It is
one line because the whole result is one object: the invocation's outcome, the
per-recipient detail, and the diagnostics all belong together in the single
`stdout` string that queue-fn hands back and the dashboard shows in its
recent-invocations table. Everything this script would log goes into a `log`
array and is printed in a `finally`, so a crash still produces a readable
result. (`stderr` is always empty here regardless: the Firecracker exec path
runs `(cmd) 2>&1` and folds both streams into stdout.)

## Exit codes

queue-fn maps any non-zero exit to `Failed`, which naks the message; JetStream
redelivers it up to `retry.max_attempts` and then parks it in the DLQ. So the
only question this script has to answer is *would running again help?*

| code | status          | meaning                                              |
|------|-----------------|------------------------------------------------------|
| 0    | `sent`          | every routed recipient took the message              |
| 0    | `partial`       | some took it; retrying would duplicate those (default)|
| 0    | `no_recipients` | nothing matched and there is no default route        |
| 65   | `bad_payload`   | the envelope is not an alert; the DLQ is the right end|
| 69   | `secrets_error` | the secret store is unreachable — retry is clean      |
| 75   | `no_delivery`   | SMTP was transiently broken, *nobody* got mail        |
| 78   | `config_error`  | auth rejected, sender refused — a human must fix it   |

`partial` exiting zero is a choice, not an oversight: the alternative re-mails
everyone who already received the alert. Set `"retry_undelivered": "always"` in
the routing secret if you would rather have duplicates than a gap.

## Delivery is at-least-once

A redelivery that reaches a *different* VM has no memory of what the first
attempt sent. To make a duplicate recognisable rather than invisible, the
`Message-ID` is derived from the invocation id and the recipient, so the same
event re-sent to the same person carries the same id and most mail systems will
collapse it. A DLQ replay mints a fresh invocation id (queue-fn does this
deliberately), so a replay is a genuinely new message and threads with the
original rather than being suppressed as a duplicate.
"""

import base64
import binascii
import email.utils
import hashlib
import json
import os
import smtplib
import socket
import ssl
import sys
import time
import urllib.error
import urllib.request
from email.message import EmailMessage

EX_OK = 0
EX_BAD_PAYLOAD = 65
EX_SECRETS = 69
EX_TEMPFAIL = 75
EX_CONFIG = 78

# Leave the connection some room to finish inside the invocation's budget. Below
# this we stop starting new work rather than getting killed mid-send with no
# summary to show for it.
MIN_SEND_SLICE_SECS = 1.5

# Where the secret cache lives. tmpfs, so a decrypted value never reaches the
# VM's disk image.
CACHE_PATH = "/dev/shm/email-alerts-secrets.json"

LOG = []


def log(level, event, **fields):
    fields["level"] = level
    fields["event"] = event
    LOG.append(fields)


class Deadline:
    """Wall-clock budget for the whole invocation.

    Shorter than the function's `timeout_secs`, which is itself capped below the
    30s the daemon's serial exec path hard-codes. A process killed by that
    timeout prints nothing at all, so the budget exists to guarantee we get to
    the summary.
    """

    def __init__(self, secs):
        self.end = time.monotonic() + secs

    def remaining(self):
        return self.end - time.monotonic()

    def expired(self):
        return self.remaining() <= 0


class ConfigError(Exception):
    """Something a retry cannot fix: bad credentials, a refused sender."""


class SecretsError(Exception):
    """The secret store could not be reached or would not answer."""


# --- Reaching the host ------------------------------------------------------


def default_gateway():
    """The guest's default route, read from /proc/net/route.

    heyvm gives each VM its own tap subnet — the daemon derives it from the hex
    suffix of the replica name — so the host's address *as the guest sees it* is
    not a constant and cannot be hard-coded in the spec. The default gateway is
    the host end of this VM's tap pair, whatever subnet it landed in.

    Set `ALERT_SECRETS_URL` explicitly to bypass this entirely, which is what
    you want when the secret store is somewhere else on the network.
    """
    with open("/proc/net/route", "r") as fh:
        next(fh, None)  # header
        for line in fh:
            parts = line.split()
            if len(parts) > 2 and parts[1] == "00000000":
                # Little-endian hex, as the kernel writes it.
                packed = int(parts[2], 16)
                return "%d.%d.%d.%d" % (
                    packed & 0xFF,
                    (packed >> 8) & 0xFF,
                    (packed >> 16) & 0xFF,
                    (packed >> 24) & 0xFF,
                )
    raise SecretsError("the guest has no default route, so the host is unreachable")


# --- Secrets ----------------------------------------------------------------


class Secrets:
    """Reads from HeyoSecret's machine API, with a short tmpfs cache.

    The cache is per-VM and TTL'd. A VM that is warm through a burst of alerts
    would otherwise read the same two secrets once per alert; the cost is that a
    rotated secret takes up to the TTL to reach a running VM. Set
    `ALERT_CACHE_TTL_SECS=0` to turn it off and read every time.
    """

    def __init__(self, base_url, token, ttl_secs, deadline):
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.ttl = ttl_secs
        self.deadline = deadline
        self._cache = self._load_cache() if ttl_secs > 0 else {}

    def _load_cache(self):
        try:
            with open(CACHE_PATH, "r") as fh:
                return json.load(fh)
        except (OSError, ValueError):
            return {}

    def _save_cache(self):
        if self.ttl <= 0:
            return
        try:
            # 0600 from the moment it exists: os.open with the mode, not a
            # chmod after the fact, which would leave a window.
            fd = os.open(CACHE_PATH, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w") as fh:
                json.dump(self._cache, fh)
        except OSError as e:
            log("warn", "secret_cache_write_failed", error=str(e))

    def read_json(self, path):
        now = time.time()
        entry = self._cache.get(path)
        if entry and now - entry.get("fetched_at", 0) < self.ttl:
            log("debug", "secret_cache_hit", path=path)
            return entry["value"]

        value = self._fetch(path)
        self._cache[path] = {"fetched_at": now, "value": value}
        self._save_cache()
        return value

    def _fetch(self, path):
        body = json.dumps({"path": path}).encode("utf-8")
        req = urllib.request.Request(
            self.base_url + "/v1/secrets/read",
            data=body,
            method="POST",
            headers={
                "content-type": "application/json",
                "authorization": "Bearer " + self.token,
            },
        )
        # Never let a hung secret store eat the whole invocation budget.
        timeout = max(1.0, min(5.0, self.deadline.remaining() / 2))
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                payload = json.load(resp)
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:200]
            # 401/404 are as fatal on the tenth try as the first, but they still
            # exit non-zero so the failure is visible in the DLQ rather than
            # being swallowed into a successful-looking invocation.
            raise SecretsError(f"secret {path!r}: HTTP {e.code}: {detail}") from None
        except (urllib.error.URLError, socket.timeout, OSError) as e:
            raise SecretsError(f"secret {path!r}: {e}") from None

        try:
            raw = base64.b64decode(payload["valueBase64"], validate=True)
        except (KeyError, binascii.Error) as e:
            raise SecretsError(f"secret {path!r} is not valid base64: {e}") from None
        try:
            return json.loads(raw)
        except ValueError as e:
            raise SecretsError(f"secret {path!r} is not JSON: {e}") from None


# --- Routing ----------------------------------------------------------------


def _as_list(value):
    return value if isinstance(value, list) else [value]


def _matches(criteria, alert):
    """One rule's `match` block against the alert.

    An empty `match` matches everything, which is how you write a catch-all rule
    that still sits in the ordered list rather than in `default`.
    """
    severity = criteria.get("severity")
    if severity is not None and alert.get("severity") not in _as_list(severity):
        return False

    source = criteria.get("source")
    if source is not None and alert.get("source") not in _as_list(source):
        return False

    labels = alert.get("labels") or {}
    for key, want in (criteria.get("labels") or {}).items():
        if str(labels.get(key)) != str(want):
            return False

    return True


def resolve_recipients(alert, routing):
    """Every matching rule contributes; `default` is the fallback, not a floor.

    Union rather than first-match: a critical alert in production should reach
    both the severity rule's pager and the environment rule's team list, and
    expressing that with first-match means writing the cross product by hand.
    """
    recipients = []
    matched_rules = []

    for index, rule in enumerate(routing.get("rules") or []):
        if _matches(rule.get("match") or {}, alert):
            matched_rules.append(rule.get("name") or f"rule[{index}]")
            recipients.extend(_as_list(rule.get("to") or []))

    if not recipients:
        recipients = list(_as_list(routing.get("default") or []))
        if recipients:
            matched_rules.append("default")

    # Dedupe, keeping the order the rules produced so the log reads the way the
    # table does.
    seen = set()
    ordered = []
    for address in recipients:
        address = str(address).strip()
        key = address.lower()
        if address and key not in seen:
            seen.add(key)
            ordered.append(address)

    cap = int(routing.get("max_recipients") or 25)
    if len(ordered) > cap:
        # A routing mistake that fans out to a thousand addresses would blow the
        # invocation budget and look like an SMTP problem. Truncating and saying
        # so keeps the failure legible.
        log("warn", "recipients_truncated", resolved=len(ordered), cap=cap)
        ordered = ordered[:cap]

    return ordered, matched_rules


# --- Message ----------------------------------------------------------------


def _domain_of(from_header):
    _, address = email.utils.parseaddr(from_header)
    if "@" in address:
        return address.rsplit("@", 1)[1]
    return "queue-fn.local"


def build_message(alert, recipient, smtp, invocation_id, attempt, domain):
    msg = EmailMessage()
    msg["From"] = smtp.get("from") or smtp.get("username")
    msg["To"] = recipient
    msg["Date"] = email.utils.formatdate(localtime=False)

    severity = str(alert.get("severity") or "info").upper()
    title = str(alert.get("title") or "alert")
    prefix = smtp.get("subject_prefix", "[ALERT]")
    msg["Subject"] = f"{prefix} {severity}: {title}"[:200]

    # Deterministic, so a JetStream redelivery of the same event produces a
    # byte-identical id and the receiving system can collapse it. A DLQ replay
    # gets a fresh invocation id from queue-fn, so it is deliberately *not*
    # suppressed.
    digest = hashlib.sha256(recipient.lower().encode("utf-8")).hexdigest()[:12]
    msg["Message-ID"] = f"<qfn-{invocation_id}-{digest}@{domain}>"

    # Thread every alert that carries the same id, so a flapping check collapses
    # into one conversation instead of forty unrelated messages.
    alert_id = alert.get("id")
    if alert_id:
        thread = f"<alert-{hashlib.sha256(str(alert_id).encode()).hexdigest()[:16]}@{domain}>"
        msg["References"] = thread
        msg["In-Reply-To"] = thread

    msg["X-QFN-Invocation"] = invocation_id
    msg["X-QFN-Attempt"] = str(attempt)
    msg["X-Alert-Severity"] = str(alert.get("severity") or "info")
    if alert_id:
        msg["X-Alert-Id"] = str(alert_id)
    if alert.get("source"):
        msg["X-Alert-Source"] = str(alert["source"])
    # Precedence and Auto-Submitted keep alert mail out of vacation responders
    # and out of other people's auto-reply loops.
    msg["Auto-Submitted"] = "auto-generated"
    msg["Precedence"] = "bulk"

    msg.set_content(render_body(alert))
    return msg


def render_body(alert):
    lines = [str(alert.get("title") or "alert"), ""]
    body = alert.get("body")
    if body:
        lines += [str(body), ""]

    facts = [
        ("Severity", alert.get("severity")),
        ("Source", alert.get("source")),
        ("Alert ID", alert.get("id")),
    ]
    timestamp = alert.get("ts")
    if timestamp:
        try:
            facts.append(
                ("Time", time.strftime("%Y-%m-%d %H:%M:%SZ", time.gmtime(float(timestamp))))
            )
        except (TypeError, ValueError):
            facts.append(("Time", str(timestamp)))

    for label, value in facts:
        if value not in (None, ""):
            lines.append(f"{label}: {value}")

    labels = alert.get("labels") or {}
    if labels:
        lines.append("")
        lines.append("Labels:")
        for key in sorted(labels):
            lines.append(f"  {key}: {labels[key]}")

    if alert.get("url"):
        lines += ["", f"Runbook / details: {alert['url']}"]

    lines += ["", "-- ", f"queue-fn {os.environ.get('QFN_FUNCTION_ID', 'email-alerts')}"]
    return "\n".join(lines) + "\n"


# --- SMTP -------------------------------------------------------------------


def connect(smtp, deadline):
    host = smtp.get("host")
    if not host:
        raise ConfigError("the SMTP secret has no `host`")
    mode = str(smtp.get("tls") or ("starttls" if smtp.get("starttls", True) else "none")).lower()
    port = int(smtp.get("port") or (465 if mode == "implicit" else 587))
    timeout = max(1.0, min(float(smtp.get("connect_timeout_secs") or 8), deadline.remaining()))

    if mode == "implicit":
        conn = smtplib.SMTP_SSL(host, port, timeout=timeout, context=ssl.create_default_context())
    else:
        conn = smtplib.SMTP(host, port, timeout=timeout)
        if mode == "starttls":
            conn.ehlo()
            conn.starttls(context=ssl.create_default_context())
            conn.ehlo()

    username = smtp.get("username")
    password = smtp.get("password")
    if username and password:
        conn.login(username, password)

    log("info", "smtp_connected", host=host, port=port, tls=mode)
    return conn


def _refusal_is_permanent(refused):
    """A 5xx for every address means the server will say the same tomorrow."""
    for code, _ in refused.values():
        if 400 <= code < 500:
            return False
    return True


def send_all(recipients, alert, smtp, deadline, invocation_id, attempt):
    """Deliver to each recipient, reconnecting around transient breakage.

    One message per recipient rather than one message with many `To:` addresses.
    It costs an extra `DATA` round trip each, and it buys the thing that makes
    retries safe: a per-recipient outcome. A single multi-recipient send that
    dies halfway leaves no record of who got it.
    """
    domain = _domain_of(smtp.get("from") or smtp.get("username") or "")
    pending = list(recipients)
    delivered = []
    rejected = {}
    last_transient = None
    backoff = 0.5

    while pending and deadline.remaining() > MIN_SEND_SLICE_SECS:
        outstanding = len(pending)
        conn = None
        try:
            conn = connect(smtp, deadline)
        except (smtplib.SMTPAuthenticationError, smtplib.SMTPNotSupportedError) as e:
            raise ConfigError(f"SMTP rejected our credentials or TLS: {e}") from None
        except (smtplib.SMTPException, socket.timeout, OSError, ssl.SSLError) as e:
            last_transient = f"connect: {e}"
            log("warn", "smtp_connect_failed", error=str(e))

        if conn is not None:
            try:
                for recipient in list(pending):
                    if deadline.remaining() <= MIN_SEND_SLICE_SECS:
                        log("warn", "deadline_reached", undelivered=len(pending))
                        break
                    message = build_message(alert, recipient, smtp, invocation_id, attempt, domain)
                    try:
                        conn.send_message(message)
                    except smtplib.SMTPRecipientsRefused as e:
                        if _refusal_is_permanent(e.recipients):
                            code, detail = next(iter(e.recipients.values()))
                            rejected[recipient] = (
                                f"{code} {detail.decode('utf-8', 'replace')[:120]}"
                            )
                            pending.remove(recipient)
                            log("warn", "recipient_rejected", to=recipient,
                                detail=rejected[recipient])
                        else:
                            last_transient = f"{recipient}: greylisted or deferred"
                            log("warn", "recipient_deferred", to=recipient)
                    except smtplib.SMTPSenderRefused as e:
                        raise ConfigError(f"the server refused our sender address: {e}") from None
                    except (smtplib.SMTPServerDisconnected, socket.timeout, OSError,
                            ssl.SSLError) as e:
                        # The connection is gone; everything still pending needs
                        # a fresh one. Break to the reconnect rather than
                        # hammering a dead socket once per recipient.
                        last_transient = f"{recipient}: {e}"
                        log("warn", "smtp_disconnected", to=recipient, error=str(e))
                        break
                    else:
                        delivered.append(recipient)
                        pending.remove(recipient)
                        log("info", "delivered", to=recipient)
            finally:
                try:
                    conn.quit()
                except (smtplib.SMTPException, OSError):
                    conn.close()

        if not pending:
            break
        if len(pending) < outstanding:
            # The session accomplished something, so the next failure is a new
            # one rather than a continuing outage. Start the ladder over.
            backoff = 0.5
            continue

        # Nothing moved. A server that accepts a connection and then drops it
        # mid-send would otherwise be reconnected to as fast as the loop can
        # spin, for the whole budget — the backoff has to cover the session
        # failing, not just the connect failing.
        wait = min(backoff, deadline.remaining() - MIN_SEND_SLICE_SECS)
        if wait <= 0:
            break
        time.sleep(wait)
        backoff = min(backoff * 2, 4.0)

    return delivered, pending, rejected, last_transient


# --- Entry point ------------------------------------------------------------


def load_alert():
    encoded = os.environ.get("QFN_PAYLOAD_B64", "")
    if not encoded:
        raise ValueError("no payload: this function needs an alert envelope to route")
    try:
        raw = base64.b64decode(encoded, validate=True)
    except binascii.Error as e:
        raise ValueError(f"QFN_PAYLOAD_B64 is not valid base64: {e}") from None
    try:
        alert = json.loads(raw)
    except ValueError as e:
        raise ValueError(f"the payload is not JSON: {e}") from None
    if not isinstance(alert, dict):
        raise ValueError("the payload must be a JSON object")
    if not alert.get("title") and not alert.get("body"):
        raise ValueError("the alert has neither `title` nor `body`, so there is nothing to send")
    return alert


def secrets_base_url():
    explicit = os.environ.get("ALERT_SECRETS_URL", "").strip()
    if explicit:
        return explicit
    port = os.environ.get("ALERT_SECRETS_PORT", "4455").strip() or "4455"
    return f"http://{default_gateway()}:{port}"


def main():
    started = time.monotonic()
    deadline = Deadline(float(os.environ.get("ALERT_DEADLINE_SECS", "20")))
    invocation_id = os.environ.get("QFN_INVOCATION_ID", "unknown")
    attempt = int(os.environ.get("QFN_ATTEMPT", "1"))

    summary = {
        "invocation_id": invocation_id,
        "attempt": attempt,
        "source": os.environ.get("QFN_SOURCE", "invoke"),
        "status": "error",
        "delivered": [],
        "undelivered": [],
        "rejected": {},
    }
    code = EX_TEMPFAIL

    try:
        alert = load_alert()
        summary["alert_id"] = alert.get("id")
        summary["severity"] = alert.get("severity")

        token = os.environ.get("ALERT_SECRETS_TOKEN", "").strip()
        if not token:
            raise ConfigError("ALERT_SECRETS_TOKEN is unset; the spec must supply it")

        secrets = Secrets(
            secrets_base_url(),
            token,
            float(os.environ.get("ALERT_CACHE_TTL_SECS", "60")),
            deadline,
        )
        smtp = secrets.read_json(os.environ.get("ALERT_SMTP_SECRET", "alerts/smtp/relay"))
        routing = secrets.read_json(
            os.environ.get("ALERT_ROUTING_SECRET", "alerts/routing/email-alerts")
        )

        recipients, matched = resolve_recipients(alert, routing)
        summary["matched_rules"] = matched
        summary["resolved"] = len(recipients)

        if not recipients:
            # Retrying cannot invent a route, so DLQ'ing this would fill the DLQ
            # with alerts that will never be deliverable. Dropping is the
            # default and it is loud; `"unrouted": "fail"` sends it to the DLQ
            # instead, which is the right choice if a missed alert is worse than
            # a noisy queue.
            summary["status"] = "no_recipients"
            log("error", "no_recipients", severity=alert.get("severity"))
            if str(routing.get("unrouted") or "drop").lower() == "fail":
                return EX_BAD_PAYLOAD, summary
            return EX_OK, summary

        delivered, pending, rejected, transient = send_all(
            recipients, alert, smtp, deadline, invocation_id, attempt
        )
        summary["delivered"] = delivered
        summary["undelivered"] = pending
        summary["rejected"] = rejected
        if transient:
            summary["last_transient_error"] = transient

        if not pending:
            summary["status"] = "sent" if not rejected else "sent_with_rejections"
            code = EX_OK
        elif not delivered:
            # Nobody got mail, so a redelivery is clean — no duplicates possible.
            summary["status"] = "no_delivery"
            code = EX_TEMPFAIL
        elif str(routing.get("retry_undelivered") or "never").lower() == "always":
            summary["status"] = "partial_retry"
            code = EX_TEMPFAIL
        else:
            # Some were delivered. Retrying re-mails them, so we stop here and
            # make the gap visible instead.
            summary["status"] = "partial"
            code = EX_OK

    except ValueError as e:
        summary["status"] = "bad_payload"
        summary["error"] = str(e)
        code = EX_BAD_PAYLOAD
    except SecretsError as e:
        summary["status"] = "secrets_error"
        summary["error"] = str(e)
        code = EX_SECRETS
    except ConfigError as e:
        summary["status"] = "config_error"
        summary["error"] = str(e)
        code = EX_CONFIG
    except Exception as e:  # noqa: BLE001 - a crash with no summary is unreadable
        summary["status"] = "error"
        summary["error"] = f"{type(e).__name__}: {e}"
        code = EX_TEMPFAIL

    summary["duration_ms"] = int((time.monotonic() - started) * 1000)
    return code, summary


if __name__ == "__main__":
    exit_code, result = main()
    result["log"] = LOG
    # One line, no newlines inside it: `json.dumps` escapes them and the default
    # separators add none. Multi-line output across a serial console that frames
    # on `\n` is not something to bet an alert on.
    sys.stdout.write(json.dumps(result, default=str) + "\n")
    sys.stdout.flush()
    sys.exit(exit_code)
