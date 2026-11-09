"""Serve a folder of release files the way GitHub Releases does, for testing the installers.

    python3 scripts/release-server.py <dist-dir> <tag> [port]

GET/HEAD /releases/latest                   -> 302 to /releases/tag/<tag>
GET/HEAD /releases/latest/download/<file>   -> 302 to /releases/download/<tag>/<file>
GET/HEAD /releases/tag/<tag>                -> 200
GET/HEAD /releases/download/<tag>/<file>    -> <dist-dir>/<file>
Point the installers at it with BEDROCK_MCP_RELEASES=http://127.0.0.1:<port>/releases.
"""

import http.server
import pathlib
import sys

DIST = pathlib.Path(sys.argv[1]).resolve()
TAG = sys.argv[2]
PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 8765


class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, body: bool) -> None:
        path = self.path.split("?", 1)[0]
        if path == "/releases/latest" or path.startswith("/releases/latest/download/"):
            target = f"/releases/tag/{TAG}"
            if path != "/releases/latest":
                target = f"/releases/download/{TAG}/" + path.rsplit("/", 1)[1]
            self.send_response(302)
            self.send_header("Location", target)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if path == f"/releases/tag/{TAG}":
            data = TAG.encode()
        elif path.startswith(f"/releases/download/{TAG}/"):
            file = DIST / path.rsplit("/", 1)[1]
            if not file.is_file():
                self.send_error(404)
                return
            data = file.read_bytes()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if body:
            self.wfile.write(data)

    def do_GET(self) -> None:
        self.reply(True)

    def do_HEAD(self) -> None:
        self.reply(False)

    def log_message(self, *args) -> None:
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
