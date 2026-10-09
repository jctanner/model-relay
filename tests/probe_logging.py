"""Check actual container stdout against terminal REST telemetry and secrets."""
import argparse
import collections
import json
import pathlib

parser = argparse.ArgumentParser()
parser.add_argument('--log', type=pathlib.Path, required=True)
parser.add_argument('--records', type=pathlib.Path, required=True)
parser.add_argument('--secret-file', type=pathlib.Path, action='append', default=[])
args = parser.parse_args()
text = args.log.read_text()
logs = [json.loads(line) for line in text.splitlines() if line.startswith('{')]
assert logs, 'no JSON stdout logs'
for entry in logs:
    assert all(field in entry for field in ('timestamp', 'level', 'event', 'version', 'build_id'))
events = collections.Counter(entry['event'] for entry in logs)
assert events['startup'] >= 2 and events['shutdown_started'] >= 1 and events['shutdown'] >= 1, events
assert events['request_rejected'] >= 2, events
assert all(300 <= entry['http_status'] < 600 for entry in logs if entry['event'] == 'request_rejected')
rows = json.loads(args.records.read_text())['items']
completed = [entry for entry in logs if entry['event'] == 'request_completed']
assert collections.Counter(entry['request_id'] for entry in completed) == collections.Counter(row['request_id'] for row in rows)
by_id = {entry['request_id']: entry for entry in completed}
assert any(row['completion'] == 'cancelled' for row in rows)
assert any(row['operation'] == 'stream_generation' and row['usage_status'] == 'complete' for row in rows)
for row in rows:
    entry = by_id[row['request_id']]
    for field in ('http_status', 'completion', 'usage_status', 'latency_ms', 'error_type', 'observation_error'):
        assert entry[field] == row[field], (field, entry)
    assert entry['usage'] == row['usage'], entry
    assert entry['context']['window_tokens'] == row['context']['window_tokens']
    assert entry['context']['total_input_tokens'] == row['context']['total_input_tokens']
failures = collections.Counter(entry['request_id'] for entry in logs if entry['event'] == 'upstream_failure')
assert failures == collections.Counter(row['request_id'] for row in rows if row['completion'] == 'failed')
for secret in [path.read_text().strip() for path in args.secret_file] + [
    'synthetic-upstream-', 'synthetic-rotated-', 'private_prompt_marker',
    'private_tool_argument', 'héllo_private_response', 'private provider error',
    'private error details', 'must_not_forward',
]:
    assert secret and secret not in text, 'content or credential entered stdout logs'
print(json.dumps({'status': 'passed', 'completion_logs': len(completed),
                  'checks': ['stdout JSON', 'lifecycle', 'streaming', 'cancellation',
                             'upstream failures', 'rejections', 'token counts', 'privacy']}))
