"""Disposable local relay plus synthetic provider. Never reads host credentials."""
from datetime import datetime, timedelta, timezone
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[1]
MODELS = ['telemetry-a', 'telemetry-b']


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def call(base, path, key=None, body=None, timeout=15):
    headers = {'Content-Type': 'application/json', 'anthropic-version': '2023-06-01'}
    if key:
        headers['Authorization'] = 'Bearer ' + key
    data = None if body is None else json.dumps(body).encode()
    try:
        with urlopen(Request(base + path, data=data, headers=headers), timeout=timeout) as response:
            return response.status, json.load(response)
    except HTTPError as error:
        data = error.read()
        try:
            return error.code, json.loads(data)
        except json.JSONDecodeError:
            return error.code, {'error': {'message': 'Non-JSON validation response'}}


class Relay:
    def __init__(self, history=0, keys=32, hold=0.7, events=16, gap=0.005,
                 one_cpu=False, payload_bytes=1024, binary=None):
        self.temp = tempfile.TemporaryDirectory(prefix='relay-telemetry-test-')
        self.directory = Path(self.temp.name)
        self.database = self.directory / 'telemetry.sqlite'
        self.history = history
        self.keys = [secrets.token_hex(24) for _ in range(keys)]
        self.admin_key, upstream_key = secrets.token_hex(24), secrets.token_hex(24)
        self.events = events
        self.payload = 'x' * payload_bytes
        self.binary = Path(binary).resolve() if binary else ROOT / 'target/debug/vertex-shim'
        self.processes = []
        self.inference = f'http://127.0.0.1:{free_port()}'
        self.admin = f'http://127.0.0.1:{free_port()}'
        self.provider = f'http://127.0.0.1:{free_port()}'
        assert len({self.inference, self.admin, self.provider}) == 3
        self.one_cpu = one_cpu
        for name, value in {
            'keys.json': json.dumps({'keys': [{'client_id': f'fixture-client-{i}', 'key_id': f'fixture-key-{i}',
                'sha256': hashlib.sha256(key.encode()).hexdigest(), 'models': MODELS, 'backends': ['anthropic']}
                for i, key in enumerate(self.keys)]}),
            'models.json': json.dumps({model: {'window_tokens': 200000, 'upstream_model': model} for model in MODELS}),
            'admin-key': self.admin_key, 'upstream-key': upstream_key,
            'provider.json': json.dumps({'port': urlsplit(self.provider).port, 'token': upstream_key,
                'models': MODELS, 'hold_seconds': hold, 'events': events, 'event_gap_seconds': gap}),
        }.items():
            path = self.directory / name
            path.write_text(value)
            path.chmod(0o600)
        self.environment = {'PATH': os.environ['PATH'], 'SHIM_BACKEND': 'anthropic',
            'SHIM_UPSTREAM_BASE_URL': self.provider, 'SHIM_UPSTREAM_SYNTHETIC': 'true',
            'SHIM_UPSTREAM_TOKEN_FILE': str(self.directory / 'upstream-key'),
            'SHIM_LISTEN': self.inference.removeprefix('http://'),
            'SHIM_ADMIN_LISTEN': self.admin.removeprefix('http://'),
            'SHIM_KEYS_FILE': str(self.directory / 'keys.json'),
            'SHIM_MODELS_FILE': str(self.directory / 'models.json'),
            'SHIM_ADMIN_KEY_FILE': str(self.directory / 'admin-key'),
            'SHIM_DB': str(self.database), 'SHIM_CLAUDE_BIN': '/bin/false'}
        if history:
            self.seed_history(history)

    def seed_history(self, count):
        stamp = (datetime.now(timezone.utc) - timedelta(days=30)).isoformat()
        with sqlite3.connect(self.database) as connection:
            connection.execute('CREATE TABLE requests(sequence INTEGER PRIMARY KEY AUTOINCREMENT,request_id TEXT UNIQUE NOT NULL,payload TEXT NOT NULL)')
            def rows():
                for i in range(count):
                    record = {'schema_version': 1, 'build_id': 'fixture', 'request_id': f'history-{i}',
                        'attempt_id': f'history-{i}', 'run_id': 'history', 'started_at': stamp, 'finished_at': stamp,
                        'backend': 'anthropic', 'synthetic': True, 'client_api': 'anthropic',
                        'upstream_profile': 'default', 'upstream_auth': 'fixture', 'project_id': 'vertex-shim-local',
                        'client_id': 'history', 'key_id': 'history', 'operation': 'generation',
                        'requested_model': MODELS[i % 2], 'resolved_model': MODELS[i % 2],
                        'usage': {'input_tokens': 12, 'output_tokens': 3, 'cache_read_input_tokens': 8, 'cache_creation_input_tokens': 5},
                        'usage_status': 'complete', 'usage_source': 'synthetic history', 'completion': 'complete',
                        'http_status': 200, 'latency_ms': 1, 'context': {}}
                    yield record['request_id'], json.dumps(record)
            connection.executemany('INSERT INTO requests(request_id,payload) VALUES(?,?)', rows())

    def spawn(self, argv, environment=None, one_cpu=False):
        log = (self.directory / f'process-{len(self.processes)}.log').open('ab')
        def affinity():
            if one_cpu:
                os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
        process = subprocess.Popen(argv, cwd=self.directory, env=environment,
                                   stdout=log, stderr=log, preexec_fn=affinity if one_cpu else None)
        log.close()
        self.processes.append(process)
        return process

    def start(self):
        self.spawn([sys.executable, str(ROOT / 'tests/fixtures/telemetry_streaming.py'), str(self.directory / 'provider.json')], {'PATH': os.environ['PATH']})
        self.wait_ready(self.provider, '/fixture/status')
        began = time.monotonic()
        self.relay = self.spawn([str(self.binary)], self.environment, self.one_cpu)
        status = self.wait_ready(self.inference, '/health')
        assert status['synthetic'] is True and status['backend'] == 'anthropic'
        self.startup_seconds = time.monotonic() - began
        return self

    def wait_ready(self, base, path):
        for _ in range(1200):
            if any(process.poll() is not None for process in self.processes):
                raise RuntimeError('Local fixture process exited before readiness.')
            try:
                status, value = call(base, path)
                if status == 200:
                    return value
            except OSError:
                pass
            time.sleep(0.05)
        raise RuntimeError('Local fixture readiness timed out.')

    def admin_call(self, path):
        return call(self.admin, '/api/private/v1' + path, self.admin_key)

    def generate(self, run, worker=0, stream=True, cancel=False):
        url = urlsplit(self.inference)
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=30)
        started = time.monotonic()
        try:
            connection.request('POST', '/v1/messages', json.dumps({'model': MODELS[worker % 2], 'max_tokens': self.events,
                'stream': stream, 'messages': [{'role': 'user', 'content': self.payload}]}),
                {'Content-Type': 'application/json', 'anthropic-version': '2023-06-01',
                 'Authorization': 'Bearer ' + self.keys[worker % len(self.keys)], 'x-shim-run-id': run})
            response = connection.getresponse()
            request_id = response.getheader('x-request-id')
            if cancel and response.status == 200:
                response.readline()
                data = b''
            else:
                data = response.read()
                if response.status == 200 and stream:
                    assert b'event: message_stop' in data
            return response.status, request_id, time.monotonic() - started
        finally:
            connection.close()

    def settle(self, timeout=30):
        began = time.monotonic()
        while time.monotonic() - began < timeout:
            status, health = self.admin_call('/health')
            if status == 200 and health['pending_writes'] == 0:
                return health
            time.sleep(0.05)
        raise RuntimeError('Telemetry writer did not drain.')

    def records(self, run):
        items, cursor = [], None
        while True:
            query = f'/telemetry/requests?run_id={run}&limit=500&order=desc'
            if cursor is not None:
                query += f'&cursor={cursor}'
            status, page = self.admin_call(query)
            assert status == 200, status
            items.extend(page['items'])
            cursor = page['next_cursor']
            if cursor is None:
                return items

    def restart(self):
        self.relay.terminate()
        self.relay.wait(timeout=10)
        self.processes.remove(self.relay)
        self.relay = self.spawn([str(self.binary)], self.environment, self.one_cpu)
        self.wait_ready(self.inference, '/health')

    def close(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
        self.temp.cleanup()

    def __enter__(self):
        try:
            return self.start()
        except BaseException:
            self.close()
            raise

    def __exit__(self, *_):
        self.close()
