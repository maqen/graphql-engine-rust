"""Action + event-trigger webhook used by the benchmark project.

Stdlib only so it runs in a slim python image with no install step.

  POST /price-quote              action handler for `trackPriceQuote`
  POST /events/<engine>          event-trigger webhook; each engine posts to
                                 its own namespace so a backlog in one never
                                 shows up in the other's counts
  GET  /events/stats             {"<engine>": {"<trigger>": <deliveries>}}
  GET  /events/payloads/<engine> {"<trigger>": [last payloads, oldest first]}
  POST /events/reset[/<engine>]  clear counters and payloads
  GET  /healthz

Being a GIL-bound Python server, the action handler caps action throughput
for every engine alike; load numbers for the action case measure the
handler as much as the engine.
"""

import json
import os
import threading
from collections import Counter, defaultdict, deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_KEEP_PAYLOADS = 20
_events: dict[str, Counter] = defaultdict(Counter)
_payloads: dict[str, dict[str, deque]] = defaultdict(lambda: defaultdict(lambda: deque(maxlen=_KEEP_PAYLOADS)))
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
                return self._send(200, {e: dict(c) for e, c in _events.items() if c})
        if self.path.startswith("/events/payloads/"):
            engine = self.path.rsplit("/", 1)[1]
            with _lock:
                return self._send(200, {t: list(q) for t, q in _payloads.get(engine, {}).items()})
        self._send(404, {"message": "not found"})

    def do_POST(self):
        try:
            body = self._body()
        except ValueError:
            return self._send(400, {"message": "invalid json"})
        if self.path == "/price-quote":
            return self._send(*price_quote(body))
        if self.path == "/events/reset" or self.path.startswith("/events/reset/"):
            engine = self.path[len("/events/reset/"):] or None
            with _lock:
                for store in (_events, _payloads):
                    if engine is None:
                        store.clear()
                    else:
                        store.pop(engine, None)
            return self._send(200, {"ok": True})
        if self.path.startswith("/events/"):
            engine = self.path[len("/events/"):]
            name = body.get("trigger", {}).get("name", "unknown")
            with _lock:
                _events[engine][name] += 1
                _payloads[engine][name].append(body)
            return self._send(200, {"ok": True})
        self._send(404, {"message": "not found"})


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "3000"))
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
