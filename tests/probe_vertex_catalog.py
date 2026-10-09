"""Catalog-driven Vertex discovery/routing against the isolated OAuth provider."""
import http.client
import json
import pathlib
import time
import uuid

state = pathlib.Path('/probe')
admin = (state / 'admin-api-key').read_text().strip()
run = 'vertex-catalog-' + uuid.uuid4().hex


def call(port, path, headers=None, payload=None):
    connection = http.client.HTTPConnection('vertex-shim', port, timeout=10)
    try:
        connection.request('POST' if payload is not None else 'GET', path,
                           json.dumps(payload) if payload is not None else None,
                           {'Content-Type': 'application/json', 'x-shim-run-id': run, **(headers or {})})
        response = connection.getresponse()
        raw = response.read()
        try:
            value = json.loads(raw)
        except ValueError:
            value = raw.decode()
        return response.status, value
    finally:
        connection.close()


assert 'claude-opus-4-6' not in json.loads((state / 'config/models.json').read_text())
token = call(8082, '/computeMetadata/v1/instance/service-accounts/default/token',
             {'Metadata-Flavor': 'Google'})[1]['access_token']
headers = {'Authorization': 'Bearer ' + token}
status, catalog = call(8080, '/v1/models?client_api=vertex&limit=1000', headers)
assert status == 200 and not catalog['has_more']
ids = [model['id'] for model in catalog['data']]
assert {'claude-sonnet-4-6', 'claude-opus-4-6', 'claude-sonnet-4-5@20250929'}.issubset(ids)
assert len(ids) == len(set(ids))
page = call(8080, '/v1/models?client_api=vertex&limit=1', headers)[1]
assert page['has_more'] and page['first_id'] == ids[0]
next_page = call(8080, '/v1/models?client_api=vertex&after_id=' + ids[0], headers)[1]
assert [model['id'] for model in next_page['data']] == ids[1:]
route = '/v1/projects/local/locations/us-east5/publishers/anthropic/models/'
body = {'anthropic_version': 'vertex-2023-10-16', 'max_tokens': 100,
        'messages': [{'role': 'user', 'content': 'private_catalog_prompt'}]}
assert call(8080, route + 'claude-opus-4-6:rawPredict', headers, body)[0] == 200
status, sse = call(8080, route + 'claude-opus-4-6:streamRawPredict', headers, body)
assert status == 200 and 'event: message_stop' in sse
assert call(8080, route + 'count-tokens:rawPredict', headers,
            {**body, 'model': 'claude-opus-4-6'})[0] == 200
assert call(8080, route + 'claude-unknown:rawPredict', headers, body)[0] == 400
config = state / 'config/keys.json'
original = config.read_text()


def replace(value):
    temporary = config.with_suffix('.json.next')
    temporary.write_text(value)
    temporary.chmod(0o600)
    temporary.replace(config)


try:
    registry = json.loads(original)
    vertex = next(key for key in registry['keys'] if key['client_id'] == 'claude-code-vertex')
    vertex['vertex_catalog_models'] = False
    vertex['catalog_models'] = True
    replace(json.dumps(registry))
    assert 'claude-opus-4-6' not in [model['id'] for model in call(8080, '/v1/models?client_api=vertex', headers)[1]['data']]
    assert call(8080, route + 'claude-opus-4-6:rawPredict', headers, body)[0] == 403
    vertex['backends'] = []
    replace(json.dumps(registry))
    assert call(8080, '/v1/models?client_api=vertex', headers)[0] == 403
finally:
    replace(original)
admin_headers = {'Authorization': 'Bearer ' + admin}
for _ in range(40):
    status, telemetry = call(8081, '/api/private/v1/telemetry/requests?limit=100', admin_headers)
    rows = [row for row in telemetry['items'] if row['run_id'] == run]
    if len(rows) == 3:
        break
    time.sleep(0.05)
assert len(rows) == 3
assert all(row['resolved_model'] == 'claude-opus-4-6' and row['client_api'] == 'vertex'
           and row['auth_scheme'] == 'mock-adc' and row['upstream_auth'] == 'claude-oauth'
           and row['context']['window_tokens'] == 1000000
           and row['context']['window_source'] == 'upstream models API' for row in rows)
assert {row['operation'] for row in rows} == {'generation', 'stream_generation', 'count_tokens'}
assert all(secret not in json.dumps(telemetry) for secret in (token, admin, 'private_catalog_prompt'))
print('PASS: catalog-only Opus via Vertex generation/SSE/counting, pagination, grants, backend denial, window metadata, and private telemetry')
