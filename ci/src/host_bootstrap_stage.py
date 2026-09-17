"""Fixed bootstrap transport: stage pinned inputs, invoke native code, never install."""
import base64
import hashlib
import io
import json
import os
import pathlib
import stat
import subprocess
import sys
import tarfile
import urllib.request


def digest(data):
    return hashlib.sha256(data).hexdigest()


def trusted(path):
    for item in [path, *path.parents]:
        if not item.exists() and not item.is_symlink():
            continue
        info = item.lstat()
        if info.st_uid != 0 or info.st_mode & 0o022 or stat.S_ISLNK(info.st_mode):
            raise ValueError("untrusted staging path")
        if stat.S_ISREG(info.st_mode) and info.st_nlink != 1:
            raise ValueError("linked staging file")


def directory(path):
    trusted(path)
    if not path.exists():
        directory(path.parent)
        path.mkdir(mode=0o700)
        sync(path.parent)
    if not path.is_dir():
        raise ValueError("staging parent is not a directory")


def sync(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def publish(path, data, mode):
    trusted(path)
    if path.exists():
        if path.read_bytes() != data or stat.S_IMODE(path.stat().st_mode) != mode:
            raise ValueError("staging identity conflict")
        return
    # Exclusive creation: a partial file is a conflict, never silently repaired.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    with os.fdopen(fd, "wb") as out:
        out.write(data)
        out.flush()
        os.fsync(out.fileno())
    sync(path.parent)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def main():
    request = json.loads(base64.b64decode(sys.argv[1], validate=True))
    os.umask(0o077)
    root = pathlib.Path("/var/lib/app-lb-bootstrap-staging") / request["operation_id"]
    directory(root)
    binary_path = root / request["binary_sha256"]
    if binary_path.exists():
        trusted(binary_path)
        binary = binary_path.read_bytes()
    else:
        # Public artifact transport has no credentials, proxy inheritance or redirects.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        with opener.open(request["artifact_url"], timeout=60) as response:
            if response.status != 200:
                raise ValueError("artifact download refused")
            archive = response.read(request["artifact_size"] + 1)
        if len(archive) != request["artifact_size"] or digest(archive) != request["artifact_sha256"]:
            raise ValueError("artifact identity mismatch")
        # Caller validated this exact archive with the shared strict parser.
        # Extract a single regular member into memory, never archive paths to disk.
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
            members = [m for m in tar if m.name == "dist/app-lb"]
            if len(members) != 1 or not members[0].isfile() or members[0].size > 268435456:
                raise ValueError("invalid executable member")
            binary = tar.extractfile(members[0]).read(268435457)
    if digest(binary) != request["binary_sha256"]:
        raise ValueError("executable identity mismatch")
    publish(binary_path, binary, 0o700)
    data = base64.b64decode(request["input_base64"], validate=True)
    if digest(data) != request["input_sha256"]:
        raise ValueError("native input identity mismatch")
    input_path = root / (request["input_sha256"] + ".json")
    publish(input_path, data, 0o600)
    argv = [str(binary_path), "--bootstrap-host-update", request["phase"], str(input_path)]
    if request["phase"] == "admit":
        argv.append(request["input_sha256"])
    elif request["phase"] != "inspect":
        raise ValueError("unsupported transport phase")
    # No restart/config mutation here: the pinned native binary alone owns it.
    result = subprocess.run(argv, cwd="/", stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode != 0:
        # Native errors may mention config; do not forward potentially secret bytes.
        raise ValueError("native bootstrap refused; reconcile native state")
    value = json.loads(result.stdout)
    print("HEYO_BOOTSTRAP_RESULT=" + base64.b64encode(json.dumps(value).encode()).decode(), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print("bootstrap transport failed; do not retry delivery", file=sys.stderr)
        sys.exit(1)
