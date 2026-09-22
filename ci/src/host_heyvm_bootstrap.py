"""Fail-closed, replay-safe installer for the dedicated host heyvm service.

This file is embedded by CI and run by app-lb's existing host-update launcher.
It intentionally uses only the Python standard library.
"""
import base64
import hashlib
import io
import json
import os
import pathlib
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

MAX_ARTIFACT = 512 * 1024 * 1024
MAX_HEYVM = 256 * 1024 * 1024
MAX_SMALL_BACKUP = 1024 * 1024
REPOSITORY = "https://github.com/Heyo-Computer/heyo.git"
TARGET_FIELDS = {"repository", "app_lb_admin_url", "app_lb_deployment", "app_lb_namespace",
                 "runner_hd_id", "backend_server_id", "executable", "unit", "state_dir",
                 "config_json_path", "systemd_drop_in_path", "local_health_url", "target_alias", "region"}
DAEMON_TARGET_FIELDS = TARGET_FIELDS | {"process_manager"}
REQUEST_FIELDS = {"operation_id", "artifact_url", "artifact_sha256", "artifact_size", "inner_path",
                  "inner_archive_sha256", "heyvm_sha256"}
DAEMON_REQUEST_FIELDS = REQUEST_FIELDS | {"component"}
TERMINAL = {"succeeded", "rolled_back", "rollback_failed"}


def sha(data): return hashlib.sha256(data).hexdigest()
def valid_sha(value): return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def closed(value, fields, what):
    if not isinstance(value, dict) or set(value) != fields:
        raise ValueError(f"{what} fields differ from closed schema")


def endpoint(value, local=False):
    parsed = urllib.parse.urlsplit(value)
    if parsed.username or parsed.password or parsed.query or parsed.fragment or not parsed.hostname:
        raise ValueError("unsafe URL")
    if local:
        if parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1", "localhost"):
            raise ValueError("health URL must be loopback HTTP")
    elif parsed.scheme != "https":
        raise ValueError("URL must use HTTPS")


def safe_path(value, directory=False):
    p = pathlib.PurePosixPath(value)
    if not p.is_absolute() or ".." in p.parts or value in ("/", "") or "\x00" in value:
        raise ValueError("unsafe absolute path")
    if not directory and value.endswith("/"):
        raise ValueError("file path ends in slash")
    return value


def mapping(raw, alias):
    values = json.loads(raw)
    if not isinstance(values, dict) or alias not in values:
        raise ValueError("unknown target alias")
    target = values[alias]
    if not isinstance(target, dict) or set(target) not in (TARGET_FIELDS, DAEMON_TARGET_FIELDS):
        raise ValueError("target fields differ from closed schema")
    if target["target_alias"] != alias or target["repository"] != REPOSITORY:
        raise ValueError("target identity mismatch")
    endpoint(target["app_lb_admin_url"])
    endpoint(target["local_health_url"], True)
    for field in ("executable", "state_dir", "config_json_path", "systemd_drop_in_path"):
        safe_path(target[field], field == "state_dir")
    manager = target.get("process_manager", "systemd")
    if manager not in ("systemd", "supervisor"): raise ValueError("invalid process manager")
    if manager == "systemd" and not re.fullmatch(r"[A-Za-z0-9_.@-]+\.service", target["unit"]): raise ValueError("invalid systemd service unit")
    if manager == "supervisor" and not re.fullmatch(r"[A-Za-z0-9_.@-]+", target["unit"]): raise ValueError("invalid Supervisor process name")
    for field in ("app_lb_deployment", "app_lb_namespace", "runner_hd_id", "backend_server_id", "region"):
        if not isinstance(target[field], str) or not target[field] or len(target[field]) > 128:
            raise ValueError("invalid target identity")
    return target


def request(value):
    if not isinstance(value, dict) or set(value) not in (REQUEST_FIELDS, DAEMON_REQUEST_FIELDS): raise ValueError("request fields differ from closed schema")
    if value.get("component", "heyvm") not in ("heyvm", "heyvmd"): raise ValueError("invalid component")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value["operation_id"]): raise ValueError("invalid operation ID")
    endpoint(value["artifact_url"])
    if not isinstance(value["artifact_size"], int) or not 0 < value["artifact_size"] <= MAX_ARTIFACT: raise ValueError("invalid artifact size")
    if not all(valid_sha(value[k]) for k in ("artifact_sha256", "inner_archive_sha256", "heyvm_sha256")): raise ValueError("invalid digest")
    p = pathlib.PurePosixPath(value["inner_path"])
    if p.is_absolute() or not p.parts or any(x in ("", ".", "..") for x in p.parts): raise ValueError("unsafe inner path")
    return value


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs): return None


