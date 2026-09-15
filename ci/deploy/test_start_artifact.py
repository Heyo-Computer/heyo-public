"""Boot-contract tests; only the filesystem-device probe is simulated."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class ArtifactBootTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.release = self.root / "release"
        self.runtime = self.root / "runtime"
        self.state = self.root / "state"
        self.tools = self.root / "tools"
        for directory in (self.release, self.runtime, self.state, self.tools):
            directory.mkdir()
        self.sha = "38567c9e9260d2deeb3a03047d56ac217056ecea"
        (self.release / "REVISION").write_text(self.sha + "\n")
        (self.release / "ci").write_bytes(b"validated candidate\n")
        digest = hashlib.sha256((self.release / "ci").read_bytes()).hexdigest()
        (self.release / "SHA256SUMS").write_text(digest + "  ci\n")
        (self.runtime / "ci").write_bytes(b"old runtime\n")
        (self.runtime / "start.sh").write_text('echo supervisor-started\n')
        (self.state / ".managed-state").write_text("ci-state-v1\n")
        # A mounted filesystem has a different device ID from the rootfs.
        probe = self.tools / "stat"
        probe.write_text('#!/bin/sh\nif [ "$3" = / ] || [ "${ROOTFS_ONLY:-}" = 1 ]; then echo 1; else echo 2; fi\n')
        probe.chmod(0o755)

    def boot(self, **extra_env):
        env = {**os.environ, "CI_EXPECTED_SHA": self.sha,
               "PATH": str(self.tools) + os.pathsep + os.environ["PATH"], **extra_env}
        return subprocess.run(["bash", str(Path(__file__).with_name("start-artifact.sh")),
                               str(self.release), str(self.runtime), str(self.state)],
                              env=env, text=True, capture_output=True)

    def assert_refused(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("supervisor-started", result.stdout)
        self.assertEqual((self.runtime / "ci").read_bytes(), b"old runtime\n")

    def test_installs_exact_candidate_before_starting_supervisor(self):
        result = self.boot()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("supervisor-started", result.stdout)
        self.assertEqual((self.runtime / "ci").read_bytes(), b"validated candidate\n")
        self.assertEqual((self.runtime / "REVISION").read_text().strip(), self.sha)
        self.assertTrue((self.runtime / "ci").stat().st_mode & 0o111)

    def test_refuses_rootfs_state(self):
        self.assert_refused(self.boot(ROOTFS_ONLY="1"))

    def test_refuses_unseeded_state(self):
        (self.state / ".managed-state").unlink()
        self.assert_refused(self.boot())

    def test_refuses_wrong_revision(self):
        self.assert_refused(self.boot(CI_EXPECTED_SHA="a" * 40))

    def test_refuses_corrupted_binary(self):
        (self.release / "ci").write_bytes(b"different bytes\n")
        self.assert_refused(self.boot())


if __name__ == "__main__":
    unittest.main()
