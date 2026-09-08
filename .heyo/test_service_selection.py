"""Execute the public workflow planner offline; never build or deploy services."""
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOW = Path('.heyo/workflows/deploy-heyo-services.yml').read_text()
PLAN = textwrap.dedent(WORKFLOW.split('      - name: Plan public service deployment\n', 1)[1]
                       .split('        run: |\n', 1)[1].split('\n      - name:', 1)[0])
TARGETS = dict(re.findall(r'- target: (\S+)\n\s+paths: (\S+)', WORKFLOW))


def plan(target, paths='', **overrides):
    with tempfile.NamedTemporaryFile() as output:
        env = {'PATH': os.environ['PATH'], 'GITHUB_ENV': output.name,
               'TARGET_HEYO_SERVICE': target,
               'TARGET_HEYO_SERVICE_PATHS': TARGETS.get(target, target),
               'CI_CHANGED_PATHS': paths, 'CI_REPO_URL': 'https://example.test/repo',
               **overrides}
        result = subprocess.run(['sh', '-c', PLAN], env=env, text=True, capture_output=True)
        values = dict(line.split('=', 1) for line in Path(output.name).read_text().splitlines())
    return result, values


class ServiceSelectionTests(unittest.TestCase):
    def selected(self, paths, **overrides):
        selected = []
        for target in TARGETS:
            result, values = plan(target, paths, **overrides)
            self.assertEqual(result.returncode, 0, result.stderr)
            if values['SERVICE_SELECTED'] == '1':
                selected.append(target)
        return set(selected)

    def test_host_app_lb_is_not_a_vm_target(self):
        self.assertEqual(set(TARGETS), {'orchestrator', 'heyosecret', 'app-obs'})
        self.assertEqual(self.selected('app-lb/src/main.rs'), set())
        self.assertNotEqual(plan('app-lb', REQUESTED_HEYO_SERVICE='all')[0].returncode, 0)
        self.assertNotEqual(plan('orchestrator', REQUESTED_HEYO_SERVICE='app-lb')[0].returncode, 0)

    def test_source_changes_select_only_their_owner(self):
        for paths, expected in [('orchestrator/README.md', {'orchestrator'}),
                                ('heyosecret-client/src/lib.rs', {'orchestrator'}),
                                ('heyosecret/src/main.rs', {'heyosecret'}),
                                ('app-obs/src/main.rs', {'app-obs'}),
                                ('orchestrator-old/src/main.rs', set())]:
            with self.subTest(paths=paths):
                self.assertEqual(self.selected(paths), expected)

    def test_shared_or_empty_changes_do_not_replace_services(self):
        shared = '.heyo/workflows/deploy-heyo-services.yml\n.heyo/deployment-environments.json'
        for paths in ['', ' \n', shared]:
            self.assertEqual(self.selected(paths), set())
        self.assertEqual(self.selected(shared + '\norchestrator/README.md'), {'orchestrator'})

    def test_json_changes_select_only_their_owner(self):
        for target in TARGETS:
            self.assertEqual(self.selected(f'.heyo/services/{target}.json'), {target})
        self.assertEqual(self.selected('.heyo/services/app-lb.json'), set())
        self.assertEqual(self.selected('.heyo/services/orchestrator.json.bak'), set())

    def test_explicit_dispatch_overrides_paths(self):
        self.assertEqual(self.selected('heyosecret/src/main.rs', REQUESTED_HEYO_SERVICE='orchestrator'),
                         {'orchestrator'})
        self.assertEqual(self.selected('', REQUESTED_HEYO_SERVICE='all'), set(TARGETS))
        self.assertNotEqual(plan('orchestrator', REQUESTED_HEYO_SERVICE='typo')[0].returncode, 0)

    def test_post_merge_requires_validated_artifact_only_for_selected_service(self):
        source = {'CI_SOURCE': 'git-submit-post-merge'}
        result, _ = plan('orchestrator', 'orchestrator/src/main.rs', **source)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('requires the validated orchestrator artifact', result.stderr)
        result, values = plan('orchestrator', 'orchestrator/src/main.rs',
                              VALIDATED_SERVICE_ARTIFACTS_AVAILABLE='1', **source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values['USE_VALIDATED_SERVICE_ARTIFACTS'], '1')
        result, values = plan('heyosecret', 'orchestrator/src/main.rs', **source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(values['SERVICE_SELECTED'], '0')
        self.assertEqual(values['USE_VALIDATED_SERVICE_ARTIFACTS'], '0')

    def test_bootstrap_requires_explicit_orchestrator(self):
        for requested in ['', 'all', 'heyosecret']:
            self.assertNotEqual(plan('orchestrator', REQUESTED_HEYO_SERVICE=requested,
                                     BOOTSTRAP_DISCOVERY_ROUTING='true')[0].returncode, 0)
        self.assertEqual(self.selected('', REQUESTED_HEYO_SERVICE='orchestrator',
                                       BOOTSTRAP_DISCOVERY_ROUTING='true'), {'orchestrator'})


if __name__ == '__main__':
    unittest.main()