def download(req, opener=None):
    opener = opener or urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    call = urllib.request.Request(req["artifact_url"], headers={"Accept": "application/octet-stream"})
    with opener.open(call, timeout=60) as response:
        if getattr(response, "status", 200) != 200: raise ValueError("artifact download refused")
        data = response.read(req["artifact_size"] + 1)
    if len(data) != req["artifact_size"] or sha(data) != req["artifact_sha256"]: raise ValueError("outer artifact identity mismatch")
    return data


def _members(data, mode):
    archive = tarfile.open(fileobj=io.BytesIO(data), mode=mode)
    members = archive.getmembers()
    for m in members:
        p = pathlib.PurePosixPath(m.name)
        if p.is_absolute() or any(x == ".." for x in p.parts) or m.issym() or m.islnk(): raise ValueError("unsafe archive member")
    return archive, members


def executable(outer, req):
    archive, members = _members(outer, "r:*")
    found = [m for m in members if m.name == req["inner_path"]]
    if len(found) != 1 or not found[0].isfile() or found[0].size > MAX_ARTIFACT: raise ValueError("missing or ambiguous inner archive")
    inner = archive.extractfile(found[0]).read(MAX_ARTIFACT + 1)
    archive.close()
    if sha(inner) != req["inner_archive_sha256"]: raise ValueError("inner archive identity mismatch")
    archive, members = _members(inner, "r:gz")
    component = req.get("component", "heyvm")
    found = [m for m in members if (pathlib.PurePosixPath(m.name).name == "heyvm" if component == "heyvm" else pathlib.PurePosixPath(m.name) == pathlib.PurePosixPath(component))]
    if len(found) != 1 or not found[0].isfile() or not 4 <= found[0].size <= MAX_HEYVM: raise ValueError("missing or ambiguous requested executable")
    binary = archive.extractfile(found[0]).read(MAX_HEYVM + 1)
    archive.close()
    if not binary.startswith(b"\x7fELF") or sha(binary) != req["heyvm_sha256"]: raise ValueError("executable identity mismatch")
    return binary


