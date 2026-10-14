# Adapted from QueryEchoWebhookServer in hasura/graphql-engine server/tests-py/
# test_webhook_request_context.py (v2.51.0): python3 query_echo.py <port>
# Header names are matched case-insensitively: our engine forwards them Title-Case, V2 as
# the client sent them.
"""
POST-mode auth webhook: the session is role `user` with x-hasura-user-id taken from the
client's `auth-user-id` header. Each request body is printed to the helper log.
"""
import http.server
import json
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        headers = {k.lower(): v for k, v in req.get("headers", {}).items()}
        print(json.dumps(req), flush=True)
        body = json.dumps({"x-hasura-role": "user", "x-hasura-user-id": headers.get("auth-user-id", "")}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
