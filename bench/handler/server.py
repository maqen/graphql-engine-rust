"""Action + event-trigger webhook used by the benchmark project.

Stdlib only so it runs in a slim python image with no install step.

  POST /price-quote   Hasura action handler for `trackPriceQuote`
  POST /events        event-trigger webhook; counts deliveries per trigger
  GET  /events/stats  {"<trigger name>": <deliveries>, ...}
  POST /events/reset  zero the counters
  GET  /healthz
"""

import json
import os
import threading
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_events = Counter()
_lock = threading.Lock()


def price_quote(payload):
    args = payload["input"]
    track_id = int(args["trackId"])
    quantity = int(args["quantity"])
    # Same pricing rule as the seed data: every third track costs 1.99.
    unit = 1.99 if track_id % 3 == 0 else 0.99
    return 200, {
        "trackId": track_id,
        "quantity": quantity,
        "total": round(unit * quantity, 2),
        "currency": "USD",
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # keep load runs quiet
        pass

    def _send(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"{}")

    def do_GET(self):
        if self.path == "/healthz":
            return self._send(200, {"ok": True})
        if self.path == "/events/stats":
            with _lock:
                return self._send(200, dict(_events))
        self._send(404, {"message": "not found"})

    def do_POST(self):
        try:
            body = self._body()
        except ValueError:
            return self._send(400, {"message": "invalid json"})
        if self.path == "/price-quote":
            return self._send(*price_quote(body))
        if self.path == "/events":
            name = body.get("trigger", {}).get("name", "unknown")
            with _lock:
                _events[name] += 1
            return self._send(200, {"ok": True})
        if self.path == "/events/reset":
            with _lock:
                _events.clear()
            return self._send(200, {"ok": True})
        self._send(404, {"message": "not found"})


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "3000"))
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
