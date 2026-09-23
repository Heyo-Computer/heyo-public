"""Disposable regional workload. Gateways own routing; the app knows no peer URLs."""
import argparse
from collections import deque
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import socket
import threading
import time
from urllib.parse import parse_qs, urlsplit


class RegionalApp(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        self.respond()

    def do_POST(self):
        self.respond()

    def hold(self, seconds):
        time.sleep(seconds)

    def respond(self):
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        try:
            seconds = float(query.get('hold', ['0'])[0])
            length = int(self.headers.get('Content-Length', '0'))
            if not math.isfinite(seconds) or not 0 <= seconds <= 60 or not 0 <= length <= 65536:
                raise ValueError()
            if self.headers.get('Transfer-Encoding'):
                raise ValueError()
        except ValueError:
            self.send_error(400, 'invalid hold or body length')
            self.close_connection = True
            return
        body = self.rfile.read(length).decode('utf-8', errors='replace')
        peer_headers = [k for k in self.headers if k.lower().startswith('x-heyo-peer')]
        if peer_headers:
            self.send_error(400, 'gateway did not consume peer credentials')
            return
        record = {
            'region': self.server.region, 'revision': self.server.revision,
            'instance': self.server.instance, 'port': self.server.server_port,
            'id': query.get('id', [''])[0], 'method': self.command,
            'path': self.path, 'host': self.headers.get('Host'), 'body': body,
            # Check credential preservation without reflecting credential values.
            'authorizationSha256': hashlib.sha256(self.headers.get('Authorization', '').encode()).hexdigest(),
            'peerHeaders': peer_headers, 'startedNs': time.time_ns(),
        }
        with self.server.record_lock:
            if url.path == '/admissions':
                payload = {'region': self.server.region, 'revision': self.server.revision,
                           'admissions': list(self.server.admissions)}
            else:
                payload = record
                if url.path != '/health':
                    self.server.admissions.append(record)
        data = json.dumps(payload).encode()
        self.send_response(503 if self.server.unhealthy else 200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Heyo-Revision', self.server.revision)
        self.send_header('X-Heyo-Region', self.server.region)
        self.end_headers()
        # Begin the response before holding it: exercise response-body lifetime,
        # not merely the time spent waiting for response headers.
        self.wfile.write(data[:1])
        self.wfile.flush()
        if url.path == '/held' or seconds:
            self.hold(seconds)
        self.wfile.write(data[1:])
        self.wfile.flush()

    def log_message(self, *args):
        pass


def server(region, revision, bind='127.0.0.1', port=0, unhealthy=False, handler=RegionalApp):
    for value in (region, revision):
        if not value or len(value) > 256 or not all(33 <= ord(c) <= 126 for c in value):
            raise ValueError('region and revision must be bounded printable identifiers')
    app = ThreadingHTTPServer((bind, port), handler)
    app.region, app.revision, app.unhealthy = region, revision, unhealthy
    app.instance = socket.gethostname()
    app.admissions, app.record_lock = deque(maxlen=4096), threading.Lock()
    return app


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--region', required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--bind', default='127.0.0.1')
    parser.add_argument('--port', type=int, default=8080)
    parser.add_argument('--unhealthy', action='store_true')
    args = parser.parse_args()
    app = server(args.region, args.revision, args.bind, args.port, args.unhealthy)
    print(json.dumps({'region': args.region, 'revision': args.revision, 'port': app.server_port}), flush=True)
    try:
        app.serve_forever()
    finally:
        app.server_close()
