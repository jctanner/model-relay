"""Local reporting acceptance with an isolated synthetic provider; zero paid calls."""
import json
from pathlib import Path
import subprocess
import time
from telemetry_harness import Relay, ROOT


with Relay(keys=2, hold=0.2, events=3, gap=0) as relay:
    for index in range(150):
        status, _, _ = relay.generate('console', worker=index % 2, stream=False)
        assert status == 200
    health = relay.settle()
    assert health['dropped_requests'] == health['storage_errors'] == 0
    status, total = relay.admin_call('/telemetry/aggregate?run_id=console')
    assert status == 200 and total['request_count'] == 150
    assert total['generation_tokens']['output_tokens'] == 450
    status, page = relay.admin_call('/telemetry/requests?run_id=console&limit=100&order=desc')
    assert status == 200 and len(page['items']) == 100 and page['next_cursor'] is not None
    status, bounded = relay.admin_call('/telemetry/summary?run_id=console&limit=100&order=desc')
    assert status == 200 and bounded['generation_tokens']['output_tokens'] == 300
    for query in ['limit=100', 'cursor=0', 'order=desc', 'format=json', 'from=bad', 'unknown=true']:
        assert relay.admin_call('/telemetry/aggregate?' + query)[0] == 400
    assert relay.admin_call('/telemetry/aggregate?run_id=absent')[1]['request_count'] == 0
    assert relay.admin_call('/telemetry/aggregate?key_id=fixture-key-0')[1]['request_count'] == 75
    assert relay.admin_call('/telemetry/aggregate?model=telemetry-a')[1]['generation_tokens']['output_tokens'] == 225
    config = relay.directory / 'console.json'
    config.write_text(json.dumps({'base': relay.admin, 'admin_key': relay.admin_key,
        'source': str(ROOT / 'src/admin_ui/admin.js'), 'exercise_refresh': True}))
    config.chmod(0o600)
    observations = json.loads(subprocess.check_output(['node', str(ROOT / 'tests/telemetry_console_probe.js'), str(config)], text=True))
    assert [row['loaded_rows'] for row in observations] == [100, 150, 100, 100]
    assert all(row['matching_requests'] == 150 and row['output_tokens'] == 450 for row in observations)
    # Completed and cancelled streams are recorded distinctly, without double counting usage updates.
    assert relay.generate('streams')[0] == 200
    assert relay.generate('streams', cancel=True)[0] == 200
    time.sleep(0.3)
    relay.settle()
    rows = relay.records('streams')
    assert len(rows) == 2 and {row['completion'] for row in rows} == {'complete', 'cancelled'}
    assert sum(row['usage'].get('output_tokens', 0) for row in rows) == 3
    relay.restart()
    assert relay.admin_call('/telemetry/aggregate?run_id=console')[1]['request_count'] == 150
    print(json.dumps({'status': 'passed', 'paid_provider_calls': 0, 'console': observations,
        'checks': ['full filter totals', 'legacy page API', 'invalid aggregate pagination',
            'scope separation', 'refresh stability', 'reporting failure atomicity', 'stream cancellation', 'restart']}))
