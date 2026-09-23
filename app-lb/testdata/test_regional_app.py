import hashlib
import http.client
import json
import threading
import unittest

from regional_app import RegionalApp, server


class RegionalAppTests(unittest.TestCase):
    def setUp(self):
        self.app = server('eu1', 'revision-17')
        self.worker = threading.Thread(target=self.app.serve_forever, daemon=True)
        self.worker.start()

    def tearDown(self):
        self.app.shutdown()
        self.app.server_close()
        self.worker.join()

    def request(self, path, method='GET', body=None, headers=None):
        connection = http.client.HTTPConnection('127.0.0.1', self.app.server_port, timeout=5)
        try:
            connection.request(method, path, body, headers or {})
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()

    def test_identity_health_and_single_admission_without_credential_reflection(self):
        status, headers, _ = self.request('/health')
        self.assertEqual(status, 200)
        self.assertEqual(headers['X-Heyo-Revision'], 'revision-17')
        self.assertEqual(headers['X-Heyo-Region'], 'eu1')
        status, _, raw = self.request('/action?id=once', 'POST', 'payload-23', {'Authorization': 'Bearer test-only'})
        record = json.loads(raw)
        self.assertEqual(status, 200)
        self.assertEqual((record['region'], record['revision'], record['id'], record['body']),
                         ('eu1', 'revision-17', 'once', 'payload-23'))
        self.assertEqual(record['authorizationSha256'], hashlib.sha256(b'Bearer test-only').hexdigest())
        self.assertNotIn(b'Bearer test-only', raw)
        records = json.loads(self.request('/admissions')[2])['admissions']
        self.assertEqual([r['id'] for r in records], ['once'])

    def test_unhealthy_is_not_healthy_and_invalid_requests_are_not_admitted(self):
        self.app.unhealthy = True
        self.assertEqual(self.request('/health')[0], 503)
        for hold in ['nan', 'inf', '-1', '60.1']:
            self.assertEqual(self.request('/held?hold=' + hold)[0], 400)
        self.assertEqual(self.request('/health', headers={'X-Heyo-Peer-Token': 'must-be-consumed'})[0], 400)
        self.assertEqual(list(self.app.admissions), [])

    def test_response_body_remains_open_after_headers(self):
        entered, release = threading.Event(), threading.Event()

        class Held(RegionalApp):
            def hold(self, seconds):
                entered.set()
                release.wait(5)

        self.app.RequestHandlerClass = Held
        connection = http.client.HTTPConnection('127.0.0.1', self.app.server_port, timeout=5)
        try:
            connection.request('GET', '/held?id=long')
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(response.read(1), b'{')
            self.assertTrue(entered.wait(1))
            self.assertEqual([r['id'] for r in self.app.admissions], ['long'])
            release.set()
            self.assertEqual(json.loads(b'{' + response.read())['revision'], 'revision-17')
        finally:
            release.set()
            connection.close()


if __name__ == '__main__':
    unittest.main()
