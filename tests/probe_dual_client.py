"""Exercise mixed client APIs/auth schemes and independent revocation on one proxy."""
import concurrent.futures
import http.client
import json
import pathlib
import time

state = pathlib.Path('/probe')
key = (state / 'proxy-api-key').read_text().strip()
admin = (state / 'admin-api-key').read_text().strip()
vertex_model = 'claude-sonnet-4-5@20250929'
messages_model = 'claude-sonnet-4-5-20250929'
vertex_route = f'/v1/projects/vertex-shim-local/locations/us-east5/publishers/anthropic/models/{vertex_model}:rawPredict'
messages_body = {'model': messages_model, 'max_tokens': 16,
                 'messages': [{'role': 'user', 'content': 'dual_private_prompt'}]}
vertex_body = {k: v for k, v in messages_body.items() if k != 'model'}
vertex_body['anthropic_version'] = 'vertex-2023-10-16'


def call(port, path, headers, body=None):
    connection = http.client.HTTPConnection('vertex-shim', port, timeout=10)
    try:
        connection.request('POST' if body is not None else 'GET', path,
                           json.dumps(body) if body is not None else None,
                           {**headers, 'Content-Type': 'application/json', 'anthropic-version': '2023-06-01'})
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


status, issued = call(8082, '/computeMetadata/v1/instance/service-accounts/default/token',
                      {'Metadata-Flavor': 'Google'})
assert status == 200
token = issued['access_token']
api_key = {'x-api-key': key}
bearer_key = {'Authorization': 'Bearer ' + key}
adc = {'Authorization': 'Bearer ' + token}
admin_headers = {'Authorization': 'Bearer ' + admin}
status, deployments = call(8080, '/v1/models?client_api=vertex&limit=1000', adc)
assert status == 200 and not deployments['has_more']
assert {model['id'] for model in deployments['data']} == {vertex_model, 'claude-haiku-5-5', 'claude-opus-4-6'}
assert call(8080, '/v1/models?client_api=invalid', adc)[0] == 400
# Both APIs continue to accept both ordinary key headers; ADC coexists.
cases = [(messages_body, '/v1/messages', api_key),
         (messages_body, '/v1/messages', bearer_key),
         (vertex_body, vertex_route, api_key), (vertex_body, vertex_route, bearer_key),
         (vertex_body, vertex_route, adc), (messages_body, '/v1/messages', adc)]
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    replies = list(pool.map(lambda args: call(8080, args[1], args[2], args[0]), cases * 2))
assert all(status == 200 for status, _ in replies), replies
assert call(8081, '/api/private/v1/telemetry/requests', api_key)[0] == 401
assert call(8081, '/api/private/v1/telemetry/requests', adc)[0] == 401
assert call(8080, '/v1/messages', admin_headers, messages_body)[0] == 401
config = state / 'config/keys.json'
original = config.read_text()
registry = json.loads(original)
assert len({entry['client_id'] for entry in registry['keys']}) == 2


def replace(value):
    temporary = config.with_suffix('.json.next')
    temporary.write_text(value)
    temporary.chmod(0o600)
    temporary.replace(config)


try:
    restricted = json.loads(original)
    for entry in restricted['keys']:
        if entry['client_id'] == 'claude-code-vertex':
            entry['models'] = [vertex_model]
            entry['catalog_models'] = True
            entry['vertex_catalog_models'] = False
    replace(json.dumps(restricted))
    status, filtered = call(8080, '/v1/models?client_api=vertex&limit=1000', adc)
    assert status == 200 and [model['id'] for model in filtered['data']] == [vertex_model]
    assert call(8080, vertex_route.replace(vertex_model, 'claude-opus-4-6'), adc, vertex_body)[0] == 403
    # Broad Messages catalog permission must not broaden Vertex deployment grants.
    restricted['keys'][-1]['models'] = []
    replace(json.dumps(restricted))
    assert call(8080, '/v1/models?client_api=vertex&limit=1000', adc)[1]['data'] == []
    revoked = json.loads(original)
    revoked['keys'] = [entry for entry in revoked['keys'] if entry['client_id'] != 'claude-code-vertex']
    replace(json.dumps(revoked))
    assert call(8080, vertex_route, adc, vertex_body)[0] == 401
    assert call(8080, '/v1/messages', api_key, messages_body)[0] == 200
    revoked = json.loads(original)
    revoked['keys'] = [entry for entry in revoked['keys'] if entry['client_id'] == 'claude-code-vertex']
    replace(json.dumps(revoked))
    assert call(8080, '/v1/messages', api_key, messages_body)[0] == 401
    assert call(8080, vertex_route, adc, vertex_body)[0] == 200
finally:
    replace(original)
for _ in range(30):
    status, data = call(8081, '/api/private/v1/telemetry/requests?limit=100', admin_headers)
    rows = data['items']
    if len(rows) >= 16:
        break
    time.sleep(0.1)
assert status == 200 and len(rows) >= 16, rows
assert len({row['request_id'] for row in rows}) == len(rows)
assert any(row['client_id'] == 'claude-code-dev' and row['client_api'] == 'anthropic'
           and row['auth_scheme'] == 'x-api-key' and row['operation'] == 'stream_generation' for row in rows)
assert any(row['client_id'] == 'claude-code-vertex' and row['client_api'] == 'vertex'
           and row['auth_scheme'] == 'mock-adc' and row['operation'] == 'stream_generation' for row in rows)
assert all(row['http_status'] == 200 and row['usage_status'] == 'complete' for row in rows)
serialized = json.dumps(data)
assert all(secret not in serialized for secret in (key, admin, token, 'dual_private_prompt'))
print('PASS: simultaneous Messages/Vertex, x-api-key/Bearer/ADC, distinct telemetry identities, independent revocation, admin separation')
