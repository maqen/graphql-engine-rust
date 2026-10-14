# Vendored from hasura/graphql-engine server/tests-py/context.py (v2.51.0): EvtsWebhookHandler
# and EvtsWebhookServer, the event-trigger webhook of test_events.py and
# test_scheduled_triggers.py, with GET routes for what upstream's tests read from the server
# directly. Run as: python3 events_webhook.py <port>

from http import HTTPStatus
import http.server
import json
import queue
import socketserver
import sys
import threading
import time
from urllib.parse import parse_qs, urlparse


class ThreadedHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


class EvtsWebhookHandler(http.server.BaseHTTPRequestHandler):
    server: 'EvtsWebhookServer'  # type: ignore

    def do_GET(self):
        self.send_response(HTTPStatus.OK)
        self.end_headers()

    def do_POST(self):
        content_len = int(self.headers['Content-Length'])
        req_body = self.rfile.read(content_len).decode("utf-8")
        req_json = json.loads(req_body)
        req_headers = self.headers
        req_path = self.path
        self.log_message(json.dumps(req_json))
        if req_path == "/fail":
            self.send_response(HTTPStatus.INTERNAL_SERVER_ERROR)
            self.end_headers()
        # This endpoint just sleeps for 2 seconds:
        elif req_path == "/sleep_2s":
            time.sleep(2)
            self.send_response(HTTPStatus.NO_CONTENT)
            self.end_headers()
        # This is like a sleep endpoint above, but allowing us to decide
        # externally when the webhook can return, with unblock()
        elif req_path == "/block":
            if not self.server.unblocked:
                self.server.blocked_count += 1
                with self.server.unblocked_wait:
                    # We expect this timeout never to be reached, but if
                    # something goes wrong the main thread will block forever:
                    self.server.unblocked_wait.wait(timeout=60)
                self.server.blocked_count -= 1
            self.send_response(HTTPStatus.NO_CONTENT)
            self.end_headers()
        else:
            self.send_response(HTTPStatus.NO_CONTENT)
            self.end_headers()

        self.server.resp_queue.put({
            "path": req_path,
            "body": req_json,
            "headers": req_headers,
        })


class EvtsWebhookServer(ThreadedHTTPServer):
    def __init__(self, server_address):
        # Data received from hasura by our web hook, pushed after it returns to the client:
        self.resp_queue = queue.Queue()
        # We use these two vars to coordinate unblocking in the /block route
        self.unblocked = False
        self.unblocked_wait = threading.Condition()
        # ...and this for bookkeeping open blocked requests; this becomes
        # meaningless after the first call to unblock()
        self.blocked_count = 0

        super().__init__(server_address, EvtsWebhookHandler)

    # Unblock all webhook requests to /block. Idempotent.
    def unblock(self):
        self.unblocked = True
        with self.unblocked_wait:
            # NOTE: this only affects currently wait()-ing threads, future
            # wait()s will block again (hence the simple self.unblocked flag)
            self.unblocked_wait.notify_all()


class ControlHandler(EvtsWebhookHandler):
    """Upstream's handler, plus the GET routes the conformance cases use in place of
    upstream's Python calls on the server (get_event, unblock, is_queue_empty), and `/__reset`,
    which empties the queue."""

    def do_GET(self):
        url = urlparse(self.path)
        q = parse_qs(url.query)
        if url.path == '/__next_event':
            # The next delivery, as {path, body, headers}, with only the headers named in
            # `headers` (comma-separated), or 404 after `timeout` seconds (default 3).
            try:
                ev = self.server.resp_queue.get(timeout=float(q.get('timeout', ['3'])[0]))
            except queue.Empty:
                return self._json(HTTPStatus.NOT_FOUND, {'error': 'no event'})
            names = [n for n in q.get('headers', [''])[0].split(',') if n]
            headers = {n: ev['headers'].get(n) for n in names}
            return self._json(HTTPStatus.OK, {'path': ev['path'], 'body': ev['body'], 'headers': headers})
        if url.path == '/__queue_empty':
            return self._json(HTTPStatus.OK, {'empty': self.server.resp_queue.empty()})
        if url.path == '/__reset':
            # Drop deliveries left by earlier cases (another engine's retries, say).
            dropped = 0
            while True:
                try:
                    self.server.resp_queue.get_nowait()
                    dropped += 1
                except queue.Empty:
                    break
            return self._json(HTTPStatus.OK, {'dropped': dropped})
        if url.path == '/__unblock':
            self.server.unblock()
            return self._json(HTTPStatus.OK, {'blocked_count': self.server.blocked_count})
        super().do_GET()

    def _json(self, status, body):
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps(body).encode('utf-8'))


if __name__ == '__main__':
    server = EvtsWebhookServer(('127.0.0.1', int(sys.argv[1])))
    server.RequestHandlerClass = ControlHandler
    server.serve_forever()
