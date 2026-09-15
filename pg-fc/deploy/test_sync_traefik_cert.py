import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("sync-traefik-cert.py")
SPEC = importlib.util.spec_from_file_location("sync_traefik_cert", SCRIPT)
sync_cert = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sync_cert)


class SyncTraefikCertTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.hostname = "pg.example.com"
        self.cert, self.key = self.make_cert("one")
        self.source = self.write_acme(self.cert, self.key)
        self.destination = self.root / "tls"

    def tearDown(self):
        self.temporary.cleanup()

    def make_cert(self, name):
        key = self.root / f"{name}.key"
        cert = self.root / f"{name}.crt"
        subprocess.run(
            [
                "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-days", "1", "-subj", f"/CN={self.hostname}",
                "-addext", f"subjectAltName=DNS:{self.hostname}",
                "-keyout", str(key), "-out", str(cert),
            ],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        return cert.read_bytes(), key.read_bytes()

    def write_acme(self, cert, key):
        source = self.root / "acme-tls.json"
        source.write_text(json.dumps({"resolver": {"Certificates": [{
            "domain": {"main": self.hostname, "sans": ["other.example"]},
            "certificate": base64.b64encode(cert).decode(),
            "key": base64.b64encode(key).decode(),
        }]}}))
        return source

    def test_successful_export_is_idempotent_and_secure(self):
        self.assertTrue(sync_cert.sync(self.source, self.hostname, self.destination))
        current = self.destination / "current"
        generation = current.resolve()
        self.assertEqual((generation / "cert.pem").read_bytes(), self.cert)
        self.assertEqual((generation / "key.pem").read_bytes(), self.key)
        self.assertEqual(os.stat(generation).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(generation / "cert.pem").st_mode & 0o777, 0o600)
        target = os.readlink(current)
        self.assertFalse(sync_cert.sync(self.source, self.hostname, self.destination))
        self.assertEqual(os.readlink(current), target)

    def test_bad_material_preserves_prior_current(self):
        sync_cert.sync(self.source, self.hostname, self.destination)
        original = os.readlink(self.destination / "current")
        _, other_key = self.make_cert("other")
        cases = [(b"not a certificate", self.key), (self.cert, other_key)]
        for cert, key in cases:
            with self.subTest(cert=cert[:10]):
                self.write_acme(cert, key)
                with self.assertRaises(sync_cert.SyncError):
                    sync_cert.sync(self.source, self.hostname, self.destination)
                self.assertEqual(os.readlink(self.destination / "current"), original)

    def test_renewal_switches_both_files_and_keeps_previous_generation(self):
        sync_cert.sync(self.source, self.hostname, self.destination)
        original = (self.destination / "current").resolve()
        cert, key = self.make_cert("renewed")
        self.write_acme(cert, key)
        self.assertTrue(sync_cert.sync(self.source, self.hostname, self.destination))
        current = self.destination / "current"
        self.assertNotEqual(current.resolve(), original)
        self.assertEqual((current / "cert.pem").read_bytes(), cert)
        self.assertEqual((current / "key.pem").read_bytes(), key)
        self.assertEqual((original / "cert.pem").read_bytes(), self.cert)
        self.assertEqual(os.stat(current / "key.pem").st_mode & 0o777, 0o600)

    def test_metadata_cannot_disguise_a_wrong_certificate_hostname(self):
        sync_cert.sync(self.source, self.hostname, self.destination)
        original = os.readlink(self.destination / "current")
        data = json.loads(self.source.read_text())
        data["resolver"]["Certificates"][0]["domain"]["main"] = "wrong.example"
        self.source.write_text(json.dumps(data))
        with self.assertRaises(sync_cert.SyncError):
            sync_cert.sync(self.source, "wrong.example", self.destination)
        self.assertEqual(os.readlink(self.destination / "current"), original)


if __name__ == "__main__":
    unittest.main()
