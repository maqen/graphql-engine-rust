"""The webhook `transforms_steps.py` records against: logs every request (`GET /log` returns
the log, `GET /reset` clears it) and answers like the webhook of `event_transforms.rs`.

usage: hook.py <port>
"""

import http.server
import json
import sys

LOG = []


class Hook(http.server.BaseHTTPRequestHandler):
    def handle_any(self):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length).decode()
        if self.path == "/reset":
            LOG.clear()
            out, status = b"{}", 200
        elif self.path == "/log":
            out, status = json.dumps(LOG).encode(), 200
        else:
            LOG.append(
                {
                    "method": self.command,
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body": body,
                }
            )
            if "fail" in self.path:
                out, status = b'{"oops": 1}', 500
            else:
                out, status = b'{"hello": "world", "n": 3}', 200
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(out)

    do_GET = do_POST = do_PUT = do_PATCH = handle_any

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Hook).serve_forever()
