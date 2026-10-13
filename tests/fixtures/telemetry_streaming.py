"""Loopback-only streaming provider for telemetry tests; no external inference."""
import asyncio
import hmac
import json
from pathlib import Path
import sys
import uuid


async def main():
    config = json.loads(Path(sys.argv[1]).read_text())
    counters = {'requests': 0, 'active': 0, 'peak_active': 0, 'completed': 0}

    async def handle(reader, writer):
        active = False
        try:
            head = await reader.readuntil(b'\r\n\r\n')
            lines = head.decode().split('\r\n')
            method, path, _ = lines[0].split(' ')
            headers = dict(line.lower().split(': ', 1) for line in lines[1:] if ': ' in line)
            body = json.loads(await reader.readexactly(int(headers.get('content-length', '0'))) or b'{}')

            async def reply(status, value):
                encoded = json.dumps(value).encode()
                writer.write(f'HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {len(encoded)}\r\nConnection: close\r\n\r\n'.encode() + encoded)
                await writer.drain()

            if method == 'GET' and path == '/fixture/status':
                await reply(200, counters)
                return
            if not hmac.compare_digest(headers.get('authorization', ''), 'bearer ' + config['token'].lower()):
                await reply(401, {'error': {'type': 'authentication_error', 'message': 'Fixture credentials rejected'}})
                return
            if method == 'GET' and path.startswith('/v1/models'):
                await reply(200, {'data': [{'id': model, 'type': 'model', 'display_name': model,
                                          'created_at': '2026-01-01T00:00:00Z', 'max_input_tokens': 200000}
                                         for model in config['models']], 'has_more': False})
                return
            if method != 'POST' or path != '/v1/messages':
                await reply(404, {})
                return
            counters['requests'] += 1
            counters['active'] += 1
            counters['peak_active'] = max(counters['peak_active'], counters['active'])
            active = True
            usage = {'input_tokens': 12, 'output_tokens': config['events'],
                     'cache_read_input_tokens': 8, 'cache_creation_input_tokens': 5}
            message = {'id': 'msg_' + uuid.uuid4().hex, 'type': 'message', 'role': 'assistant',
                       'model': body['model'], 'content': [{'type': 'text', 'text': 'Synthetic fixture.'}],
                       'stop_reason': 'end_turn', 'stop_sequence': None, 'usage': usage}
            if not body.get('stream'):
                await reply(200, message)
            else:
                writer.write(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n')

                async def event(kind, value):
                    writer.write(f'event: {kind}\ndata: {json.dumps(value)}\n\n'.encode())
                    await writer.drain()

                await event('message_start', {'type': 'message_start', 'message': {
                    **message, 'content': [], 'stop_reason': None, 'usage': {**usage, 'output_tokens': 0}}})
                # Hold every stream open long enough to observe actual overlap.
                await asyncio.sleep(config['hold_seconds'])
                for count in range(1, config['events'] + 1):
                    await event('message_delta', {'type': 'message_delta',
                        'delta': {'stop_reason': 'end_turn' if count == config['events'] else None},
                        'usage': {'output_tokens': count}})
                    await asyncio.sleep(config['event_gap_seconds'])
                await event('message_stop', {'type': 'message_stop'})
            counters['completed'] += 1
        except (ConnectionError, asyncio.IncompleteReadError):
            pass
        finally:
            if active:
                counters['active'] -= 1
            writer.close()
            try:
                await writer.wait_closed()
            except ConnectionError:
                pass

    server = await asyncio.start_server(handle, '127.0.0.1', config['port'])
    async with server:
        await server.serve_forever()


if __name__ == '__main__':
    asyncio.run(main())
