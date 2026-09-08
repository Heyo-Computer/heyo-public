#!/usr/bin/env python3
"""Offline contract test for the Python embedded in the service deploy workflow."""
import base64
import builtins
import io
import json
import os
import subprocess
import uuid
from pathlib import Path
from unittest.mock import patch

root = Path(__file__).resolve().parents[2]
workflow_path = root / ".heyo/workflows/deploy-heyo-services.yml"
workflow = workflow_path.read_text()
baseline_ref = os.environ.get("SERVICE_SPEC_BASELINE_REF")
old_workflow = (subprocess.check_output(
    ["git", "show", f"{baseline_ref}:.heyo/workflows/deploy-heyo-services.yml"],
    cwd=root, text=True,
) if baseline_ref else None)


def python_blocks(text):
    blocks = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if "python3 - <<'PY'" not in line:
            continue
        indent = len(line) - len(line.lstrip())
        body = []
        for candidate in lines[index + 1:]:
            if candidate == " " * indent + "PY":
                break
            body.append(candidate[indent:] if candidate.startswith(" " * indent) else candidate)
        else:
            raise AssertionError("unterminated Python heredoc")
        blocks.append("\n".join(body) + "\n")
    return blocks


def deploy_block(text):
    marker = "      - name: Deploy selected service through Heyo orchestrator"
    section = text[text.index(marker):]
    return next(block for block in python_blocks(section) if "def deploy(" in block)


sources = [("current", workflow)]
if old_workflow is not None:
    sources.append((baseline_ref, old_workflow))
for source_name, source in sources:
    blocks = python_blocks(source)
    assert blocks
    for number, block in enumerate(blocks, 1):
        compile(block, f"{source_name}:python-heredoc-{number}", "exec")

try:
    import yaml
except ModuleNotFoundError:
    subprocess.run(
        ["ruby", "-ryaml", "-e", "YAML.load_file(ARGV[0])", str(workflow_path)],
        cwd=root, text=True, check=True,
    )
else:
    for _, source in sources:
        yaml.safe_load(source)


class Result:
    returncode = 0
    stderr = ""

    def __init__(self, stdout=""):
        self.stdout = stdout


def execute(source, service, *, discovery=False, uploaded=False):
    captured = []
    real_open = builtins.open

    def fake_open(name, mode="r", *args, **kwargs):
        if str(name).startswith("dist/heyo-"):
            return io.BytesIO(b"archive-content")
        return real_open(name, mode, *args, **kwargs)

    def fake_check_output(command, **kwargs):
        assert command[:3] == ["git", "rev-parse", "HEAD"]
        return "a" * 40 + "\n"

    def fake_run(command, **kwargs):
        url = command[-1]
        body = kwargs.get("input")
        if url.endswith("/archives/presign"):
            return Result('{"archiveId":"archive-1","uploadUrl":"https://upload.invalid/one"}')
        if url.endswith("/archives/finalize"):
            return Result('{"archiveId":"archive-1"}')
        if url.endswith("/orchestration/services/deployments"):
            captured.append(json.loads(body))
            return Result('{"deploymentId":"captured","statusUrl":"/status/captured"}')
        if url.endswith("/status/captured"):
            return Result('{"status":"passed"}')
        if url == "https://upload.invalid/one":
            return Result()
        # Post-deployment HTTP health probes use urllib, and are stopped below.
        raise AssertionError(f"unexpected subprocess: {command}")

    env = {
        "TARGET_HEYO_SERVICE": service,
        "HEYO_PUBLIC_HOST": "stage.example.test",
        "ORCHESTRATOR_URL": "https://orchestrator.example.test",
        "ORCHESTRATOR_INTERNAL_API_KEY": "fake-key",
        "CI_REPO_URL": "git@github.com:example/repo.git",
        "CI_REF": "refs/heads/main",
        "CI_AFTER": "b" * 40,
        "GITHUB_REF": "refs/heads/main",
        "HEYO_TRAEFIK_CERT_RESOLVER": "test-resolver",
        "ORCHESTRATOR_BACKEND_API_URL": "https://backend.example.test",
    }
    if discovery:
        env.update({
            "ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES": service,
            "HEYO_SERVICE_REPLICAS": f"{service}=2",
            "HEYO_SERVICE_REPLICA_REGIONS": "EU,US",
            "HEYO_SERVICE_PLACEMENT_POOL": "pool-a",
        })
    size = 21 * 1024 * 1024 if uploaded else len(b"archive-content")
    with patch.dict(os.environ, env, clear=True), patch("builtins.open", fake_open), \
         patch("subprocess.check_output", fake_check_output), patch("subprocess.run", fake_run), \
         patch("os.path.getsize", return_value=size), patch("urllib.request.urlopen", side_effect=SystemExit("captured")):
        with patch("uuid.uuid4", return_value=uuid.UUID("12345678-1234-5678-1234-567812345678")):
            try:
                exec(compile(deploy_block(source), "embedded-public-deploy.py", "exec"), {"__name__": "__main__"})
            except SystemExit as error:
                assert str(error) == "captured", error
    assert len(captured) == 1
    return captured[0]


