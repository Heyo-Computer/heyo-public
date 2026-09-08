"""Offline regression checks for the actual workflow's deployment inputs."""
import os
from pathlib import Path
import tempfile
import textwrap
import unittest
from unittest.mock import patch


WORKFLOW = Path('.heyo/workflows/deploy-heyo-services.yml').read_text()
LOAD = textwrap.dedent(WORKFLOW.split('      - name: Load deployment environment defaults\n', 1)[1]
                       .split("          python3 - <<'PY'\n", 1)[1].split('\n          PY', 1)[0])
DEPLOY = textwrap.dedent('          discovery_routed=' + WORKFLOW.split('          discovery_routed=', 1)[1]
                         .split('          archive_path=', 1)[0])


def environment(host='stage.heyo.computer', **overrides):
    env = {'HEYO_PUBLIC_HOST': host, **overrides}
    with tempfile.NamedTemporaryFile() as output:
        with patch.dict(os.environ, {**env, 'GITHUB_ENV': output.name}, clear=True):
            exec(LOAD, {})
        env.update(dict(line.split('=', 1) for line in Path(output.name).read_text().splitlines()))
    return env


def payload(env, service='orchestrator'):
    scope = dict(service=service, payload={'vm': {'env_vars': {}}, 'deploy': {}}, route={'strip_prefix': True},
                 public_host=env['HEYO_PUBLIC_HOST'], key='test-key', git_sha='test-revision',
                 envref=lambda name, path: f'{name}=heyosecret://{path}@active', os=os)
    with patch.dict(os.environ, env, clear=True):
        exec(DEPLOY, scope)
    return scope['payload'], scope['route']


class DeploymentEnvironmentTests(unittest.TestCase):
    def test_empty_ci_settings_preserve_stage_discovery(self):
        env = environment(ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES='', HEYO_SERVICE_REPLICAS='')
        for service in ['orchestrator', 'heyosecret']:
            request, route = payload(env, service)
            self.assertEqual(request['scaling'], {'min_replicas': 1, 'max_replicas': 1})
            self.assertNotIn('placement_pool', request['deploy'])
            self.assertNotIn('replica_regions', request['deploy'])
            self.assertFalse(route['strip_prefix'])
        request, _ = payload(env)
        self.assertEqual(request['vm']['env_vars']['ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES'], 'heyosecret,orchestrator')

    def test_other_services_and_hosts_do_not_gain_replica_policy(self):
        for service in ['app-lb', 'app-obs']:
            self.assertNotIn('scaling', payload(environment(), service)[0])
        self.assertNotIn('scaling', payload(environment('other.example'))[0])

    def test_explicit_policy_overrides_defaults(self):
        env = environment(ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES='orchestrator',
                          HEYO_SERVICE_REPLICAS='orchestrator=2',
                          HEYO_SERVICE_REPLICA_REGIONS='US,EU', HEYO_SERVICE_PLACEMENT_POOL='custom')
        request, _ = payload(env)
        self.assertEqual(request['scaling'], {'min_replicas': 2, 'max_replicas': 2})
        self.assertEqual(request['deploy']['replica_regions'], ['US', 'EU'])
        self.assertEqual(request['deploy']['placement_pool'], 'custom')
        self.assertEqual(request['vm']['env_vars']['ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES'], 'orchestrator')

    def test_mismatched_region_count_still_fails(self):
        with self.assertRaises(SystemExit):
            payload(environment(HEYO_SERVICE_REPLICA_REGIONS='US,EU'))


if __name__ == '__main__':
    unittest.main()
