"""Exercise two turns in one native process; credentials never enter host argv."""
import json
import pathlib
import queue
import subprocess
import sys
import threading
import time

container, artifact, delay = sys.argv[1], pathlib.Path(sys.argv[2]), float(sys.argv[3])
artifact.parent.mkdir(parents=True, exist_ok=True)
lines = queue.Queue()
with artifact.with_suffix('.stderr').open('w') as stderr:
    process = subprocess.Popen([
        'podman', 'exec', '--interactive', container, 'env', 'CLAUDE_CODE_MAX_RETRIES=0',
        'claude-proxy', '-p', '--input-format', 'stream-json', '--output-format',
        'stream-json', '--verbose', '--tools', '', '--max-turns', '1',
    ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True)

    def receive():
        for line in process.stdout:
            lines.put(line)
        lines.put(None)

    threading.Thread(target=receive, daemon=True).start()
    results = []
    try:
        with artifact.open('w') as output:
            for turn in range(2):
                if turn:
                    time.sleep(delay)
                process.stdin.write(json.dumps({'type': 'user', 'message': {
                    'role': 'user', 'content': 'Reply with a short greeting.'}}) + '\n')
                process.stdin.flush()
                deadline = time.monotonic() + 60
                while time.monotonic() < deadline:
                    line = lines.get(timeout=max(0.1, deadline - time.monotonic()))
                    if line is None:
                        raise RuntimeError('Native CLI exited before completing both turns')
                    output.write(line)
                    output.flush()
                    record = json.loads(line)
                    if record.get('type') == 'result':
                        assert not record.get('is_error'), record.get('subtype')
                        assert 'Greetings from vertex-shim.' in record.get('result', '')
                        results.append(record)
                        break
                else:
                    raise TimeoutError('Native turn timed out')
        process.stdin.close()
        assert process.wait(timeout=10) == 0
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
assert len(results) == 2
assert results[0]['session_id'] == results[1]['session_id']
print('PASS: two native Claude turns in one process through Google ADC')
