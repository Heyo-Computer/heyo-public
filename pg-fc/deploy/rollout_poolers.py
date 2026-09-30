#!/usr/bin/env python3
"""Release job: preflight all operator targets, then replace each pooler in order."""
import base64
import json
import os
from pathlib import Path
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

from replace_pooler import LIMIT, NoRedirect, executable, require, sha


def endpoint(url):
    parsed = urllib.parse.urlsplit(url)
    require(parsed.scheme == "https" and parsed.hostname and not parsed.username and not parsed.password
            and not parsed.query and not parsed.fragment, "operator endpoints must use HTTPS without credentials")
    return url.rstrip("/")


class Host:
    def __init__(self, target):
        self.target = target
        self.url = endpoint(target["url"])
        self.token = os.environ[target["token_env"]]
        require(self.token and re.fullmatch(r"[a-zA-Z0-9_-]+", target["name"]), "missing credential or invalid target name")
        self.http = urllib.request.build_opener(NoRedirect())

    def call(self, path, body=None):
        request = urllib.request.Request(self.url + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"})
        with self.http.open(request, timeout=30) as response:
            data = response.read(8 * 1024 * 1024 + 1)
        require(len(data) <= 8 * 1024 * 1024, "app-lb response too large")
        return json.loads(data)

    def run(self, request, preflight):
        envelope = {"target": self.target["pooler"], "request": request, "preflight": preflight}
        source = Path(__file__).with_name("replace_pooler.py").read_bytes()
        encoded = base64.b64encode(json.dumps(envelope).encode()).decode()
        command = "python3 -c \"import base64;exec(base64.b64decode('{}'))\" '{}'".format(base64.b64encode(source).decode(), encoded)
        # Unique launchers never overwrite a serving deployment or another release.
        identity = sha(json.dumps(envelope, sort_keys=True).encode() + source)
        name = "pooler-update-" + identity[:32]
        path = "/deployments/" + name
        spec = {"id": name, "namespace": self.target["namespace"], "maintenance": True,
                "routes": [{"host": name + ".invalid"}], "upstreams": ["127.0.0.1:1"],
                "health": {"path": None, "timeout_secs": 2},
                "update": {"working_dir": "/", "commands": [command], "timeout_secs": 900,
                           "verify_timeout_secs": 0, "env_from": self.target["env_from"]}}
        if not spec["update"]["env_from"]:
            del spec["update"]["env_from"]
        try:
            existing = self.call(path)["spec"]
        except urllib.error.HTTPError as error:
            # Namespace-confined callers cannot distinguish a missing name
            # from an inaccessible one. Registration checks authority again.
            if error.code not in (403, 404):
                raise
            self.call("/deployments", spec)
            existing = self.call(path)["spec"]
        existing.setdefault("namespace", "default")
        require(all(existing.get(key) == value for key, value in spec.items()), "managed launcher configuration differs")
        jobs = self.call(path + "/jobs")
        require(len(jobs) <= 1, "ambiguous launcher history; reconcile before retry")
        if not jobs:
            # Do not retry this POST if the reply is lost. A later invocation
            # reconciles the unique launcher's recorded job and host receipt.
            self.call(path + "/update", {})
        deadline = time.monotonic() + 960
        while time.monotonic() < deadline:
            jobs = self.call(path + "/jobs")
            require(len(jobs) == 1, "missing or ambiguous managed update job")
            job = jobs[0]
            require(job["deployment"] == name and job["kind"] == "host-update", "unexpected managed job identity")
            if job["status"] in ("queued", "pending", "running"):
                time.sleep(2)
                continue
            require(job["status"] == "succeeded", "managed pooler job failed; later targets were not updated")
            lines = [line for chunk in job["log"] for line in chunk.splitlines()]
            if preflight:
                require(lines.count("POOLER_PREFLIGHT_OK") == 1, "missing preflight receipt")
            else:
                receipts = [json.loads(line.split("=", 1)[1]) for line in lines if line.startswith("POOLER_REPLACEMENT=")]
                require(receipts == [{"status": "succeeded", "binary_sha256": request["binary_sha256"],
                                     "operation": request["operation"]}], "replacement receipt differs")
            return
        raise RuntimeError("managed update still unresolved; reconcile its recorded job before retry")


def rollout(hosts, request):
    for host in hosts:
        print("Preflight pooler " + host.target["name"], flush=True)
        host.run(request, True)
    for host in hosts:
        print("Replacing pooler " + host.target["name"], flush=True)
        host.run(request, False)
        print("Verified pooler " + host.target["name"], flush=True)


def main():
    config = json.loads(os.environ["PG_FC_ROLLOUT_TARGETS"])
    hosts = [Host(target) for target in config["targets"]]
    require(hosts and len({host.target["name"] for host in hosts}) == len(hosts), "missing or duplicate pooler targets")
    archive = Path(sys.argv[1])
    require(archive.stat().st_size <= LIMIT, "artifact too large")
    data = archive.read_bytes()
    revision = os.environ["PG_FC_REVISION"]
    binary = executable(data, revision)
    operation = "pooler-" + sha(os.environ["PG_FC_RUN_ID"].encode())
    request = {"operation": operation, "revision": revision, "artifact_sha256": sha(data),
               "binary_sha256": sha(binary), "artifact_url": endpoint(config["artifact_store"]) + "/blobs/" + sha(data)}
    rollout(hosts, request)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("pooler rollout stopped (" + type(error).__name__ + "); inspect the managed update job and protected host receipt", file=sys.stderr)
        sys.exit(1)
