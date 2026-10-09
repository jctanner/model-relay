"""HTTP acceptance for the synthetic metadata issuer and existing key auth."""
import concurrent.futures
import http.client
import json
import pathlib
import sys
import time

state = pathlib.Path('/probe')
model = 'claude-sonnet-4-5@20250929'
route = f'/v1/projects/vertex-shim-local/locations/us-east5/publishers/anthropic/models/{model}:rawPredict'
token_path = '/computeMetadata/v1/instance/service-accounts/default/token'
body = {'anthropic_version': 'vertex-2023-10-16', 'max_tokens': 16,
        'messages': [{'role': 'user', 'content': 'private_mock_adc_prompt'}]}
key = (state / 'proxy-api-key').read_text().strip()
admin = (state / 'admin-api-key').read_text().strip()
minted = []


def request(port, path, method='GET', headers=None, payload=None):
    connection = http.client.HTTPConnection('vertex-shim', port, timeout=10)
    try:
        connection.request(method, path, json.dumps(payload) if payload is not None else None,
                           headers or {})
        response = connection.getresponse()
        raw = response.read()
        try:
            value = json.loads(raw)
        except ValueError:
            value = raw.decode()
        return response.status, dict(response.getheaders()), value
    finally:
        connection.close()


def issue():
    status, headers, value = request(8082, token_path, headers={'Metadata-Flavor': 'Google'})
    assert status == 200, status
    assert headers['metadata-flavor'] == 'Google' and headers['cache-control'] == 'no-store'
    assert value['token_type'] == 'Bearer' and value['expires_in'] > 0
    token = value['access_token']
    assert token.startswith('mock-gcp-')
    minted.append(token)
    return token


def predict(token, expected=200, path=route):
    status, _, _ = request(8080, path, 'POST', {'Authorization': 'Bearer ' + token,
                                              'Content-Type': 'application/json'}, body)
    assert status == expected, (status, expected)


if sys.argv[1] == 'expiry':
    token = issue()
    predict(token)
    ttl = request(8082, '/health')[2]['expires_in']
    assert ttl <= 5
    time.sleep(ttl + 0.2)
    predict(token, 401)
    renewed = issue()
    assert token != renewed
    predict(renewed)
    print('PASS: expired token rejected; renewed token accepted')
else:
    assert request(8082, token_path)[0] == 403
    assert request(8082, token_path, headers={'Metadata-Flavor': 'Wrong'})[0] == 403
    assert request(8082, token_path, headers={'Metadata-Flavor': 'Google', 'X-Forwarded-For': '127.0.0.1'})[0] == 403
    assert request(8082, token_path + '?scopes=invalid', headers={'Metadata-Flavor': 'Google'})[0] == 403
    assert request(8082, '/computeMetadata/v1/unknown', headers={'Metadata-Flavor': 'Google'})[0] == 404
    assert request(8080, token_path, headers={'Metadata-Flavor': 'Google'})[0] == 404
    assert request(8081, token_path, headers={'Metadata-Flavor': 'Google'})[0] == 404
    assert request(8082, '/api/private/v1/telemetry/requests')[0] == 404
    assert request(8082, '/v1/messages', 'POST', payload=body)[0] == 404
    assert request(8082, '/computeMetadata/v1/project/project-id', headers={'Metadata-Flavor': 'Google'})[2] == 'vertex-shim-local'
    first, second = issue(), issue()
    assert first != second
    predict(first)
    predict(second)
    predict(first)  # An existing token can be reused; issuing another does not revoke it.
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        list(pool.map(predict, [first, second] * 3))
    predict('mock-gcp-' + '0' * 64, 401)
    predict('mock-gcp-malformed', 401)
    predict(first, 403, route.replace(model, 'unknown-model'))
    assert request(8081, '/api/private/v1/auth/status', headers={'Authorization': 'Bearer ' + first})[0] == 401
    assert request(8080, route, 'POST', {'x-api-key': first, 'Content-Type': 'application/json'}, body)[0] == 401
    assert request(8080, route, 'POST', {'Authorization': 'Bearer ' + first,
                                      'x-api-key': key, 'Content-Type': 'application/json'}, body)[0] == 400
    config = state / 'config/keys.json'
    original = config.read_text()

    def update(field, value):
        registry = json.loads(original)
        registry['keys'][0][field] = value
        config.write_text(json.dumps(registry))

    try:
        ambiguous = json.loads(original)
        ambiguous['keys'].append(dict(ambiguous['keys'][0]))
        config.write_text(json.dumps(ambiguous))
        predict(first, 401)
        assert request(8082, token_path, headers={'Metadata-Flavor': 'Google'})[0] == 403
        config.write_text(original)
        update('mock_adc', False)
        predict(first, 401)
        assert request(8082, token_path, headers={'Metadata-Flavor': 'Google'})[0] == 403
        config.write_text(original)
        update('models', [])
        predict(first, 403)
        update('backends', [])
        predict(first, 403)
        import hashlib
        update('sha256', hashlib.sha256(b'rotated-synthetic-key').hexdigest())
        predict(first, 401)
        rotated = issue()
        predict(rotated)
        config.write_text(original)
        predict(rotated, 401)
        predict(first)
    finally:
        config.write_text(original)
    predict(key)  # Existing opaque API-key clients continue working alongside ADC.
    time.sleep(0.3)
    status, _, records = request(8081, '/api/private/v1/telemetry/requests?limit=500',
                                 headers={'Authorization': 'Bearer ' + admin})
    assert status == 200
    schemes = {r['auth_scheme'] for r in records['items']}
    assert {'mock-adc', 'bearer'} <= schemes
    assert all(r['client_id'] == 'claude-code-dev' and r['key_id'] == 'dev-inference'
               for r in records['items'])
    status, _, auth = request(8081, '/api/private/v1/auth/status', headers={'Authorization': 'Bearer ' + admin})
    assert status == 200 and auth['mock_adc']['issued_tokens'] >= 3
    public = json.dumps([records, auth])
    logs = (state / 'stack.log').read_text()
    for secret in [*minted, key, admin, 'private_mock_adc_prompt']:
        assert secret not in public and secret not in logs, 'credential/content leaked'
    assert 'mock-gcp-' not in logs
    print('PASS: metadata isolation, concurrent/reusable tokens, grants, revocation, key rotation, telemetry, privacy, and API-key compatibility')
