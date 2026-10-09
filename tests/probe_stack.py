"""Offline REST acceptance probe, run inside the disposable test container."""
import concurrent.futures
import http.client
import json
import pathlib
import time
import uuid
import urllib.error
import urllib.request

BASE = "http://vertex-shim:8080"
ADMIN = "http://vertex-shim:8081/api/private/v1"
KEY = pathlib.Path("/run/secrets/proxy-api-key").read_text().strip()
ADMIN_KEY = pathlib.Path("/run/secrets/admin-api-key").read_text().strip()
MODEL = "claude-sonnet-4-5@20250929"
ROUTE = f"/v1/projects/local/locations/us-east5/publishers/anthropic/models/{MODEL}"
PAYLOAD = {"anthropic_version": "vertex-2023-10-16", "messages": [{"role": "user", "content": "private-test-prompt"}], "max_tokens": 100}
RUN_ID = "rest-probe-" + uuid.uuid4().hex


def request(url, body=None, key=KEY, header="Authorization"):
    headers = {"Content-Type": "application/json", "x-shim-run-id": RUN_ID}
    if key is not None:
        headers[header] = f"Bearer {key}" if header == "Authorization" else key
    req = urllib.request.Request(url, data=json.dumps(body).encode() if body is not None else None, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            text = response.read().decode()
            return response.status, json.loads(text)
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()


for key in (None, "invalid", ADMIN_KEY):
    assert request(BASE + ROUTE + ":rawPredict", PAYLOAD, key=key)[0] == 401
assert request(ADMIN + "/health", key=KEY)[0] == 401
assert request(BASE + "/api/private/v1/health", key=ADMIN_KEY)[0] == 404
assert request(ADMIN + "/telemetry/requests?unknown=bad", key=ADMIN_KEY)[0] == 400
assert request(ADMIN + "/telemetry/requests?limit=501", key=ADMIN_KEY)[0] == 400
assert request(ADMIN + "/telemetry/requests?from=bad", key=ADMIN_KEY)[0] == 400

status, message = request(BASE + ROUTE + ":rawPredict", PAYLOAD, header="x-api-key")
assert status == 200 and message["usage"]["output_tokens"] == 3
count_route = "/v1/projects/local/locations/us-east5/publishers/anthropic/models/count-tokens:rawPredict"
status, counted = request(BASE + count_route, {"anthropic_version": "vertex-2023-10-16", "model": MODEL, "messages": PAYLOAD["messages"]})
assert status == 200 and counted == {"input_tokens": 12}

# Incremental stream reception, including final usage. Closing a stream after
# the start event also exercises the cancellation guard and partial telemetry.
connection = http.client.HTTPConnection("vertex-shim", 8080, timeout=5)
connection.request("POST", ROUTE + ":streamRawPredict", json.dumps(PAYLOAD), {"Authorization": f"Bearer {KEY}", "x-shim-run-id": RUN_ID})
response = connection.getresponse()
assert response.status == 200 and response.getheader("Content-Type").startswith("text/event-stream")
started = time.monotonic()
assert response.readline().decode().startswith("event: message_start")
assert time.monotonic() - started < 1
response.close()
connection.close()

with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
    results = list(pool.map(lambda _: request(BASE + ROUTE + ":rawPredict", PAYLOAD), range(12)))
assert all(status == 200 for status, _ in results)
for _ in range(100):
    _, health = request(ADMIN + "/health", key=ADMIN_KEY)
    _, page = request(ADMIN + f"/telemetry/requests?run_id={RUN_ID}&limit=100", key=ADMIN_KEY)
    if health["pending_writes"] == 0 and all(r["completion"] != "in_progress" for r in page["items"]):
        break
    time.sleep(0.05)
assert health["storage_errors"] == 0 and health["dropped_records"] == 0, health
records = page["items"]
assert len(records) == 15, records
assert all(r["synthetic"] and r["backend"] == "mock" for r in records)
assert all(r["context"]["window_tokens"] is None for r in records)
assert any(r["operation"] == "count_tokens" for r in records)
assert any(r["completion"] == "cancelled" and r["usage_status"] == "partial" for r in records), records
assert "private-test-prompt" not in json.dumps(page)
assert KEY not in json.dumps(page) and ADMIN_KEY not in json.dumps(page)
_, first = request(ADMIN + f"/telemetry/requests?run_id={RUN_ID}&limit=2", key=ADMIN_KEY)
assert len(first["items"]) == 2 and first["next_cursor"] is not None
_, second = request(ADMIN + f"/telemetry/requests?run_id={RUN_ID}&limit=2&cursor={first['next_cursor']}", key=ADMIN_KEY)
assert not {r["request_id"] for r in first["items"]} & {r["request_id"] for r in second["items"]}
for record in records:
    assert request(ADMIN + "/telemetry/requests/" + record["request_id"], key=ADMIN_KEY)[0] == 200
assert request(ADMIN + "/telemetry/requests/missing", key=ADMIN_KEY)[0] == 404
_, totals = request(ADMIN + f"/telemetry/summary?run_id={RUN_ID}", key=ADMIN_KEY)
assert totals["counting_requests"] == 1
assert totals["generation_tokens"]["output_tokens"] == 39
print(json.dumps({"status": "passed", "requests": len(records), "telemetry_health": health}))
