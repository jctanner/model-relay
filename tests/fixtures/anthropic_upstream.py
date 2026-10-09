"""Synthetic Messages API; no credential values or user content in logs.

Contract: https://platform.claude.com/docs/en/api/messages/create
https://platform.claude.com/docs/en/build-with-claude/streaming
https://platform.claude.com/docs/en/manage-claude/authentication
Documentation-derived fixtures, retrieved 2026-10-08; no recorded live traffic.
"""
import collections
import hmac
import json
import pathlib
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

counts = collections.Counter()
lock = threading.Lock()


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value, headers=None):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("request-id", "req_fixture")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.startswith('/v1/models'):
            token = pathlib.Path('/run/secrets/upstream-token').read_text().strip()
            valid = (hmac.compare_digest(self.headers.get('Authorization', ''), 'Bearer ' + token)
                     and not self.headers.get('x-api-key')
                     and self.headers.get('anthropic-version') == '2023-06-01'
                     and self.headers.get('anthropic-workspace-id') == 'wrkspc_fixture')
            with lock:
                counts['model_list_requests'] += 1
                if not valid: counts['contract_failures'] += 1
            if not valid:
                self.reply(401, {'error': {'type': 'authentication_error', 'message': 'private provider error'}})
                return
            data = [{'type': 'model', 'id': model, 'display_name': label, 'line': family,
                     'created_at': '2026-01-01T00:00:00Z', 'max_input_tokens': window, 'lifecycle': 'active'}
                    for model, label, family, window in (
                        ('claude-sonnet-4-6', 'Claude Sonnet 4.6', 'sonnet', 1000000),
                        ('claude-opus-4-6', 'Claude Opus 4.6', 'opus', 1000000),
                        ('claude-sonnet-4-5-20250929', 'Claude Sonnet 4.5', 'sonnet', 200000))]
            self.reply(200, {'data': data, 'has_more': False, 'first_id': data[0]['id'], 'last_id': data[-1]['id']})
            return
        with lock:
            self.reply(200, dict(counts))

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        mode = body.get("metadata", {}).get("user_id", "native")
        token = pathlib.Path("/run/secrets/upstream-token").read_text().strip()
        with lock:
            counts[mode] += 1
        valid = (
            hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + token)
            and not any(self.headers.get(k) for k in ("x-api-key", "x-shim-run-id", "x-client-secret"))
            and self.headers.get("anthropic-version") == "2023-06-01"
            and self.headers.get("User-Agent", "").startswith("vertex-shim/")
            and self.headers.get("anthropic-workspace-id") == "wrkspc_fixture"
            and body.get("model") in ('fixture-sonnet', 'claude-sonnet-4-6', 'claude-opus-4-6', 'claude-sonnet-4-5-20250929')
            and "anthropic_version" not in body
            and self.path in ("/v1/messages", "/v1/messages/count_tokens")
        )
        if mode == "tool":
            valid &= (body.get("tools", [{}])[0].get("name") == "fixture_tool"
                      and self.headers.get("anthropic-beta") == "fixture-beta")
        if not valid:
            with lock:
                counts["contract_failures"] += 1
            self.reply(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "Fixture contract failed"}})
            return
        if mode in ("rate", "server", "redirect"):
            status = {"rate": 429, "server": 500, "redirect": 307}[mode]
            self.reply(status, {"type": "error", "error": {"type": "rate_limit_error", "message": "private provider error"}},
                       {"Retry-After": "7", "Location": "http://upstream-fixture:8090/v1/messages"})
            return
        if self.path.endswith("count_tokens"):
            assert "stream" not in body
            self.reply(200, {"input_tokens": 12})
            return
        usage = {"input_tokens": 12, "output_tokens": 3, "cache_read_input_tokens": 8,
                 "cache_creation_input_tokens": 5, "cache_creation": {"ephemeral_5m_input_tokens": 5}}
        message = {"id": "msg_fixture", "type": "message", "role": "assistant", "model": body['model'],
                   "content": [{"type": "text", "text": "Greetings from vertex-shim."}],
                   "usage": usage, "stop_reason": "end_turn", "stop_sequence": None}
        if mode == "tool":
            message["content"] = [{"type": "tool_use", "id": "toolu_fixture", "name": "fixture_tool", "input": {"secret": "private_tool_argument"}}]
            message["stop_reason"] = "tool_use"
        if not body.get("stream"):
            self.reply(200, message)
            return
        start = {**message, "content": [], "stop_reason": None, "usage": {**usage, "output_tokens": 0}}
        events = [
            {"type": "message_start", "message": start},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "héllo_private_response"}},
            *[{"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": n}} for n in (1, 2, 3)],
            {"type": "content_block_stop", "index": 0},
            {"type": "message_stop"},
        ]
        if mode in ("truncated", "cancel", "timeout"):
            events = events[:3]
        if mode == "error":
            events = events[:3] + [{"type": "error", "error": {"type": "overloaded_error", "message": "private error details"}}]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("request-id", "req_fixture")
        if mode == "gzip":
            self.send_header("Content-Encoding", "gzip")
        self.end_headers()
        compressor = zlib.compressobj(wbits=31) if mode == "gzip" else None
        try:
            for index, event in enumerate(events):
                wire = f"event: {event['type']}\r\ndata: {json.dumps(event, ensure_ascii=False)}\r\n\r\n".encode()
                if mode == "malformed" and index == 2:
                    wire = b"data: invalid json\n\n" + wire
                if compressor:
                    wire = compressor.compress(wire) + compressor.flush(zlib.Z_SYNC_FLUSH)
                # Split bytes inside UTF-8 and JSON rather than at event boundaries.
                for offset in range(0, len(wire), 7):
                    self.wfile.write(wire[offset:offset + 7])
                self.wfile.flush()
                if index == 0:
                    time.sleep(0.6 if mode not in ("cancel", "timeout") else 3)
            if compressor:
                self.wfile.write(compressor.flush(zlib.Z_FINISH))
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 8090), Upstream).serve_forever()
