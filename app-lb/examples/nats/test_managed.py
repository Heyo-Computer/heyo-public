"""Test the built managed image with disposable stores and authenticated NATS.

Requires Docker/OrbStack, the nats CLI, and NATS_TEST_IMAGE. No production access.
"""
import base64
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
import unittest


class ManagedBrokerTest(unittest.TestCase):
    def setUp(self):
        self.image = os.environ["NATS_TEST_IMAGE"]
        self.tmp = tempfile.TemporaryDirectory(prefix="heyo-nats-managed-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.env = {**os.environ, "NATS_TOKEN": secrets.token_hex(32)}

    def command(self, *args, check=True, env=None):
        return subprocess.run(args, env=self.env if env is None else env,
                              text=True, capture_output=True, check=check, timeout=40)

    def store(self, name):
        path = self.root / name
        path.mkdir()
        (path / ".managed-state").write_text("nats-state-v1\n")
        (path / "jetstream").mkdir()
        return path

    def refused(self, path, expected, token=True):
        args = ["docker", "run", "--rm", "--entrypoint", "/opt/nats/start.sh"]
        if token:
            args += ["--env", "NATS_TOKEN"]
        if path:
            args += ["--mount", f"type=bind,source={path},target=/workspace"]
        result = self.command(*args, self.image, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(expected, result.stderr)
        self.assertNotIn(self.env["NATS_TOKEN"], result.stderr)

    def broker(self, path):
        cid = self.command("docker", "run", "-d", "--entrypoint", "/opt/nats/start.sh",
                           "--env", "NATS_TOKEN", "-p", "127.0.0.1::4222",
                           "--mount", f"type=bind,source={path},target=/workspace",
                           self.image).stdout.strip()
        self.addCleanup(self.command, "docker", "rm", "-f", cid)
        address = self.command("docker", "port", cid, "4222/tcp").stdout.strip()
        url = "nats://" + address
        self.ready(url)
        return cid, url

    def cli(self, url, *args, check=True, env=None):
        return self.command("nats", "--no-context", "--server", url, "--timeout", "2s",
                            *args, check=check, env=env)

    def ready(self, url):
        for _ in range(30):
            result = self.cli(url, "stream", "ls", "--json", check=False)
            if result.returncode == 0:
                return
            time.sleep(0.2)
        self.fail("broker did not become ready: " + result.stderr)

    def test_refuses_missing_credentials(self):
        self.refused(self.store("state"), "configure the managed broker credential", token=False)

    def test_credentials_are_opaque_not_config_expressions(self):
        for index, token in enumerate(['9j-token', '123456', 'true', '9j-"quoted"\\slash']):
            with self.subTest(index=index):
                self.env['NATS_TOKEN'] = token
                _, url = self.broker(self.store('token-' + str(index)))
                self.assertFalse(json.loads(self.cli(url, 'stream', 'ls', '--json').stdout))
                wrong = {**self.env, 'NATS_TOKEN': token + '-wrong'}
                self.assertNotEqual(self.cli(url, 'stream', 'ls', '--json', check=False,
                                            env=wrong).returncode, 0)

    def test_refuses_rootfs_storage(self):
        self.refused(None, "dedicated mounted workspace")

    def test_refuses_missing_marker(self):
        path = self.store("state")
        (path / ".managed-state").unlink()
        self.refused(path, "explicitly seeded")

    def test_refuses_missing_or_redirected_store(self):
        path = self.store("state")
        (path / "jetstream").rmdir()
        self.refused(path, "seeded JetStream directory")
        (path / "jetstream").symlink_to("/tmp", target_is_directory=True)
        self.refused(path, "seeded JetStream directory")

    def test_backup_restore_and_restart_preserve_acknowledgements(self):
        source_id, source = self.broker(self.store("source"))
        _, destination = self.broker(self.store("destination"))
        # Reachability without valid authentication must never mean success.
        wrong = {**self.env, "NATS_TOKEN": "incorrect-token"}
        self.assertNotEqual(self.cli(source, "stream", "ls", "--json", check=False,
                                     env=wrong).returncode, 0)
        anonymous = {k: v for k, v in self.env.items() if not k.startswith("NATS_")}
        self.assertNotEqual(self.cli(source, "stream", "ls", "--json", check=False,
                                     env=anonymous).returncode, 0)
        self.cli(source, "stream", "add", "MIGRATION", "--subjects", "migration.test",
                 "--storage", "file", "--retention", "limits", "--defaults")
        self.cli(source, "consumer", "add", "MIGRATION", "worker", "--pull",
                 "--ack", "explicit", "--deliver", "all", "--wait", "10m", "--defaults")
        payloads = ["acked-first", "unacked-second", "undelivered-third"]
        for payload in payloads:
            self.cli(source, "pub", "migration.test", payload)
        self.cli(source, "consumer", "next", "MIGRATION", "worker", "--ack", "--count", "1")
        self.cli(source, "consumer", "next", "MIGRATION", "worker", "--no-ack", "--count", "1")
        before = json.loads(self.cli(source, "consumer", "info", "MIGRATION", "worker", "--json").stdout)
        self.assertEqual(before["ack_floor"]["stream_seq"], 1)
        self.assertEqual(before["delivered"]["stream_seq"], 2)
        self.cli(source, "stream", "backup", "MIGRATION", str(self.root / "backup"),
                 "--consumers", "--check", "--no-progress")
        self.cli(destination, "stream", "restore", str(self.root / "backup"), "--no-progress")
        # Restart the original independently: destination remains reachable.
        self.command("docker", "restart", source_id)
        # Docker may allocate a different ephemeral host port on restart.
        source = "nats://" + self.command("docker", "port", source_id, "4222/tcp").stdout.strip()
        self.ready(source)
        for url in (source, destination):
            after = json.loads(self.cli(url, "consumer", "info", "MIGRATION", "worker", "--json").stdout)
            self.assertEqual(after["num_ack_pending"], 1)
            self.assertEqual(after["num_pending"], 1)
            for field in ("ack_floor", "delivered"):
                for seq in ("stream_seq", "consumer_seq"):
                    self.assertEqual(before[field][seq], after[field][seq])
            state = json.loads(self.cli(url, "stream", "info", "MIGRATION", "--json").stdout)["state"]
            self.assertEqual((state["messages"], state["first_seq"], state["last_seq"]), (3, 1, 3))
            for seq, expected in enumerate(payloads, 1):
                message = json.loads(self.cli(url, "stream", "get", "MIGRATION", str(seq), "--json").stdout)
                self.assertEqual(base64.b64decode(message["data"]).decode(), expected)


if __name__ == "__main__":
    unittest.main()