def old_to_canonical(old):
    route = old["route"]
    result = {
        "id": old["serviceId"], "user_id": old["userId"], "account_id": old["accountId"],
        "vm": {
            "driver": old["driver"], "image": old["image"], "port": old["ports"][0],
            "working_directory": old["workingDirectory"], "start_command": old["startCommand"],
            "size_class": old["sizeClass"], "ttl_seconds": old["ttlSeconds"],
            "env_vars": old["env"],
            "env_from": [{"secret": ref.split("heyosecret://", 1)[1].rsplit("/", 1)[0],
                          "key": ref.split("/", 3)[-1].removesuffix("@active"),
                          "as": ref.split("=", 1)[0]} for ref in old["envRefs"]],
        },
        "routes": [{"host": route["host"], "path_prefix": route["pathPrefix"],
                    "strip_prefix": route["stripPrefix"]}],
        "health": {"path": old["healthPath"], "timeout_secs": 5},
        "deploy": {
            "name": old["name"], "deployment_id": old["deploymentId"],
            "archive_name": old["archiveName"], "region": old["region"], "async": old["async"],
            "health_timeout_seconds": old["healthTimeoutSeconds"], "drain_seconds": old["drainSeconds"],
            "retire_previous": old["retirePrevious"], "retire_previous_async": old["retirePreviousAsync"],
            "delete_previous": old["deletePrevious"], "metadata": old["metadata"],
            "revision_guard": {"repository_url": old["revisionGuard"]["repositoryUrl"],
                               "ref": old["revisionGuard"]["ref"], "expected_sha": old["revisionGuard"]["expectedSha"],
                               "force": old["revisionGuard"]["force"]},
            "ingress": {"entry_points": route["entryPoints"], "priority": route["priority"],
                        "pass_host_header": route["passHostHeader"], "cert_resolver": route["certResolver"]},
        },
    }
    if "archiveBytesBase64" in old: result["deploy"]["archive_bytes_base64"] = old["archiveBytesBase64"]
    if "archiveId" in old: result["deploy"]["archive_id"] = old["archiveId"]
    if "placementPool" in old: result["deploy"]["placement_pool"] = old["placementPool"]
    if "replicaRegions" in old: result["deploy"]["replica_regions"] = old["replicaRegions"]
    if "desiredReplicas" in old:
        result["scaling"] = {"min_replicas": old["desiredReplicas"], "max_replicas": old["desiredReplicas"]}
    return result


fixture_dir = os.environ.get("SERVICE_SPEC_FIXTURE_DIR")
if fixture_dir:
    Path(fixture_dir).mkdir(parents=True, exist_ok=True)


def assert_invariants(service, payload):
    spec = json.loads((root / f".heyo/services/{service}.json").read_text())
    assert payload["id"] == spec["id"] == service
    assert payload["user_id"] == spec["user_id"]
    assert payload["account_id"] == spec["account_id"]
    for field in ("driver", "image", "port", "working_directory", "start_command",
                  "size_class", "ttl_seconds"):
        assert payload["vm"][field] == spec["vm"][field], (service, field)
    assert spec["vm"]["env_vars"].items() <= payload["vm"]["env_vars"].items()
    assert all(ref in payload["vm"]["env_from"] for ref in spec["vm"]["env_from"])
    assert payload["health"] == spec["health"]
    for field in ("name", "async", "region", "health_timeout_seconds", "drain_seconds",
                  "retire_previous", "retire_previous_async", "delete_previous"):
        assert payload["deploy"][field] == spec["deploy"][field], (service, field)


for service in ("heyosecret", "orchestrator", "app-obs"):
    new = execute(workflow, service)
    assert_invariants(service, new)
    if fixture_dir:
        (Path(fixture_dir) / f"{service}.json").write_text(json.dumps(new, indent=2, sort_keys=True) + "\n")
    if old_workflow is not None:
        old = execute(old_workflow, service)
        assert new == old_to_canonical(old), f"{service} canonical payload changed semantics"

# Exercise both branches that are easy to regress: discovery routing and direct archive upload.
new = execute(workflow, "orchestrator", discovery=True, uploaded=True)
assert new["scaling"] == {"min_replicas": 2, "max_replicas": 2}
assert new["deploy"]["replica_regions"] == ["EU", "US"]
assert new["deploy"]["placement_pool"] == "pool-a"
assert new["deploy"]["archive_id"] == "archive-1"
if old_workflow is not None:
    old = execute(old_workflow, "orchestrator", discovery=True, uploaded=True)
    assert new == old_to_canonical(old), "discovery/upload canonical payload changed semantics"
print(f"executed and validated 3 public deploy payloads plus discovery/upload variant{' against ' + baseline_ref if baseline_ref else ''}")
