"""Synthetic Vertex gateway for the native Claude Code compatibility probe.

Contract sources (documentation-derived, not a recorded provider response):
https://platform.claude.com/docs/en/api/messages/create
https://platform.claude.com/docs/en/build-with-claude/streaming
https://code.claude.com/docs/en/google-vertex-ai
Retrieved 2026-10-08. Client version is recorded by the probe launcher.
No request content or credential values are written to logs.
"""

import hmac
import json
import pathlib
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KEY = pathlib.Path("/run/secrets/proxy-api-key").read_text().strip()
ROUTE = re.compile(
    r"^/v1/projects/[^/]+/locations/[^/]+/publishers/anthropic/models/"
    r"([^/]+):(rawPredict|streamRawPredict)$"
)


class Gateway(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_json(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            self.send_json(200, {"status": "ready", "synthetic": True})
        else:
            self.send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": "Unknown route"}})

    def do_POST(self):
        match = ROUTE.fullmatch(self.path)
        authorized = hmac.compare_digest(
            self.headers.get("Authorization", ""), f"Bearer {KEY}"
        )
        print(json.dumps({"path": self.path, "authorized": authorized}), flush=True)
        if not authorized:
            self.send_json(401, {"type": "error", "error": {"type": "authentication_error", "message": "Invalid local key"}})
            return
        if not match:
            self.send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": "Expected a Vertex route"}})
            return
        length = int(self.headers.get("Content-Length", "0"))
        if length > 8 * 1024 * 1024:
            self.send_json(413, {"type": "error", "error": {"type": "request_too_large", "message": "Too large"}})
            return
        body = json.loads(self.rfile.read(length))
        model, operation = match.groups()
        print(json.dumps({
            "anthropic_version": body.get("anthropic_version"),
            "stream": body.get("stream"),
            "model_in_body": "model" in body,
            "message_count": len(body.get("messages", [])),
        }), flush=True)
        if model == "count-tokens":
            self.send_json(200, {"input_tokens": 12})
            return
        usage = {"input_tokens": 12, "output_tokens": 3,
                 "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
        message = {"id": "msg_vertex_shim_fixture", "type": "message", "role": "assistant",
                   "model": model, "content": [{"type": "text", "text": "Greetings from vertex-shim."}],
                   "stop_reason": "end_turn", "stop_sequence": None, "usage": usage}
        if operation == "rawPredict":
            self.send_json(200, message)
            return
        start = {**message, "content": [], "stop_reason": None,
                 "usage": {**usage, "output_tokens": 0}}
        events = [
            {"type": "message_start", "message": start},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Greetings from vertex-shim."}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 3}},
            {"type": "message_stop"},
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for event in events:
            self.wfile.write(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode())
            self.wfile.flush()


ThreadingHTTPServer(("0.0.0.0", 8080), Gateway).serve_forever()
