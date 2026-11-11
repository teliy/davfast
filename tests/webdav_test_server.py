#!/usr/bin/env python3
"""Minimal WebDAV test server (stdlib only) for smoke-testing davfast.

Deliberately uses a different XML style than davfast's own test fixtures
(no namespace prefixes, attribute-style depth handling) to validate the
PROPFIND parser against foreign servers.

Usage: python3 webdav_test_server.py <port> <root_dir>
"""
import os
import sys
import threading
from email.utils import formatdate
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlparse

ROOT = None  # set in main()


def fs_path(url_path: str) -> str:
    rel = unquote(url_path).lstrip("/")
    if not rel:
        return ROOT
    return os.path.join(ROOT, rel)


def xml_escape(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "PyMiniDAV/0.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def _date(self, p):
        try:
            return formatdate(os.path.getmtime(p), usegmt=True)
        except OSError:
            return formatdate(0, usegmt=True)

    def _entry(self, url_path, is_dir, size):
        href = xml_escape(url_path)
        rt = "<D:collection/>" if is_dir else ""
        length = "" if is_dir else "<D:getcontentlength>%d</D:getcontentlength>" % size
        p = fs_path(url_path)
        return (
            "<D:response>"
            "<D:href>%s</D:href>"
            "<D:propstat><D:prop>"
            '<D:resourcetype xmlns:D="DAV:">%s</D:resourcetype>'
            "%s"
            "<D:getlastmodified>%s</D:getlastmodified>"
            '<D:getetag xmlns:D="DAV:">&quot;py-%d&quot;</D:getetag>'
            "</D:prop>"
            "<D:status>HTTP/1.1 200 OK</D:status>"
            "</D:propstat></D:response>" % (href, rt, length, self._date(p), size)
        )

    def do_OPTIONS(self):
        self.send_response(200)
        self.send_header("DAV", "1, 2")
        self.send_header("Allow", "OPTIONS, GET, PUT, DELETE, PROPFIND, MKCOL")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_PROPFIND(self):
        path = unquote(urlparse(self.path).path)
        depth = self.headers.get("Depth", "1")
        fs = fs_path(path)
        if not os.path.exists(fs):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        entries = []
        self_url = path if path.endswith("/") else path + "/"
        entries.append(self._entry(self_url, True, 0))
        if depth != "0" and os.path.isdir(fs):
            for name in sorted(os.listdir(fs)):
                child_fs = os.path.join(fs, name)
                child_url = self_url + name + ("/" if os.path.isdir(child_fs) else "")
                if os.path.isdir(child_fs):
                    entries.append(self._entry(child_url, True, 0))
                else:
                    entries.append(self._entry(child_url, False, os.path.getsize(child_fs)))
        body = (
            '<?xml version="1.0" encoding="utf-8"?>'
            '<D:multistatus xmlns:D="DAV:">'
            + "".join(entries)
            + "</D:multistatus>"
        ).encode("utf-8")
        self.send_response(207)
        self.send_header("Content-Type", 'application/xml; charset="utf-8"')
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = unquote(urlparse(self.path).path)
        fs = fs_path(path)
        if not os.path.isfile(fs):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        size = os.path.getsize(fs)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(size))
        self.send_header("Last-Modified", self._date(fs))
        self.end_headers()
        with open(fs, "rb") as f:
            while True:
                chunk = f.read(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)

    def do_PUT(self):
        path = unquote(urlparse(self.path).path)
        fs = fs_path(path)
        os.makedirs(os.path.dirname(fs), exist_ok=True)
        length = int(self.headers.get("Content-Length", "0"))
        remaining = length
        with open(fs, "wb") as f:
            while remaining > 0:
                chunk = self.rfile.read(min(65536, remaining))
                if not chunk:
                    break
                f.write(chunk)
                remaining -= len(chunk)
        self.send_response(201)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_MKCOL(self):
        path = unquote(urlparse(self.path).path)
        fs = fs_path(path)
        if os.path.exists(fs):
            self.send_response(405)
        else:
            os.makedirs(fs, exist_ok=True)
            self.send_response(201)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_DELETE(self):
        path = unquote(urlparse(self.path).path)
        fs = fs_path(path)
        if os.path.isfile(fs):
            os.remove(fs)
            self.send_response(204)
        elif os.path.isdir(fs):
            import shutil

            shutil.rmtree(fs)
            self.send_response(204)
        else:
            self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()


def main():
    global ROOT
    port = int(sys.argv[1])
    ROOT = sys.argv[2]
    os.makedirs(ROOT, exist_ok=True)
    # keep-alive loop per connection
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print("ready", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
