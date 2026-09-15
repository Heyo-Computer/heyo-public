"""Offline regression checks for the actual workflow's deployment inputs."""
import os
from pathlib import Path
import textwrap
import unittest
from unittest.mock import patch


WORKFLOW = Path('.heyo/workflows/deploy-heyo-services.yml').read_text()
DEPLOY = textwrap.dedent(WORKFLOW.split('      - name: Deploy selected service through Heyo orchestrator\n', 1)[1]
                         .split("          python3 - <<'PY'\n", 1)[1].split('          archive_path=', 1)[0])


def environment(host='stage.heyo.computer', **overrides):
    return {'HEYO_PUBLIC_HOST': host, 'ORCHESTRATOR_INTERNAL_API_KEY': 'test-key',
            'CI_REPO_URL': 'https://example.test/repo', 'CI_REF': 'refs/heads/main',
            'CI_AFTER': 'test-revision', **overrides}


def payload(env, service='orchestrator'):
    scope = {}
    # Start with the empty job settings from the failed run, without a GITHUB_ENV handoff.
    with patch.dict(os.environ, {**env, 'TARGET_HEYO_SERVICE': service}, clear=True), \
         patch('subprocess.check_output', return_value='test-revision\n'):
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
        self.assertNotIn('scaling', payload(environment(), 'app-obs')[0])
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
