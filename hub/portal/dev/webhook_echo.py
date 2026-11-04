"""A webhook receiver for the dev loop: logs every request and keeps the last
50, so the portal's alarm webhooks can be checked without a real endpoint.

    POST /anything   -> 200, body recorded
    GET  /           -> the recorded requests, newest first, as JSON

Point the portal at http://localhost:8099/alarms (Settings -> Notifications ->
Webhook). stdlib only; see dev/README.md.
"""

import json
import sys
from collections import deque
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RECEIVED = deque(maxlen=50)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 - http.server's naming
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length).decode("utf-8", "replace")
        try:
            body = json.loads(raw)
        except ValueError:
            body = raw
        entry = {
            "received_at": datetime.now(timezone.utc).isoformat(),
            "path": self.path,
            "headers": {k: v for k, v in self.headers.items() if k.lower() != "authorization"},
            "body": body,
        }
        RECEIVED.appendleft(entry)
        event = body.get("event") if isinstance(body, dict) else None
        print(f"webhook {self.path} event={event}", flush=True)
        self.send_response(200)
        self.end_headers()

    do_PUT = do_POST  # the portal's method is configurable

    def do_GET(self):  # noqa: N802
        payload = json.dumps(list(RECEIVED), indent=2).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
    print(f"webhook echo on :{port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
