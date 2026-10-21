#!/usr/bin/env python3
"""A tiny MCP server over Streamable HTTP (stdlib only), for tests.

Listens on 127.0.0.1 on a free port and prints `PORT <n>` as its first stdout
line. The endpoint is `/mcp`. Speaks protocol 2025-11-25: initialize,
notifications/initialized, ping, tools/list, tools/call. Behaviour is chosen
by environment variables:

  FIXTURE_MODE         json (default): one application/json body per request.
                       sse: a text/event-stream whose last event is the
                       response. sse_multi: the same, but the stream starts
                       with a priming event (id, no data), a comment, a
                       progress notification and a `ping` request (which the
                       client must answer with a POST), and the response is
                       pretty-printed so it spans several `data:` lines,
                       with CRLF line ends.
  FIXTURE_SESSION      1: initialize answers with `MCP-Session-Id`; every other
                       request must carry it (400 without it, 404 when it is
                       not the one issued).
  FIXTURE_AUTH         the value `Authorization` must have; 401 otherwise.
  FIXTURE_LOG          append one JSON line per request: {http, rpc, headers}
                       (header names lowercased), and one for a DELETE.
  FIXTURE_EXPIRE_AFTER N: after N tools/call requests the session is gone: every
                       later request that carries the old id gets 404.
  FIXTURE_NO_DELETE    answer DELETE with 405.
  FIXTURE_REDIRECT     answer every POST with a 302 to this URL.
  FIXTURE_NO_STREAM_END  in sse modes, never close the stream after the
                       response (the client must stop reading by itself).

Tools: echo {text}, add {a,b}, fail, big {size}, slow {seconds}, headers (the request headers
the server saw, as JSON, so a test can tell what reached it).
"""
import json
import os
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = os.environ.get("FIXTURE_MODE", "json")
SESSION = os.environ.get("FIXTURE_SESSION") == "1"
AUTH = os.environ.get("FIXTURE_AUTH")
LOG = os.environ.get("FIXTURE_LOG")
EXPIRE_AFTER = int(os.environ.get("FIXTURE_EXPIRE_AFTER", "0"))
LOCK = threading.Lock()
STATE = {"session": None, "calls": 0, "expired": False}


def log(entry):
    if LOG:
        with LOCK, open(LOG, "a") as handle:
            handle.write(json.dumps(entry) + "\n")


def tool(name, description, props=None, required=None):
    return {
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": props or {}, "required": required or []},
    }


TOOLS = [
    tool("echo", "Echo the text back", {"text": {"type": "string"}}, ["text"]),
    tool("add", "Add two numbers", {"a": {"type": "number"}, "b": {"type": "number"}}, ["a", "b"]),
    tool("fail", "Always reports a tool error"),
    tool("big", "A text of `size` characters", {"size": {"type": "number"}}, ["size"]),
    tool("headers", "The request headers the server saw"),
    tool("slow", "Sleep then answer", {"seconds": {"type": "number"}}, ["seconds"]),
]


def text(value, error=False):
    return {"content": [{"type": "text", "text": value}], "isError": error}


def call(name, args, headers):
    if name == "echo":
        return text(args.get("text", ""))
    if name == "add":
        return text(str(args["a"] + args["b"]))
    if name == "fail":
        return text("it failed", True)
    if name == "big":
        return text("x" * int(args["size"]))
    if name == "slow":
        import time

        time.sleep(float(args["seconds"]))
        return text("slept")
    if name == "headers":
        return text(json.dumps({k: v for k, v in headers.items() if k.startswith("x-") or k == "authorization"}, sort_keys=True))
    return text("unknown tool " + name, True)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def headers_dict(self):
        return {k.lower(): v for k, v in self.headers.items()}

    def reply(self, status, body=b"", content_type=None, extra=None):
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_DELETE(self):
        headers = self.headers_dict()
        log({"http": "DELETE", "rpc": None, "headers": headers})
        if os.environ.get("FIXTURE_NO_DELETE"):
            return self.reply(405)
        self.reply(200)

    def do_GET(self):
        log({"http": "GET", "rpc": None, "headers": self.headers_dict()})
        self.reply(405)

    def do_POST(self):
        headers = self.headers_dict()
        length = int(headers.get("content-length", "0"))
        body = self.rfile.read(length)
        try:
            message = json.loads(body)
        except ValueError:
            return self.reply(400)
        method = message.get("method")
        ident = message.get("id")
        rpc = method or ("response " + str(ident))
        log({"http": "POST", "rpc": rpc, "headers": headers})
        redirect = os.environ.get("FIXTURE_REDIRECT")
        if redirect:
            return self.reply(302, extra={"Location": redirect})
        if AUTH is not None and headers.get("authorization") != AUTH:
            return self.reply(401, extra={"WWW-Authenticate": "Bearer"})
        accept = headers.get("accept", "")
        if "application/json" not in accept or "text/event-stream" not in accept:
            return self.reply(406)
        if SESSION and method != "initialize":
            sent = headers.get("mcp-session-id")
            if sent is None:
                return self.reply(400)
            if sent != STATE["session"] or STATE["expired"]:
                return self.reply(404)
        if method is None:
            # a response to something we asked on a stream
            return self.reply(202)
        if ident is None:
            return self.reply(202)
        extra = {}
        if method == "initialize":
            result = {
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture-http", "version": "1"},
            }
            if SESSION:
                STATE["session"] = "sess-" + uuid.uuid4().hex
                STATE["expired"] = False
                extra["MCP-Session-Id"] = STATE["session"]
        elif method == "ping":
            result = {}
        elif method == "tools/list":
            result = {"tools": TOOLS}
        elif method == "tools/call":
            params = message.get("params", {})
            result = call(params.get("name"), params.get("arguments", {}), headers)
            STATE["calls"] += 1
            if EXPIRE_AFTER and STATE["calls"] >= EXPIRE_AFTER:
                STATE["expired"] = True
        else:
            return self.send_json(
                {"jsonrpc": "2.0", "id": ident, "error": {"code": -32601, "message": "Method not found"}}, extra
            )
        self.send_json({"jsonrpc": "2.0", "id": ident, "result": result}, extra)

    def send_json(self, response, extra):
        if MODE == "json":
            return self.reply(200, json.dumps(response).encode(), "application/json", extra)
        eol = "\r\n" if MODE == "sse_multi" else "\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        for key, value in extra.items():
            self.send_header(key, value)
        self.send_header("Connection", "close")
        self.end_headers()
        out = self.wfile

        def event(data=None, ident=None, comment=None):
            if comment is not None:
                out.write((": " + comment + eol).encode())
            if ident is not None:
                out.write(("id: " + str(ident) + eol).encode())
            if data is not None:
                for line in data.split("\n"):
                    out.write(("data: " + line + eol).encode())
            else:
                out.write(("data:" + eol).encode())
            out.write(eol.encode())
            out.flush()

        if MODE == "sse_multi" and response.get("result") is not None and "content" in response["result"]:
            event(ident="1")
            event(data=json.dumps({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": 1, "progress": 1}}), comment="keep-alive")
            event(data=json.dumps({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"}))
            event(data=json.dumps(response, indent=1), ident="2")
        elif MODE == "sse_multi":
            event(data=json.dumps(response, indent=1))
        else:
            event(data=json.dumps(response))
        if os.environ.get("FIXTURE_NO_STREAM_END"):
            import time

            time.sleep(30)
        self.close_connection = True


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print("PORT", server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
    sys.exit(0)
