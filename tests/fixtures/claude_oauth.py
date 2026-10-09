"""Synthetic subscription-bearer fixture; authentication calls are forbidden."""
from http.server import ThreadingHTTPServer
from anthropic_upstream import Upstream, counts, lock


class OAuthProvider(Upstream):
    def do_POST(self):
        if self.path == '/v1/oauth/token':
            with lock:
                counts['refresh_requests'] += 1
            self.reply(400, {'error': 'invalid_grant'})
            return
        if 'oauth-2025-04-20' not in self.headers.get('anthropic-beta', '').split(','):
            with lock:
                counts['contract_failures'] += 1
        return super().do_POST()


ThreadingHTTPServer(('0.0.0.0', 8090), OAuthProvider).serve_forever()
