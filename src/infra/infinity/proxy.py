#!/usr/bin/env python3
"""Transparent logging proxy for the Infinity embedding server.

Logs the 'model' field from POST /v1/embeddings requests so you can
diagnose model-name mismatches between clients and the loaded model.

Usage: python3 proxy.py <upstream_url> <listen_port>
  upstream_url  — base URL of the real Infinity server (e.g. http://localhost:17997)
  listen_port   — port this proxy listens on (e.g. 7997)
"""

import http.server
import json
import sys
import urllib.request
import urllib.error
from socketserver import ThreadingMixIn

UPSTREAM = sys.argv[1].rstrip("/")
LISTEN_PORT = int(sys.argv[2])

HOP_BY_HOP = frozenset(
    ["transfer-encoding", "connection", "keep-alive", "proxy-authenticate",
     "proxy-authorization", "te", "trailers", "upgrade"]
)


class ProxyHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # suppress default Apache-style access log

    def _forward(self, method: str, body: bytes | None = None):
        url = UPSTREAM + self.path
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in HOP_BY_HOP | {"host"}}
        req = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req) as resp:
                rb = resp.read()
                self.send_response(resp.status)
                for k, v in resp.headers.items():
                    if k.lower() not in HOP_BY_HOP:
                        self.send_header(k, v)
                self.end_headers()
                self.wfile.write(rb)
                return resp.status, rb
        except urllib.error.HTTPError as exc:
            rb = exc.read()
            self.send_response(exc.code)
            for k, v in exc.headers.items():
                if k.lower() not in HOP_BY_HOP:
                    self.send_header(k, v)
            self.end_headers()
            self.wfile.write(rb)
            return exc.code, rb
        except urllib.error.URLError:
            msg = b'{"detail":"infinity-emb not ready"}'
            self.send_response(503)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)
            return 503, msg

    def do_GET(self):
        self._forward("GET")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)

        is_embed = self.path.rstrip("/") in ("/v1/embeddings", "/embeddings")
        model = None
        n_inputs = None

        if is_embed:
            try:
                data = json.loads(body)
                model = data.get("model", "<missing>")
                inp = data.get("input", "")
                n_inputs = len(inp) if isinstance(inp, str) else len(inp)
            except Exception:
                pass

        status, resp_body = self._forward("POST", body)

        if is_embed:
            tag = f"model={model!r}  inputs={n_inputs}"
            if status == 200:
                print(f"[embed  OK] {tag}", flush=True)
            elif status == 400:
                detail = ""
                try:
                    detail = json.loads(resp_body).get("detail", "")
                except Exception:
                    pass
                print(f"[embed 400] {tag}  →  {detail or resp_body[:120]!r}", flush=True)
            elif status == 503:
                print(f"[embed 503] {tag}  →  infinity-emb not ready", flush=True)
            else:
                print(f"[embed {status}] {tag}", flush=True)


class ThreadedHTTPServer(ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


if __name__ == "__main__":
    server = ThreadedHTTPServer(("127.0.0.1", LISTEN_PORT), ProxyHandler)
    print(f"[proxy] :{LISTEN_PORT} → {UPSTREAM}  (logging /v1/embeddings model field)", flush=True)
    server.serve_forever()
