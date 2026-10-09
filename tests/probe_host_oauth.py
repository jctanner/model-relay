"""Synthetic host-login reuse. The real host credential file is never mounted."""
import http.client
import json
import pathlib
import time
import urllib.request

KEY = pathlib.Path('/run/secrets/proxy-api-key').read_text().strip()
ADMIN = pathlib.Path('/run/secrets/admin-api-key').read_text().strip()
SOURCE = pathlib.Path('/synthetic-host/.credentials.json')
TOKEN = pathlib.Path('/run/secrets/upstream-token')
ROOT = 'http://vertex-shim:8081/api/private/v1'
MODEL = 'claude-sonnet-4-5@20250929'
ROUTE = f'/v1/projects/local/locations/us-east5/publishers/anthropic/models/{MODEL}:rawPredict'


def get(url, key=None):
    req = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + key} if key else {})
    with urllib.request.urlopen(req, timeout=5) as response:
        return json.load(response)


def ready(state):
    for _ in range(40):
        status = get(ROOT + '/auth/status', ADMIN)
        if status['state'] == state:
            assert status['managed'] is False
            return
        time.sleep(0.1)
    raise AssertionError(status)


def call(label):
    connection = http.client.HTTPConnection('vertex-shim', 8080, timeout=5)
    body = {'anthropic_version': 'vertex-2023-10-16', 'max_tokens': 100,
            'messages': [{'role': 'user', 'content': 'private_host_oauth_prompt'}], 'metadata': {'user_id': label}}
    connection.request('POST', ROUTE, json.dumps(body), {'x-api-key': KEY,
                       'Content-Type': 'application/json', 'x-shim-run-id': 'host-oauth-' + label})
    response = connection.getresponse()
    data = json.loads(response.read())
    connection.close()
    return response.status, data


def replace(root, mode=0o600):
    path = SOURCE.parent / '.fixture-replacement'
    path.write_text(json.dumps(root))
    path.chmod(mode)
    path.replace(SOURCE)


for port, key, expected in ((8081, KEY, 401), (8080, ADMIN, 404)):
    connection = http.client.HTTPConnection('vertex-shim', port, timeout=5)
    connection.request('GET', '/api/private/v1/auth/status', headers={'Authorization': 'Bearer ' + key})
    response = connection.getresponse()
    assert response.status == expected, response.status
    response.read()
    connection.close()
ready('ready')
original = SOURCE.read_bytes()
assert call('initial')[0] == 200
assert SOURCE.read_bytes() == original, 'watcher/proxy modified host credentials'
root = json.loads(original)
root['claudeAiOauth']['accessToken'] = 'synthetic-host-rotated'
TOKEN.write_text('synthetic-host-rotated\n')
replace(root)
time.sleep(0.8)
assert call('rotated')[0] == 200, 'atomic source/mirror replacement was not followed'
projected = pathlib.Path('/mirror/.credentials.json').read_text()
assert 'refreshToken' not in projected and 'never_export_other_auth' not in projected

root['claudeAiOauth']['expiresAt'] = 1
replace(root)
ready('needs_login')
assert call('expired')[0] == 503
SOURCE.unlink()
for _ in range(40):
    if not pathlib.Path('/mirror/.credentials.json').exists():
        break
    time.sleep(0.1)
assert not pathlib.Path('/mirror/.credentials.json').exists()
assert call('logged-out')[0] == 503
root['claudeAiOauth']['expiresAt'] = int(time.time() * 1000) + 3600000
replace(root)
ready('ready')
assert call('relogin')[0] == 200
replace(root, mode=0o644)
ready('needs_login')
assert call('invalid-permissions')[0] == 503
replace(root)
ready('ready')

stats = get('http://upstream-fixture:8090/stats')
assert stats.get('refresh_requests', 0) == 0 and stats.get('contract_failures', 0) == 0, stats
assert all(stats.get(label, 0) == 0 for label in ('expired', 'logged-out', 'invalid-permissions')), stats
records = get(ROOT + '/telemetry/requests?limit=100', ADMIN)
serialized = json.dumps(records)
for secret in (KEY, ADMIN, root['claudeAiOauth']['accessToken'], root['claudeAiOauth']['refreshToken'], 'private_host_oauth_prompt'):
    assert secret not in serialized
print(json.dumps({'status': 'passed', 'profile': 'read-only host mirror', 'refresh_requests': 0,
                  'checks': ['source unchanged', 'access-only projection', 'atomic host rotation',
                             'expiry', 'logout', 'relogin', 'permissions', 'no provider auth calls', 'telemetry privacy']}))
