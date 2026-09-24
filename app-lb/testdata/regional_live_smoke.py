"""Exercise a deployed regional workload through a public HTTPS app-lb only."""
import argparse
import hashlib
import http.client
import json
import ssl
import time
import uuid
from urllib.parse import urlsplit


def request(origin, host, path, method='GET', body=None):
    url = urlsplit(origin)
    if (url.scheme != 'https' or not url.hostname or url.username or url.password
            or url.path not in ('', '/') or url.query or url.fragment):
        raise ValueError('source origin must be a credential-free HTTPS origin')
    # TLS identity is the source gateway; application Host selects its route.
    connection = http.client.HTTPSConnection(url.hostname, url.port or 443,
                                            timeout=30, context=ssl.create_default_context())
    headers = {'Host': host, 'Authorization': 'Bearer disposable-smoke-probe',
               'Content-Type': 'text/plain', 'Cache-Control': 'no-cache'}
    started = time.monotonic()
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        if response.status != 200:
            raise RuntimeError(f'{origin} {method} {path}: HTTP {response.status}')
        first_byte = response.read(1)
        first_at = time.monotonic() - started
        payload = first_byte + response.read(1024 * 1024)
        if response.read(1):
            raise RuntimeError('workload response exceeded one MiB')
        return response.headers, json.loads(payload), first_at, time.monotonic() - started
    finally:
        connection.close()


def verify(origin, host, region, revision):
    probe_id = uuid.uuid4().hex
    authorization = hashlib.sha256(b'Bearer disposable-smoke-probe').hexdigest()
    for method, path, body in [
        ('GET', '/health', None),
        ('POST', f'/echo?id={probe_id}', 'cross-region-body'),
        ('GET', f'/held?hold=3&id={probe_id}-held', None),
    ]:
        headers, result, first, complete = request(origin, host, path, method, body)
        for name, expected in [('X-Heyo-Region', region), ('X-Heyo-Revision', revision)]:
            if headers.get_all(name) != [expected]:
                raise RuntimeError(f'{name} did not identify the expected workload')
        expected = {'region': region, 'revision': revision, 'method': method,
                    'path': path, 'host': host, 'body': body or '',
                    'authorizationSha256': authorization, 'peerHeaders': []}
        if any(result.get(key) != value for key, value in expected.items()):
            raise RuntimeError('workload identity or preserved request differs')
        if '/held?' in path and complete - first < 2:
            raise RuntimeError('held response did not exercise response-body lifetime')
    _, admissions, _, _ = request(origin, host, '/admissions')
    posts = [item for item in admissions['admissions'] if item['id'] == probe_id]
    if len(posts) != 1 or posts[0]['method'] != 'POST':
        raise RuntimeError('POST was lost or delivered more than once')
    print(json.dumps({'source': origin, 'host': host, 'servingRegion': region,
                      'revision': revision, 'requestPreservation': True,
                      'singlePostDelivery': True, 'heldBody': True}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-origin', required=True)
    parser.add_argument('--host', required=True)
    parser.add_argument('--region', required=True)
    parser.add_argument('--revision', required=True)
    args = parser.parse_args()
    verify(args.source_origin, args.host, args.region, args.revision)
