#!/usr/bin/env python3
"""Export one Traefik ACME certificate for a local service."""

import argparse
import base64
import binascii
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


class SyncError(Exception):
    pass


def _certificate_entries(value):
    entries = []
    if isinstance(value, dict):
        for key, child in value.items():
            if key == "Certificates":
                if not isinstance(child, list):
                    raise SyncError("Traefik Certificates value is not a list")
                entries.extend(child)
            else:
                entries.extend(_certificate_entries(child))
    elif isinstance(value, list):
        for child in value:
            entries.extend(_certificate_entries(child))
    return entries


def _select_material(source, hostname):
    try:
        data = json.loads(Path(source).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise SyncError(f"cannot read Traefik ACME JSON: {exc}") from exc

    matches = []
    for entry in _certificate_entries(data):
        if not isinstance(entry, dict):
            raise SyncError("malformed Traefik certificate entry")
        domain = entry.get("domain") or entry.get("Domain")
        if not isinstance(domain, dict):
            continue
        main = domain.get("main") if "main" in domain else domain.get("Main")
        sans = domain.get("sans") if "sans" in domain else domain.get("SANs", [])
        if sans is None:
            sans = []
        if not isinstance(main, str) or not isinstance(sans, list) or not all(
            isinstance(name, str) for name in sans
        ):
            raise SyncError("malformed Traefik certificate domain")
        if hostname == main or hostname in sans:
            matches.append(entry)

    if len(matches) != 1:
        raise SyncError(
            f"expected exactly one certificate naming {hostname!r}, found {len(matches)}"
        )
    entry = matches[0]
    encoded_cert = entry.get("certificate", entry.get("Certificate"))
    encoded_key = entry.get("key", entry.get("Key"))
    if not isinstance(encoded_cert, str) or not isinstance(encoded_key, str):
        raise SyncError("selected certificate has no encoded certificate or key")
    try:
        return (
            base64.b64decode(encoded_cert, validate=True),
            base64.b64decode(encoded_key, validate=True),
        )
    except (binascii.Error, ValueError) as exc:
        raise SyncError("selected certificate or key is not valid base64") from exc


def _openssl(arguments, *, input_data=None):
    try:
        result = subprocess.run(
            ["openssl", *arguments],
            input=input_data,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except FileNotFoundError as exc:
        raise SyncError("openssl is not installed") from exc
    if result.returncode != 0:
        raise SyncError(f"openssl validation failed ({' '.join(arguments[:2])})")
    return result.stdout


def _validate(cert, key, hostname):
    if not cert or not key:
        raise SyncError("selected certificate or key is empty")
    with tempfile.TemporaryDirectory(prefix="sync-traefik-cert-") as temporary:
        cert_path = Path(temporary, "cert.pem")
        key_path = Path(temporary, "key.pem")
        cert_path.write_bytes(cert)
        key_path.write_bytes(key)
        os.chmod(cert_path, 0o600)
        os.chmod(key_path, 0o600)
        _openssl(["x509", "-in", str(cert_path), "-noout", "-checkend", "0"])
        # OpenSSL 3.0 can exit zero even when -checkhost reports a mismatch.
        # Require its positive verdict, not merely successful command execution.
        verdict = _openssl(["x509", "-in", str(cert_path), "-noout", "-checkhost", hostname])
        if verdict.strip() != f"Hostname {hostname} does match certificate".encode():
            raise SyncError("certificate hostname does not match")
        cert_public = _openssl(["x509", "-in", str(cert_path), "-pubkey", "-noout"])
        key_public = _openssl(["pkey", "-in", str(key_path), "-pubout"])
        if cert_public != key_public:
            raise SyncError("certificate and private key do not match")


def sync(source, hostname, destination):
    cert, key = _select_material(source, hostname)
    _validate(cert, key, hostname)
    digest = hashlib.sha256(cert + b"\0" + key).hexdigest()
    destination = Path(destination)
    generation_name = f"generation-{digest}"
    generation = destination / generation_name
    current = destination / "current"

    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(destination, 0o700)
    if current.exists() and not current.is_symlink():
        raise SyncError(f"refusing to replace non-symlink {current}")
    unchanged = current.is_symlink() and os.readlink(current) == generation_name
    if unchanged:
        return False

    if generation.exists():
        if not generation.is_dir() or (generation / "cert.pem").read_bytes() != cert or (
            generation / "key.pem"
        ).read_bytes() != key:
            raise SyncError(f"existing generation is inconsistent: {generation}")
    else:
        staging = Path(tempfile.mkdtemp(prefix=".generation-", dir=destination))
        try:
            os.chmod(staging, 0o700)
            for name, material in (("cert.pem", cert), ("key.pem", key)):
                path = staging / name
                path.write_bytes(material)
                os.chmod(path, 0o600)
            os.rename(staging, generation)
        finally:
            if staging.exists():
                shutil.rmtree(staging)

    temporary_link = destination / f".current-{os.getpid()}"
    try:
        temporary_link.symlink_to(generation_name)
        os.replace(temporary_link, current)
    finally:
        temporary_link.unlink(missing_ok=True)

    return True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", help="Traefik ACME JSON file")
    parser.add_argument("hostname", help="exact certificate main/SAN hostname")
    parser.add_argument("destination", help="certificate generation directory")
    args = parser.parse_args(argv)
    try:
        sync(args.source, args.hostname, args.destination)
    except SyncError as exc:
        parser.exit(1, f"sync-traefik-cert: {exc}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
