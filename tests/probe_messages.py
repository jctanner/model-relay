"""Messages and authenticated discovery acceptance; synthetic credentials only."""
import collections
import http.client
import json
import pathlib
import time
import uuid

KEY = pathlib.Path('/run/secrets/proxy-api-key').read_text().strip()
ADMIN = pathlib.Path('/run/secrets/admin-api-key').read_text().strip()
RESTRICTED = pathlib.Path('/run/secrets/restricted-key').read_text().strip()
TOKEN = pathlib.Path('/run/secrets/upstream-token')
REGISTRY = pathlib.Path('/config/keys.json')
RUN = 'messages-' + uuid.uuid4().hex
NEW = 'claude-sonnet-4-6'
OLD = 'claude-sonnet-4-5-20250929'
BODY = {'model': NEW, 'max_tokens': 100, 'messages': [{'role': 'user', 'content': 'private_prompt_marker'}]}


def call(path, method='GET', body=None, key=KEY, headers=None):
    conn = http.client.HTTPConnection('vertex-shim', 8080, timeout=6)
    conn.request(method, path, json.dumps(body) if body is not None else None, {
        'Authorization': 'Bearer ' + key, 'anthropic-version': '2023-06-01',
        'Content-Type': 'application/json', 'x-shim-run-id': RUN, **(headers or {}),
    })
    response = conn.getresponse()
    data = response.read()
    conn.close()
    return response.status, data


for key in ('invalid', ADMIN):
    assert call('/v1/models', key=key)[0] == 401
    assert call('/v1/messages', 'POST', BODY, key)[0] == 401
status, data = call('/v1/models?limit=1000')
catalog = json.loads(data)
assert status == 200 and len(catalog['data']) == 3 and not catalog['has_more']
ids = [m['id'] for m in catalog['data']]
assert len(ids) == len(set(ids)) and all(m['type'] == 'model' for m in catalog['data'])
assert catalog['data'][0]['max_input_tokens'] == 1000000
assert [m['id'] for m in json.loads(call('/v1/models', key=RESTRICTED)[1])['data']] == [OLD]
assert call('/v1/models/' + NEW, key=RESTRICTED)[0] == 403
assert call('/v1/models/nonexistent')[0] == 404
assert json.loads(call('/v1/models/' + NEW)[1])['id'] == NEW
page = json.loads(call('/v1/models?limit=1')[1])
assert page['has_more'] and page['first_id'] == NEW and len(page['data']) == 1
next_page = json.loads(call('/v1/models?after_id=' + NEW)[1])
assert [m['id'] for m in next_page['data']] == ids[1:]
assert [m['id'] for m in json.loads(call('/v1/models?before_id=' + OLD)[1])['data']] == ids[:2]
before = json.loads(call('/v1/models?before_id=' + OLD + '&limit=1')[1])
assert [m['id'] for m in before['data']] == ids[1:2] and before['has_more']
for query in ('limit=0', 'limit=1001', 'after_id=missing', 'after_id=' + NEW + '&before_id=' + OLD):
    assert call('/v1/models?' + query)[0] == 400
assert call('/api/hello', 'HEAD')[0] == 204
assert call('/v1/messages?beta=true', 'POST', BODY)[0] == 200
status, data = call('/v1/messages', 'POST', {**BODY, 'stream': True})
assert status == 200 and b'event: message_stop' in data
assert call('/v1/messages/count_tokens', 'POST', {k: v for k, v in BODY.items() if k != 'max_tokens'})[0] == 200
assert call('/v1/messages', 'POST', {**BODY, 'model': 'claude-opus-4-6'})[0] == 200
assert call('/v1/messages', 'POST', BODY, headers={'x-api-key': KEY})[0] == 200
assert call('/v1/messages', 'POST', BODY, headers={'x-api-key': 'different'})[0] == 400
assert call('/v1/messages', 'POST', BODY, key=RESTRICTED)[0] == 403
assert call('/v1/messages', 'POST', {**BODY, 'model': 'claude-unknown'})[0] == 400
assert call('/v1/messages', 'POST', {**BODY, 'stream': 'true'})[0] == 400
assert call('/v1/messages', 'POST', {**BODY, 'anthropic_version': 'vertex-2023-10-16'})[0] == 400
assert call('/v1/messages', 'POST', BODY, headers={'anthropic-version': ''})[0] == 400

# Permission changes apply to the cached catalog without a proxy restart.
original = REGISTRY.read_text()
try:
    registry = json.loads(original)
    registry['keys'][0]['catalog_models'] = False
    registry['keys'][0]['models'] = [OLD]
    REGISTRY.write_text(json.dumps(registry))
    assert [m['id'] for m in json.loads(call('/v1/models')[1])['data']] == [OLD]
    assert call('/v1/messages', 'POST', BODY)[0] == 403
finally:
    REGISTRY.write_text(original)

# Cached metadata must still check that upstream credentials exist.
token = TOKEN.read_text()
try:
    TOKEN.write_text('')
    assert call('/v1/models')[0] == 503
    assert call('/v1/messages', 'POST', BODY)[0] == 503
finally:
    TOKEN.write_text(token)

for _ in range(100):
    conn = http.client.HTTPConnection('vertex-shim', 8081, timeout=5)
    conn.request('GET', '/api/private/v1/telemetry/requests?limit=100', headers={'Authorization': 'Bearer ' + ADMIN})
    page = json.loads(conn.getresponse().read())
    conn.close()
    rows = [r for r in page['items'] if r['run_id'] == RUN]
    if len(rows) == 5 and all(r['completion'] == 'complete' for r in rows): break
    time.sleep(0.05)
assert len(rows) == 5 and all(r['client_api'] == 'anthropic' for r in rows)
assert collections.Counter(r['operation'] for r in rows) == {'generation': 3, 'stream_generation': 1, 'count_tokens': 1}
assert all(r['context']['window_tokens'] == 1000000 for r in rows)
assert all(r['context']['window_source'] == 'upstream models API' for r in rows)
serialized = json.dumps(rows)
for secret in (KEY, ADMIN, RESTRICTED, token.strip(), 'private_prompt_marker', 'héllo_private_response'):
    assert secret not in serialized, 'secret in telemetry'
print(json.dumps({'status': 'passed', 'models': len(ids), 'requests': len(rows),
                  'checks': ['Messages routes', 'catalog discovery', 'pagination', 'per-key filtering',
                             'permission reload', 'credential validation despite cache', 'streaming', 'counting', 'context capacity']}))
