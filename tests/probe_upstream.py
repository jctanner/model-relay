"""Offline adapter acceptance. Uses only generated synthetic credentials."""
import http.client
import json
import pathlib
import time
import urllib.error
import urllib.request
import uuid

KEY = pathlib.Path('/run/secrets/proxy-api-key').read_text().strip()
ADMIN = pathlib.Path('/run/secrets/admin-api-key').read_text().strip()
TOKEN_FILE = pathlib.Path('/run/secrets/upstream-token')
MODEL = 'claude-sonnet-4-5@20250929'
ROUTE = f'/v1/projects/local/locations/us-east5/publishers/anthropic/models/{MODEL}'
RUN = 'upstream-' + uuid.uuid4().hex
PAYLOAD = {'anthropic_version': 'vertex-2023-10-16', 'max_tokens': 100,
           'messages': [{'role': 'user', 'content': 'private_prompt_marker'}]}


def get(url, key=None):
    req = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + key} if key else {})
    with urllib.request.urlopen(req, timeout=5) as response:
        return json.load(response)


def call(mode, stream=False, key=KEY, count=False, extras=None):
    body = {**PAYLOAD, 'metadata': {'user_id': mode}, **(extras or {})}
    path = ROUTE + (':streamRawPredict' if stream else ':rawPredict')
    if count:
        path = ROUTE.replace(MODEL, 'count-tokens') + ':rawPredict'
        body['model'] = MODEL
        body.pop('max_tokens')
    connection = http.client.HTTPConnection('vertex-shim', 8080, timeout=6)
    started = time.monotonic()
    connection.request('POST', path, json.dumps(body), {
        'x-api-key': key, 'Content-Type': 'application/json', 'x-shim-run-id': RUN + '-' + mode,
        'x-client-secret': 'must_not_forward', 'anthropic-beta': 'fixture-beta',
    })
    response = connection.getresponse()
    headers = dict(response.getheaders())
    if stream and response.status == 200:
        first = response.readline()
        assert first.startswith(b'event: message_start'), first
        assert time.monotonic() - started < 0.5, 'response was buffered'
        if mode == 'cancel':
            while response.readline().strip():
                pass
            response.close()
            connection.close()
            return response.status, headers, first
        try:
            data = first + response.read()
        except http.client.IncompleteRead as error:
            data = first + error.partial
    else:
        data = response.read()
    connection.close()
    return response.status, headers, data


for key in ('invalid', ADMIN):
    assert call('denied', key=key)[0] == 401
status, _, data = call('normal')
assert status == 200 and json.loads(data)['usage']['output_tokens'] == 3
status, _, data = call('tool', extras={'tools': [{'name': 'fixture_tool', 'input_schema': {'type': 'object'}}]})
assert status == 200 and json.loads(data)['content'][0]['input']['secret'] == 'private_tool_argument'
assert call('count', count=True)[0] == 200
for mode in ('stream', 'gzip', 'malformed', 'truncated', 'error', 'cancel', 'timeout'):
    status, headers, data = call(mode, stream=True)
    assert status == 200, (mode, data)
    assert 'Content-Encoding' not in headers
    if mode in ('stream', 'gzip', 'malformed'):
        assert b'event: message_stop' in data and 'héllo_private_response'.encode() in data
for mode, expected in [('rate', 429), ('server', 500), ('redirect', 307)]:
    status, headers, _ = call(mode)
    assert status == expected and headers.get('retry-after') == '7'

# Same inode is mounted: replacing the file contents rotates the server token.
old = TOKEN_FILE.read_text().strip()
TOKEN_FILE.write_text('synthetic-rotated-' + uuid.uuid4().hex + '\n')
assert call('rotated')[0] == 200
stats = get('http://upstream-fixture:8090/stats')
assert stats.get('denied', 0) == 0 and stats.get('contract_failures', 0) == 0, stats
assert all(stats.get(mode) == 1 for mode in ('rate', 'server', 'redirect')), stats

root = 'http://vertex-shim:8081/api/private/v1'
for _ in range(120):
    page = get(root + '/telemetry/requests?limit=100', ADMIN)
    rows = {r['run_id'].removeprefix(RUN + '-'): r for r in page['items'] if (r['run_id'] or '').startswith(RUN)}
    if len(rows) == 14 and all(r['completion'] != 'in_progress' for r in rows.values()):
        break
    time.sleep(0.05)
assert len(rows) == 14, rows
assert rows['cancel']['completion'] == 'cancelled', rows['cancel']
for mode in ('normal', 'stream', 'gzip', 'tool', 'rotated'):
    row = rows[mode]
    assert row['usage_status'] == 'complete' and row['usage']['output_tokens'] == 3, row
    assert row['context']['total_input_tokens'] == 25 and row['context']['window_tokens'] is None
    assert row['resolved_model'] == 'fixture-sonnet' and row['upstream_request_id'] == 'req_fixture'
for mode in ('malformed', 'truncated', 'error', 'cancel', 'timeout'):
    assert rows[mode]['usage_status'] == 'partial', rows[mode]
assert rows['truncated']['completion'] == 'failed'
assert rows['error']['error_type'] == 'overloaded_error'
assert rows['malformed']['observation_error'] == 'invalid_sse_json'
assert rows['tool']['stop_reason'] == 'tool_use'
assert all(r['synthetic'] and r['backend'] == 'anthropic' for r in rows.values())
for row in rows.values():
    detail = get(root + '/telemetry/requests/' + row['request_id'], ADMIN)
    events = get(root + '/telemetry/requests/' + row['request_id'] + '/events', ADMIN)
    serialized = json.dumps({'record': detail, 'events': events})
    if row['run_id'].endswith('-stream'):
        assert [e['usage']['output_tokens'] for e in events['items']] == [0, 1, 2, 3], events
    for secret in (KEY, ADMIN, old, TOKEN_FILE.read_text().strip(), 'private_prompt_marker',
                   'private_tool_argument', 'héllo_private_response', 'private provider error', 'must_not_forward'):
        assert secret not in serialized, 'content or credential entered telemetry'
health = get(root + '/health', ADMIN)
assert health['storage_errors'] == 0 and health['dropped_records'] == 0, health
print(json.dumps({'status': 'passed', 'requests': len(rows), 'fixture_contract_failures': stats.get('contract_failures', 0),
                  'checks': ['translation', 'auth isolation', 'rotation', 'tools', 'gzip', 'incremental SSE',
                             'cumulative usage', 'cancellation', 'errors', 'no retries', 'telemetry privacy']}))
