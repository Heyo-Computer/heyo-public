#!/usr/bin/env python3
"""Disposable PostgreSQL 18 integration tests for pg-fc-physical."""
import json
import os
import pathlib
import re
import subprocess
import tempfile
import time
import unittest
import uuid

HERE = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = pathlib.Path(os.environ.get("PG_FC_PHYSICAL_SCRIPT", HERE / "physical.sh")).resolve()
IMAGE = os.environ.get("PG_FC_PHYSICAL_TEST_IMAGE", "postgres:18-bookworm")


def run(*args, check=True, **kwargs):
    result = subprocess.run(args, text=True, capture_output=True, check=False, **kwargs)
    if check and result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {args}\nstdout={result.stdout}\nstderr={result.stderr}")
    return result


class PhysicalSeedTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tag = "pgfc-physical-test-" + uuid.uuid4().hex[:10]
        dockerfile = "FROM postgres:18-bookworm\nRUN apt-get update && apt-get install -y jq util-linux python3-minimal && rm -rf /var/lib/apt/lists/*\n"
        run("docker", "build", "-t", cls.tag, "-", input=dockerfile)

    @classmethod
    def tearDownClass(cls):
        run("docker", "rm", "-f", *getattr(cls, "containers", []), check=False)
        run("docker", "network", "rm", getattr(cls, "network", "missing"), check=False)
        run("docker", "rmi", cls.tag, check=False)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pgfc-physical-")
        self.root = pathlib.Path(self.tmp.name)
        os.chmod(self.root, 0o777)
        self.network = "pgfc-" + uuid.uuid4().hex[:10]
        self.source = self.network + "-source"
        self.candidate = self.network + "-candidate"
        type(self).network = self.network
        type(self).containers = [self.source, self.candidate]
        run("docker", "network", "create", self.network)
        run("docker", "run", "-d", "--name", self.source, "--network", self.network,
            "-e", "POSTGRES_PASSWORD=secret", self.tag,
            "-c", "wal_level=replica", "-c", "max_wal_senders=10",
            "-c", "max_replication_slots=10")
        self.wait_ready(self.source)
        self.sql("CREATE TABLE seed_test(id bigserial PRIMARY KEY, v text); "
                 "INSERT INTO seed_test(v) VALUES ('before'); CREATE SEQUENCE extra_seq START 41; "
                 "CREATE ROLE repl WITH LOGIN REPLICATION PASSWORD 'replsecret'; "
                 "SELECT pg_create_physical_replication_slot('standby_slot');")
        run("docker", "exec", self.source, "sh", "-c",
            "echo 'host replication repl 0.0.0.0/0 scram-sha-256' >> \"$PGDATA/pg_hba.conf\"")
        self.sql("SELECT pg_reload_conf()")
        self.system_id = self.sql("SELECT system_identifier FROM pg_control_system();").strip()
        workspace = self.root / "workspace"
        workspace.mkdir()
        os.chmod(workspace, 0o777)
        run("docker", "run", "-d", "--name", self.candidate, "--network", self.network,
            "-e", "POSTGRES_PASSWORD=scratch", "-e", "PGDATA=/workspace/pgdata",
            "-v", f"{workspace}:/workspace", "--entrypoint", "sh", self.tag, "-c",
            "docker-entrypoint.sh postgres & exec sleep infinity")
        self.wait_ready(self.candidate)
        state = workspace / "pg-fc-physical"
        state.mkdir(); os.chmod(state, 0o777)
        self.write_plan(self.system_id)

    def tearDown(self):
        run("docker", "rm", "-f", self.source, self.candidate, check=False)
        run("docker", "network", "rm", self.network, check=False)
        self.tmp.cleanup()

    def wait_ready(self, container):
        consecutive = 0
        for _ in range(60):
            if run("docker", "exec", container, "pg_isready", "-U", "postgres", check=False).returncode == 0:
                consecutive += 1
                if consecutive == 3:
                    return
            else:
                consecutive = 0
            time.sleep(.25)
        self.fail(f"{container} did not become ready")

    def sql(self, sql):
        return run("docker", "exec", "-e", "PGPASSWORD=secret", self.source,
                   "psql", "-U", "postgres", "-Atqc", sql).stdout

    def cexec(self, command, *, env=(), check=True):
        args = ["docker", "exec"]
        for item in env: args += ["-e", item]
        args += [self.candidate, "sh", "-c", command]
        result = run(*args, check=False)
        if check and result.returncode:
            log = run("docker", "exec", self.candidate, "sh", "-c",
                      "cat /workspace/pg-fc-physical/basebackup.log 2>/dev/null || true", check=False)
            raise RuntimeError(f"candidate command failed: {command}\n{result.stderr}\nbasebackup={log.stdout}")
        return result

    def write_plan(self, system_id):
        plan = {
            "generation": "generation-1", "system_identifier": system_id, "pg_major": 18,
            "conninfo": f"host={self.source} user=repl password=replsecret dbname=postgres",
            "slot": "standby_slot",
            "settings": {"max_connections": 100, "max_prepared_transactions": 0,
                         "max_locks_per_transaction": 64, "max_wal_senders": 10,
                         "max_worker_processes": 8},
        }
        path = self.root / "workspace/pg-fc-physical/plan.json"
        path.write_text(json.dumps(plan)); os.chmod(path, 0o600)
        self.cexec("chown postgres:postgres /workspace/pg-fc-physical/plan.json")

    @property
    def seed_cmd(self):
        return ("PG_FC_TEST_ALLOW_NONMOUNT=1 PG_FC_TEST_ALLOW_PLAN_OWNER=1 PG_FC_ROOT_MARKER=/tmp/physical-required "
                "/mounted/physical.sh seed")

    def mount_script(self):
        # docker exec cannot add mounts, so copy this exact checkout's script.
        run("docker", "cp", str(SCRIPT), f"{self.candidate}:/mounted-physical.sh")
        self.cexec("mkdir -p /mounted && mv /mounted-physical.sh /mounted/physical.sh && chmod +x /mounted/physical.sh")

    def test_seed_replication_recovery_and_boot_guards(self):
        self.mount_script()
        # Plan presence inhibits an incomplete seed.
        r = self.cexec("PG_FC_TEST_ALLOW_NONMOUNT=1 PG_FC_TEST_ALLOW_PLAN_OWNER=1 PG_FC_ROOT_MARKER=/tmp/m /mounted/physical.sh boot-check", check=False)
        self.assertEqual(r.returncode, 20)
        # Copy failure is durable and retryable without touching scratch.
        r = self.cexec("PG_FC_TEST_ALLOW_NONMOUNT=1 PG_FC_TEST_ALLOW_PLAN_OWNER=1 PG_FC_ROOT_MARKER=/tmp/m PG_FC_TEST_FAIL_AT=after-copy /mounted/physical.sh seed", check=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertTrue((self.root / "workspace/pgdata/PG_VERSION").exists())
        self.cexec(self.seed_cmd)
        self.wait_ready(self.candidate)
        out = self.cexec("psql -U postgres -Atqc \"SELECT pg_is_in_recovery(),v FROM seed_test\"", env=("PGHOST=/var/run/postgresql",)).stdout
        self.assertEqual(out.strip(), "t|before")
        self.sql("INSERT INTO seed_test(v) VALUES ('after'); ALTER TABLE seed_test ADD COLUMN n integer DEFAULT 7; SELECT nextval('extra_seq');")
        for _ in range(40):
            out = self.cexec("psql -U postgres -Atqc \"SELECT count(*),max(n),(SELECT last_value FROM extra_seq) FROM seed_test\"",
                             env=("PGHOST=/var/run/postgresql",), check=False)
            fields = out.stdout.strip().split("|")
            if out.returncode == 0 and len(fields) == 3 and fields[:2] == ["2", "7"] and int(fields[2]) >= 42: break
            time.sleep(.25)
        self.assertEqual(fields[:2], ["2", "7"], out)
        self.assertGreaterEqual(int(fields[2]), 42)
        self.assertNotEqual(self.cexec("psql -U postgres -c \"INSERT INTO seed_test(v) VALUES ('no')\"",
                                       env=("PGHOST=/var/run/postgresql",), check=False).returncode, 0)
        self.assertEqual(self.cexec("PG_FC_TEST_ALLOW_NONMOUNT=1 PG_FC_TEST_ALLOW_PLAN_OWNER=1 PG_FC_ROOT_MARKER=/tmp/m /mounted/physical.sh boot-check", check=False).returncode, 10)
        status = json.loads(self.cexec("/mounted/physical.sh status").stdout)
        self.assertEqual(status, {"phase": "active", "error": None})
        self.assertNotIn("conninfo", json.dumps(status))
        # Execute the exact controller verification script. In particular,
        # psql -c does not expand the variables used for identity checks.
        source = (HERE / "src/replication/physical.rs").read_text()
        verify = re.search(r'const VERIFY_RUNTIME: &str = r#"(.*?)"#;', source, re.S).group(1)
        variables = ("PGFC_DB=postgres", "PGFC_ROLE=postgres", f"PGFC_SYSTEM_ID={self.system_id}",
                     "PGFC_SLOT=standby_slot", "PGFC_LSN=0/1", f"PGFC_HOST={self.source}", "PGFC_PORT=5432")
        self.assertEqual(self.cexec(verify, env=variables).stdout.strip(), "t")
        self.assertEqual(self.cexec(verify, env=variables + ("PGFC_SLOT=unrelated_slot",)).stdout.strip(), "f")
        # A completed seed with Postgres stopped must restart, not report
        # success merely because an activation file exists.
        self.cexec("gosu postgres pg_ctl -D /workspace/pgdata -m fast -w stop")
        self.cexec(self.seed_cmd)
        self.assertEqual(self.cexec(verify, env=variables).stdout.strip(), "t")
        # Removing standby.signal must not authorize an accidental primary.
        self.cexec("gosu postgres pg_ctl -D /workspace/pgdata -m fast -w stop")
        self.cexec("rm /workspace/pgdata/standby.signal")
        self.assertEqual(self.cexec("PG_FC_ROOT_MARKER=/tmp/physical-required /mounted/physical.sh boot-check", check=False).returncode, 20)
        self.cexec("mv /workspace/pg-fc-physical/plan.json /workspace/pg-fc-physical/plan.saved")
        self.assertEqual(self.cexec("PG_FC_ROOT_MARKER=/tmp/physical-required /mounted/physical.sh boot-check", check=False).returncode, 20)

    def test_identity_mismatch_concurrency_and_activation_crash(self):
        self.mount_script()
        self.write_plan(str(int(self.system_id) + 1))
        r = self.cexec(self.seed_cmd, check=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("mismatch", json.loads(self.cexec("/mounted/physical.sh status").stdout)["error"])
        self.write_plan(self.system_id)
        # Holding the same advisory lock proves concurrent workers are rejected.
        r = self.cexec("exec 9>/workspace/pg-fc-physical/seed.lock; flock 9; " + self.seed_cmd, check=False)
        self.assertEqual(r.returncode, 75)
        r = self.cexec("PG_FC_TEST_ALLOW_NONMOUNT=1 PG_FC_TEST_ALLOW_PLAN_OWNER=1 PG_FC_ROOT_MARKER=/tmp/m PG_FC_TEST_FAIL_AT=after-quarantine /mounted/physical.sh seed", check=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse((self.root / "workspace/pgdata").exists())
        self.cexec(self.seed_cmd)
        self.wait_ready(self.candidate)
        self.assertEqual(self.cexec("psql -U postgres -Atqc 'select pg_is_in_recovery()'", env=("PGHOST=/var/run/postgresql",)).stdout.strip(), "t")


if __name__ == "__main__":
    unittest.main(verbosity=2)
