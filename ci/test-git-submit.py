#!/usr/bin/env python3
"""Executed regression tests for the public git-submit shell client."""

import base64
import hashlib
import hmac
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import unittest


CLIENT = pathlib.Path(__file__).parent / "bin" / "git-submit"
TOKEN = "repo-token-must-not-leak"
SECRET = "shared-secret-must-not-leak"


class SubmitHandler(http.server.BaseHTTPRequestHandler):
    requests = []

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        headers = {name.lower(): value for name, value in self.headers.items()}
        self.requests.append((self.path, headers, body))
        response = b'{"runs":["test-run"],"url":"http://example/"}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def log_message(self, *_args):
        pass


class GitSubmitTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        self.repo = root / "repo"
        self.origin = root / "origin.git"
        subprocess.run(["git", "init", "--bare", "-q", self.origin], check=True)
        subprocess.run(["git", "init", "-q", "-b", "main", self.repo], check=True)
        self.git("config", "user.name", "Test User")
        self.git("config", "user.email", "test@example.invalid")
        (self.repo / "file.txt").write_text("main\n")
        self.git("add", "file.txt")
        self.git("commit", "-qm", "main")
        self.main_sha = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-q", "-u", "origin", "main")
        self.git("remote", "set-head", "origin", "main")
        self.git("switch", "-qc", "feature")
        (self.repo / "file.txt").write_text("feature\n")
        self.git("commit", "-qam", "feature")
        self.feature_sha = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("switch", "-qc", "pr-source", "main")
        (self.repo / "file.txt").write_text("pull request\n")
        self.git("commit", "-qam", "pull request")
        self.pr_sha = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("push", "-q", "origin", "HEAD:refs/pull/59/head")
        self.git("switch", "-q", "feature")
        SubmitHandler.requests = []
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SubmitHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.server.server_port}"

    def tearDown(self):
        self.server.shutdown()
        self.thread.join()
        self.server.server_close()
        self.temp.cleanup()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True, text=True,
                              capture_output=True)

    def run_client(self, *args, env=None, check=True):
        clean_env = os.environ.copy()
        for key in ("CI_ENDPOINT", "CICD_ENDPOINT", "CI_TOKEN", "CI_WEBHOOK_SECRET"):
            clean_env.pop(key, None)
        clean_env.update(env or {})
        result = subprocess.run([CLIENT, *args], cwd=self.repo, env=clean_env,
                                text=True, capture_output=True)
        if check and result.returncode:
            self.fail(f"git-submit failed:\n{result.stdout}\n{result.stderr}")
        self.assertNotIn(TOKEN, result.stdout + result.stderr)
        self.assertNotIn(SECRET, result.stdout + result.stderr)
        return result

    def test_payload_metadata_base_endpoint_and_repository_token(self):
        self.git("config", "ci.endpoint", self.base)
        self.git("config", "ci.token", TOKEN)
        self.run_client()
        path, headers, raw = SubmitHandler.requests.pop()
        payload = json.loads(raw)
        self.assertEqual(path, "/api/submit")
        self.assertEqual(headers["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(payload["after"], self.feature_sha)
        self.assertEqual(payload["ref"], "refs/heads/feature")
        self.assertEqual(payload["repository"]["defaultBranch"], "main")
        self.assertEqual(payload["repository"]["releaseBaseSha"], self.main_sha)
        self.assertNotIn(TOKEN, raw.decode())

    def test_legacy_endpoint_git_push_and_shared_hmac(self):
        self.run_client(env={"CICD_ENDPOINT": self.base + "/git/push",
                             "CI_WEBHOOK_SECRET": SECRET})
        path, headers, raw = SubmitHandler.requests.pop()
        self.assertEqual(path, "/api/submit")
        expected = "sha256=" + hmac.new(SECRET.encode(), raw, hashlib.sha256).hexdigest()
        self.assertEqual(headers["x-heyo-signature-256"], expected)

    def test_full_api_submit_spelling(self):
        self.run_client(env={"CI_ENDPOINT": self.base + "/api/submit", "CI_TOKEN": TOKEN})
        self.assertEqual(SubmitHandler.requests.pop()[0], "/api/submit")

    def test_canonical_environment_keeps_precedence_over_canonical_git_config(self):
        self.git("config", "ci.endpoint", "http://127.0.0.1:1/wrong")
        self.run_client(env={"CI_ENDPOINT": self.base, "CI_TOKEN": TOKEN})
        self.assertEqual(SubmitHandler.requests.pop()[0], "/api/submit")

    def test_conflicting_aliases_are_rejected_without_post(self):
        result = self.run_client(env={"CI_ENDPOINT": self.base,
                                      "CICD_ENDPOINT": self.base + "/other",
                                      "CI_TOKEN": TOKEN}, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("conflicting endpoint settings", result.stderr)
        self.assertEqual(SubmitHandler.requests, [])

    def test_dry_run_never_posts_or_discloses_credentials(self):
        # Exceed a pipe buffer: a `head -20` consumer makes git ls-tree die
        # with SIGPIPE under pipefail, although small repositories pass.
        for i in range(1500):
            (self.repo / (f"entry-{i:04d}-" + "x" * 100)).write_text("data\n")
        self.git("add", ".")
        self.git("commit", "-qm", "large tree")
        result = self.run_client("--dry-run", env={"CI_ENDPOINT": self.base + "/git/push",
                                                    "CI_TOKEN": TOKEN})
        self.assertIn(self.base + "/api/submit", result.stdout)
        self.assertEqual(SubmitHandler.requests, [])
        files = result.stdout.split("  files:\n", 1)[1].splitlines()
        self.assertEqual(len(files), 20)

    def test_pr_selector_submits_remote_pr_head_without_checkout_or_remote_write(self):
        head_before = self.git("rev-parse", "HEAD").stdout.strip()
        status_before = self.git("status", "--porcelain=v1").stdout
        remote_before = self.git("ls-remote", "origin").stdout
        self.run_client("pr59", "--submit-empty",
                        env={"CI_ENDPOINT": self.base, "CI_TOKEN": TOKEN})
        _path, _headers, raw = SubmitHandler.requests.pop()
        payload = json.loads(raw)
        self.assertEqual(payload["after"], self.pr_sha)
        self.assertEqual(payload["before"], self.main_sha)
        self.assertEqual(payload["ref"], "refs/heads/pull/59")
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.strip(), head_before)
        self.assertEqual(self.git("status", "--porcelain=v1").stdout, status_before)
        self.assertEqual(self.git("ls-remote", "origin").stdout, remote_before)
        self.assertEqual(self.git("for-each-ref", "refs/git-submit").stdout, "")

        bundle = pathlib.Path(self.temp.name) / "submitted.bundle"
        bundle.write_bytes(base64.b64decode(payload["source"]["contentBase64"]))
        heads = subprocess.run(["git", "bundle", "list-heads", bundle], check=True,
                               text=True, capture_output=True).stdout
        self.assertIn(f"{self.pr_sha} refs/heads/pull/59", heads)

    def test_pr_selector_rejects_malformed_and_conflicting_selectors(self):
        for args in (("pr0",), ("prx",), ("pr59x",), ("pr59", "--ref", "HEAD"),
                     ("pr59", "--dirty"), ("pr59", "pr60")):
            with self.subTest(args=args):
                result = self.run_client(*args, env={"CI_ENDPOINT": self.base,
                                                     "CI_TOKEN": TOKEN}, check=False)
                self.assertNotEqual(result.returncode, 0)
        self.assertEqual(SubmitHandler.requests, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
