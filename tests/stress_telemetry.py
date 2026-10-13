"""Reusable concurrent streaming/reporting stress test; synthetic loopback only.

Reports configured concurrency AND measured active upstream streams. The relay
can be pinned to one CPU; RSS is measured, not capped. No personal credentials,
remote providers or model executables are used. Any lost final record fails.
"""
import argparse
import concurrent.futures
from collections import Counter
import json
from pathlib import Path
import sqlite3
import threading
import time
from datetime import datetime, timedelta, timezone
from telemetry_harness import Relay, call


def percentile(values, fraction):
    return sorted(values)[min(int((len(values) - 1) * fraction), len(values) - 1)] if values else None


def rss_kib(pid):
    try:
        for line in Path(f'/proc/{pid}/status').read_text().splitlines():
            if line.startswith('VmRSS:'):
                return int(line.split()[1])
    except FileNotFoundError:
        pass
    return 0


def scenario(concurrency, waves, history, readers, one_cpu, payload_bytes, binary):
    run = f'stress-{concurrency}-{readers}'
    with Relay(history=history, one_cpu=one_cpu, payload_bytes=payload_bytes, binary=binary) as relay:
        stopped = threading.Event()
        read_times, read_statuses, peaks, read_errors = [], Counter(), {'pending': 0, 'rss_kib': 0}, []
        lock = threading.Lock()

        def observe(index):
            # Include retained history as well as currently active run/key scopes.
            recent = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat().replace('+00:00', 'Z')
            scopes = ['', '?project_id=vertex-shim-local', f'?run_id={run}',
                      f'?project_id=vertex-shim-local&from={recent}', f'?run_id={run}&key_id=fixture-key-0']
            iteration = index
            while not stopped.is_set():
                scope = scopes[iteration % len(scopes)]
                iteration += 1
                began = time.monotonic()
                try:
                    status, value = relay.admin_call('/telemetry/aggregate' + scope)
                    elapsed = time.monotonic() - began
                    with lock:
                        read_statuses[status] += 1
                        read_times.append(elapsed)
                        if status == 200:
                            assert value['scope'] == 'all matching requests'
                            assert value['provider_requests'] == 0
                            assert sum(model['tokens'] for model in value['models']) == sum(value['generation_tokens'].values())
                            health_counts = value['request_health']
                            assert sum(health_counts[key] for key in ['complete', 'failed_cancelled', 'in_progress']) == value['request_count']
                except Exception as error:
                    with lock:
                        read_errors.append(type(error).__name__)
                stopped.wait(0.2)

        threads = [threading.Thread(target=observe, args=(index,), daemon=True) for index in range(readers)]
        def monitor():
            while not stopped.is_set():
                try:
                    status, health = relay.admin_call('/health')
                    if status == 200:
                        with lock:
                            peaks['pending'] = max(peaks['pending'], health['pending_writes'])
                            peaks['rss_kib'] = max(peaks['rss_kib'], rss_kib(relay.relay.pid))
                except OSError:
                    pass
                stopped.wait(0.1)
        threads.append(threading.Thread(target=monitor, daemon=True))
        for thread in threads:
            thread.start()
        barrier = threading.Barrier(concurrency)

        def client(worker):
            barrier.wait(timeout=30)
            return [relay.generate(run, worker) for _ in range(waves)]

        began = time.monotonic()
        try:
            with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
                results = [result for batch in pool.map(client, range(concurrency)) for result in batch]
        finally:
            stopped.set()
            for thread in threads:
                thread.join(timeout=30)
        elapsed = time.monotonic() - began
        health = relay.settle()
        provider = call(relay.provider, '/fixture/status')[1]
        rows = relay.records(run)
        successful_ids = {request_id for status, request_id, _ in results if status == 200}
        actual_ids = {row['request_id'] for row in rows}
        # Independent raw-table verification, rather than trusting the new projection alone.
        with sqlite3.connect(f'file:{relay.database}?mode=ro', uri=True) as connection:
            raw_count, raw_tokens = connection.execute(
                "SELECT COUNT(*),SUM(json_extract(payload,'$.usage.output_tokens')) FROM requests WHERE json_extract(payload,'$.run_id')=?", [run]
            ).fetchone()
            retained_count = connection.execute('SELECT COUNT(*) FROM requests').fetchone()[0]
            raw_events = connection.execute('SELECT COUNT(*) FROM events e JOIN requests r USING(request_id) WHERE json_extract(r.payload,\'$.run_id\')=?', [run]).fetchone()[0]
        time.sleep(1.1)  # Allow the documented aggregate cache to expire.
        status, aggregate = relay.admin_call('/telemetry/aggregate?run_id=' + run)
        expected = concurrency * waves
        errors = []
        if len(successful_ids) != expected: errors.append('inference status/ID mismatch')
        if actual_ids != successful_ids: errors.append('missing or extra telemetry IDs')
        if raw_count != expected or raw_tokens != expected * relay.events: errors.append('raw usage mismatch')
        if retained_count != history + expected: errors.append('retained history mismatch')
        if raw_events != expected * (relay.events + 1): errors.append('missing intermediate usage events')
        if status != 200 or aggregate.get('request_count') != expected: errors.append('aggregate request mismatch')
        if aggregate.get('generation_tokens', {}).get('output_tokens') != expected * relay.events: errors.append('aggregate usage mismatch')
        all_totals = relay.admin_call('/telemetry/aggregate')[1]
        if all_totals.get('request_count') != retained_count or all_totals.get('generation_tokens', {}).get('output_tokens') != history * 3 + raw_tokens:
            errors.append('retained history aggregate mismatch')
        if any(row['completion'] != 'complete' or row['usage_status'] != 'complete' or not row['synthetic'] for row in rows): errors.append('incomplete final telemetry')
        if health['dropped_requests'] or health['storage_errors']: errors.append('dropped final requests or storage errors')
        if health['dropped_events']: errors.append('dropped intermediate usage events')
        if readers and (read_errors or any(code != 200 for code in read_statuses)): errors.append('reporting failures under load')
        if provider['peak_active'] < min(concurrency, 100): errors.append('insufficient measured concurrent streams')
        relay.restart()
        restored = relay.admin_call('/telemetry/aggregate?run_id=' + run)[1]
        if restored['request_count'] != expected: errors.append('restart count mismatch')
        return {'status': 'passed' if not errors else 'failed', 'errors': errors,
            'clients': concurrency, 'keys': len(relay.keys), 'waves': waves, 'requests': expected,
            'history_rows': history, 'reporting_readers': readers, 'one_cpu_affinity': one_cpu,
            'request_payload_bytes': payload_bytes,
            'measured_peak_active_streams': provider['peak_active'], 'elapsed_seconds': round(elapsed, 3),
            'requests_per_second': round(expected / elapsed, 2),
            'inference_statuses': dict(Counter(status for status, _, _ in results)),
            'inference_p95_ms': round(percentile([duration for _, _, duration in results], .95) * 1000, 2),
            'aggregate_statuses': dict(read_statuses),
            'aggregate_p95_ms': round(percentile(read_times, .95) * 1000, 2) if read_times else None,
            'peak_pending_writes': peaks['pending'], 'peak_rss_mib': round(peaks['rss_kib'] / 1024, 2),
            'health': health, 'raw_verified_requests': raw_count, 'raw_verified_output_tokens': raw_tokens,
            'raw_verified_events': raw_events, 'retained_requests': retained_count,
            'startup_migration_seconds': round(relay.startup_seconds, 3), 'paid_provider_calls': 0}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--concurrency', default='32,128,256')
    parser.add_argument('--waves', type=int, default=20)
    parser.add_argument('--history', type=int, default=100000)
    parser.add_argument('--readers', type=int, default=4)
    parser.add_argument('--one-cpu', action='store_true')
    parser.add_argument('--payload-bytes', type=int, default=131072)
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    clients = list(map(int, args.concurrency.split(',')))
    if not clients or any(value < 1 or value > 1024 for value in clients) or args.waves < 1 or args.history < 0 or args.readers < 1 or args.payload_bytes < 1:
        parser.error('Use 1–1024 clients, positive waves/readers/payload bytes and nonnegative history.')
    report = {'synthetic_only': True, 'paid_provider_calls': 0, 'scenarios': []}
    for concurrency in clients:
        for readers in [0, args.readers]:
            result = scenario(concurrency, args.waves, args.history, readers, args.one_cpu, args.payload_bytes, args.binary)
            report['scenarios'].append(result)
            print(json.dumps(result), flush=True)
            if args.output:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(report, indent=2) + '\n')
    if any(case['status'] != 'passed' for case in report['scenarios']):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
