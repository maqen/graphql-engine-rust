# Adapted from hasura/graphql-engine server/tests-py/auth_webhook_server.py (v2.51.0), the
# auth webhook of test_auth_webhook_cookie.py: python3 cookie_auth.py <port>
# Upstream's POST branch returns the request headers as response headers (a bug its tests
# never reach, they use GET mode); here POST answers like GET.
"""
A request with a non-empty Cookie header is admin; every `response-set-cookie*` request
header comes back as a Set-Cookie response header. Anything else is 401.
"""
import http.server
import json
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    def answer(self, headers):
        headers = {k.lower(): v for k, v in headers.items()}
        if not headers.get("cookie"):
            self.send_response(401)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = json.dumps({"x-hasura-role": "admin"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        for k, v in headers.items():
            if "response-set-cookie" in k:
                self.send_header("Set-Cookie", v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.answer(dict(self.headers.items()))

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.answer(json.loads(body or b"{}").get("headers", {}))


if __name__ == "__main__":
    http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