class Host:
    def command(self, argv):
        return subprocess.run(argv, check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout
    def boot_id(self): return pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    def starttime(self, pid): return pathlib.Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()[19]
    def proc_exe(self, pid): return os.path.realpath(f"/proc/{pid}/exe")
    def proc_digest(self, pid): return sha(pathlib.Path(f"/proc/{pid}/exe").read_bytes())
    def cgroup_pids(self, group):
        safe_path(group, directory=True)
        root = pathlib.Path("/sys/fs/cgroup") / group.lstrip("/")
        files = [root / "cgroup.procs", *root.glob("**/*/cgroup.procs")]
        return {int(pid) for path in files for pid in path.read_text().split()}
    def environment_has(self, pid, expected):
        data = pathlib.Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
        return data.count(("HEYVM_HOST_UPDATE_CONFIG=" + expected).encode()) == 1
    def health(self, url):
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        with opener.open(url, timeout=5) as response:
            if response.status != 200: raise ValueError("health status refused")
            return json.loads(response.read(1024 * 1024 + 1))


def service(host, target, component="heyvm"):
    if target.get("process_manager", "systemd") == "supervisor":
        text = host.command(["supervisorctl", "pid", target["unit"]]).strip()
        if not text.isdigit() or int(text) <= 1: raise ValueError("unsafe Supervisor process state")
        pid = int(text)
        if host.proc_exe(pid) != os.path.realpath(target["executable"]): raise ValueError("service executable differs")
        return {"boot_id": host.boot_id(), "pid": pid, "starttime": host.starttime(pid), "disk_sha256": sha(pathlib.Path(target["executable"]).read_bytes()), "running_sha256": host.proc_digest(pid)}
    keys = "LoadState ActiveState KillMode MainPID ExecStart ControlGroup".split()
    text = host.command(["systemctl", "show", target["unit"], "--property=" + ",".join(keys)])
    values = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    if values.get("LoadState") != "loaded" or values.get("ActiveState") != "active": raise ValueError("unsafe service state")
    pid = int(values.get("MainPID", "0")); command = values.get("ExecStart", "")
    if values.get("KillMode") != "process":
        if component != "heyvmd" or values.get("KillMode") != "control-group" or host.cgroup_pids(values.get("ControlGroup", "")) != {pid}:
            raise ValueError("unsafe service process group")
    # systemctl show serializes ExecCommand with an authoritative path= field.
    paths = re.findall(r"(?:^|[ {;])path=([^ ;}]+)", command)
    # The legacy eu1 service starts a stable symlink. Before the one-time
    # bootstrap /proc resolves that symlink to its versioned release; after the
    # atomic replacement both names are the stable path. Require both identities
    # rather than incorrectly requiring their string representations to match.
    if pid <= 1 or len(paths) != 1 or paths[0] != target["executable"] or host.proc_exe(pid) != os.path.realpath(target["executable"]):
        raise ValueError("service executable differs")
    return {"boot_id": host.boot_id(), "pid": pid, "starttime": host.starttime(pid), "disk_sha256": sha(pathlib.Path(target["executable"]).read_bytes()), "running_sha256": host.proc_digest(pid)}


def wait_for_health(host, url):
    # Type=simple only waits for the process to start, not its HTTP listener.
    deadline = time.monotonic() + 30
    while True:
        try:
            return host.health(url)
        except (urllib.error.URLError, ConnectionError, TimeoutError) as error:
            if isinstance(error, urllib.error.HTTPError):
                error.close()
                if error.code not in (502, 503, 504):
                    raise
            if time.monotonic() >= deadline:
                raise
            time.sleep(1)


def secure_file(path, limit, allow_symlink=False):
    p = pathlib.Path(path)
    if not p.exists() and not p.is_symlink(): return {"present": False}
    s = p.lstat()
    if allow_symlink and stat.S_ISLNK(s.st_mode):
        link = os.readlink(path)
        safe_path(link)
        resolved = p.resolve(strict=True)
        info = resolved.stat()
        if not stat.S_ISREG(info.st_mode) or s.st_uid != 0 or info.st_uid != 0 or info.st_nlink != 1 or info.st_size > limit:
            raise ValueError("unsafe predecessor symlink")
        return {"present": True, "link_target": link}
    if not stat.S_ISREG(s.st_mode) or s.st_uid != 0 or s.st_nlink != 1 or s.st_size > limit: raise ValueError("unsafe predecessor file")
    return {"present": True, "mode": stat.S_IMODE(s.st_mode), "bytes": base64.b64encode(p.read_bytes()).decode()}


def fsync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try: os.fsync(fd)
    finally: os.close(fd)


def atomic(path, data, mode):
    p = pathlib.Path(path); p.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
    fd, name = tempfile.mkstemp(prefix=".heyvm-update-", dir=p.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as out: out.write(data); out.flush(); os.fsync(out.fileno())
        os.replace(name, p); fsync_dir(p.parent)
    finally:
        if os.path.exists(name): os.unlink(name)


def atomic_symlink(path, target):
    p = pathlib.Path(path); p.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
    fd, name = tempfile.mkstemp(prefix=".heyvm-update-", dir=p.parent); os.close(fd); os.unlink(name)
    try:
        os.symlink(target, name); os.replace(name, p); fsync_dir(p.parent)
    finally:
        if os.path.lexists(name): os.unlink(name)


def save_journal(path, value): atomic(path, json.dumps(value, sort_keys=True, separators=(",", ":")).encode(), 0o600)


def exact_regular(path, mode):
    info = pathlib.Path(path).lstat()
    return stat.S_ISREG(info.st_mode) and info.st_uid == 0 and info.st_nlink == 1 and stat.S_IMODE(info.st_mode) == mode


def exact_files(target):
    config = {"target": target["target_alias"], "executable": target["executable"], "systemdUnit": target["unit"],
              "maintenanceStateDirectory": target["state_dir"]}
    config_bytes = (json.dumps(config, sort_keys=True, separators=(",", ":")) + "\n").encode()
    drop = ("[Service]\nEnvironment=HEYVM_HOST_UPDATE_CONFIG=" + target["config_json_path"] + "\n").encode()
    return config_bytes, drop


def restart(host, target):
    if target.get("process_manager", "systemd") == "supervisor": host.command(["supervisorctl", "restart", target["unit"]])
    else: host.command(["systemctl", "restart", target["unit"]])


def restore(host, target, journal):
    daemon = journal.get("component") == "heyvmd"
    files = (("executable", target["executable"]),) if daemon else (("executable", target["executable"]), ("config", target["config_json_path"]), ("drop_in", target["systemd_drop_in_path"]))
    for key, path in files:
        old = journal["predecessors"][key]
        if old.get("link_target") is not None: atomic_symlink(path, old["link_target"])
        elif old["present"]: atomic(path, base64.b64decode(old["bytes"], validate=True), old["mode"])
        elif pathlib.Path(path).exists() or pathlib.Path(path).is_symlink(): pathlib.Path(path).unlink(); fsync_dir(pathlib.Path(path).parent)
    if not daemon: host.command(["systemctl", "daemon-reload"])
    restart(host, target)
    now = service(host, target, journal.get("component", "heyvm"))
    before = journal["service"]
    if now["disk_sha256"] != before["disk_sha256"] or now["running_sha256"] != before["running_sha256"] or host.proc_exe(now["pid"]) != os.path.realpath(target["executable"]):
        raise ValueError("rollback verification failed")


def install(target, req, binary, host=None):
    host = host or Host(); operation_hash = sha(json.dumps(req, sort_keys=True, separators=(",", ":")).encode())
    journal_path = str(pathlib.Path(target["state_dir"]) / (req["operation_id"] + ".json"))
    if pathlib.Path(journal_path).exists():
        old = json.loads(pathlib.Path(journal_path).read_bytes())
        if old.get("request_sha256") != operation_hash: raise ValueError("operation ID request conflict")
        if old.get("status") in TERMINAL: return old["result"]
        raise ValueError("operation is nonterminal; operator reconciliation required")
    component=req.get("component", "heyvm"); daemon=component == "heyvmd"
    before = service(host, target, component)
    if before["disk_sha256"] != before["running_sha256"]: raise ValueError("predecessor executable drift")
    predecessors={"executable": secure_file(target["executable"], MAX_HEYVM, allow_symlink=True)}
    if not daemon: predecessors.update(config=secure_file(target["config_json_path"], MAX_SMALL_BACKUP), drop_in=secure_file(target["systemd_drop_in_path"], MAX_SMALL_BACKUP))
    journal = {"version": 1, "component":component, "operation_id": req["operation_id"], "request_sha256": operation_hash, "status": "prepared", "service": before, "predecessors":predecessors}
    pathlib.Path(target["state_dir"]).mkdir(parents=True, exist_ok=True, mode=0o700)
    save_journal(journal_path, journal)  # durable before the first mutation
    config, drop = exact_files(target)
    try:
        atomic(target["executable"], binary, 0o755)
        if not daemon:
            atomic(target["config_json_path"], config, 0o600); atomic(target["systemd_drop_in_path"], drop, 0o644); host.command(["systemctl", "daemon-reload"])
        restart(host, target)
        now = service(host, target, component)
        if now["boot_id"] != before["boot_id"] or now["pid"] == before["pid"] or now["starttime"] == before["starttime"]: raise ValueError("service generation did not change")
        if now["disk_sha256"] != req["heyvm_sha256"] or now["running_sha256"] != req["heyvm_sha256"] or host.proc_exe(now["pid"]) != os.path.realpath(target["executable"]): raise ValueError("new executable verification failed")
        if not daemon and (pathlib.Path(target["config_json_path"]).read_bytes() != config or pathlib.Path(target["systemd_drop_in_path"]).read_bytes() != drop): raise ValueError("installed file verification failed")
        if not exact_regular(target["executable"], 0o755) or (not daemon and (not exact_regular(target["config_json_path"], 0o600) or not exact_regular(target["systemd_drop_in_path"], 0o644))): raise ValueError("installed ownership or mode verification failed")
        if not daemon and not host.environment_has(now["pid"], target["config_json_path"]): raise ValueError("service environment verification failed")
        health = wait_for_health(host, target["local_health_url"])
        if health.get("backendId", health.get("backend_id")) != target["backend_server_id"] or health.get("backendRegion", health.get("backend_region")) != target["region"] or health.get("status") not in ("ok", "healthy", "running"):
            raise ValueError("health identity or API status differs")
        if service(host, target, component) != now: raise ValueError("service changed during health verification")
        result = {"protocol": "host-heyvm-bootstrap-v1", "operation_id": req["operation_id"], "request_sha256": operation_hash,
                  "target_alias": target["target_alias"], "status": "succeeded", "heyvm_sha256": req["heyvm_sha256"],
                  "config_sha256": sha(config) if not daemon else sha(b""), "systemd_drop_in_sha256": sha(drop) if not daemon else sha(b""),
                  "backend_server_id": target["backend_server_id"], "region": target["region"]}
        journal.update(status="succeeded", result=result); save_journal(journal_path, journal); return result
    except Exception as error:
        # Keep the failing source location without logging command arguments,
        # response bodies, or exception messages that may contain credentials.
        journal["failure"] = {"type": type(error).__name__, "line": error.__traceback__.tb_lineno}
        try:
            restore(host, target, journal)
            result = {"protocol": "host-heyvm-bootstrap-v1", "operation_id": req["operation_id"], "request_sha256": operation_hash,
                      "target_alias": target["target_alias"], "status": "rolled_back",
                      "backend_server_id": target["backend_server_id"], "region": target["region"]}
            journal.update(status="rolled_back", result=result); save_journal(journal_path, journal); return result
        except Exception as rollback_error:
            journal["rollback_failure"] = {"type": type(rollback_error).__name__, "line": rollback_error.__traceback__.tb_lineno}
            result = {"protocol": "host-heyvm-bootstrap-v1", "operation_id": req["operation_id"], "request_sha256": operation_hash,
                      "target_alias": target["target_alias"], "status": "rollback_failed",
                      "backend_server_id": target["backend_server_id"], "region": target["region"]}
            journal.update(status="rollback_failed", result=result); save_journal(journal_path, journal); return result


def verify_existing(target, req, host=None):
    """Read-only recovery: never download, install, restart, or rewrite the journal."""
    host = host or Host()
    journal_path = pathlib.Path(target["state_dir"]) / (req["operation_id"] + ".json")
    old = json.loads(journal_path.read_bytes())
    operation_hash = sha(json.dumps(req, sort_keys=True, separators=(",", ":")).encode())
    if old.get("status") != "succeeded" or old.get("request_sha256") != operation_hash:
        raise ValueError("no matching successful bootstrap journal")
    config, drop = exact_files(target); daemon=req.get("component", "heyvm") == "heyvmd"
    expected = {"protocol": "host-heyvm-bootstrap-v1", "operation_id": req["operation_id"], "request_sha256": operation_hash,
                "target_alias": target["target_alias"], "status": "succeeded", "heyvm_sha256": req["heyvm_sha256"],
                "config_sha256": sha(config) if not daemon else sha(b""), "systemd_drop_in_sha256": sha(drop) if not daemon else sha(b""),
                "backend_server_id": target["backend_server_id"], "region": target["region"]}
    if old.get("result") != expected: raise ValueError("saved receipt differs")
    now = service(host, target, req.get("component", "heyvm"))
    if now["disk_sha256"] != req["heyvm_sha256"] or now["running_sha256"] != req["heyvm_sha256"] or host.proc_exe(now["pid"]) != os.path.realpath(target["executable"]):
        raise ValueError("current executable differs")
    if not daemon and (pathlib.Path(target["config_json_path"]).read_bytes() != config or pathlib.Path(target["systemd_drop_in_path"]).read_bytes() != drop):
        raise ValueError("current bootstrap files differ")
    if not exact_regular(target["executable"], 0o755) or (not daemon and (not exact_regular(target["config_json_path"], 0o600) or not exact_regular(target["systemd_drop_in_path"], 0o644))):
        raise ValueError("current ownership or mode differs")
    if not daemon and not host.environment_has(now["pid"], target["config_json_path"]): raise ValueError("current environment differs")
    health = host.health(target["local_health_url"])
    if health.get("backendId", health.get("backend_id")) != target["backend_server_id"] or health.get("backendRegion", health.get("backend_region")) != target["region"] or health.get("status") not in ("ok", "healthy", "running"):
        raise ValueError("current health identity differs")
    if service(host, target) != now: raise ValueError("service changed during verification")
    return expected


def main():
    envelope = json.loads(base64.b64decode(sys.argv[1], validate=True))
    verify_only = envelope.pop("verify_only", False)
    if not isinstance(verify_only, bool): raise ValueError("invalid verification mode")
    closed(envelope, {"mapping_json", "target_alias", "request"}, "envelope")
    target = mapping(envelope["mapping_json"], envelope["target_alias"]); req = request(envelope["request"])
    result = verify_existing(target, req) if verify_only else install(target, req, executable(download(req), req))
    print("HEYO_HEYVM_BOOTSTRAP_RESULT=" + base64.b64encode(json.dumps(result, sort_keys=True, separators=(",", ":")).encode()).decode(), flush=True)


if __name__ == "__main__":
    try: main()
    except Exception:
        result = {"protocol": "host-heyvm-bootstrap-v1", "status": "refused"}
        print("HEYO_HEYVM_BOOTSTRAP_RESULT=" + base64.b64encode(json.dumps(result, separators=(",", ":")).encode()).decode(), flush=True)
        sys.exit(1)
