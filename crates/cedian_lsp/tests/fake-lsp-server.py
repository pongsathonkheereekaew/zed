#!/usr/bin/env python3
"""Fake LSP server for S1 hermetic tests (pattern: fake DAP adapter).

Speaks Content-Length framing over stdio. Answers initialize, then on didOpen
emits publishDiagnostics 3 waves (0, 2, 3 items) 2s apart — mirroring the real
rust-analyzer E0308 shape. Answers workspace/documentSymbol + hover minimally.
Usage: LspClient::spawn("python3 fake-lsp-server.py", workdir, root_uri).
"""
import json
import sys
import threading
import time


def read_frame(stream):
    hdr = b""
    while not hdr.endswith(b"\r\n\r\n"):
        c = stream.read(1)
        if not c:
            return None
        hdr += c
    n = 0
    for line in hdr.decode().split("\r\n"):
        if line.lower().startswith("content-length"):
            n = int(line.split(":")[1])
    body = b""
    while len(body) < n:
        chunk = stream.read(n - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body)


_SEND_LOCK = threading.Lock()


def send(obj):
    body = json.dumps(obj, separators=(",", ":")).encode()
    with _SEND_LOCK:
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        sys.stdout.buffer.flush()


SEQ = [100]


def notify(method, params):
    SEQ[0] += 1
    send({"jsonrpc": "2.0", "method": method, "params": params})


def respond(req_id, result):
    SEQ[0] += 1
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def diag(line, char, msg, sev=1):
    return {
        "range": {"start": {"line": line, "character": char}, "end": {"line": line, "character": char + 6}},
        "severity": sev,
        "code": "E0308",
        "source": "rustc",
        "message": msg,
    }


def waves(uri):
    notify("textDocument/publishDiagnostics", {"uri": uri, "diagnostics": []})
    time.sleep(2)
    notify(
        "textDocument/publishDiagnostics",
        {"uri": uri, "diagnostics": [diag(1, 17, "mismatched types"), diag(3, 4, "not found")]},
    )
    time.sleep(2)
    notify(
        "textDocument/publishDiagnostics",
        {
            "uri": uri,
            "diagnostics": [
                diag(1, 17, "mismatched types"),
                diag(3, 4, "not found"),
                diag(0, 0, "unused", 2),
            ],
        },
    )


def main():
    while True:
        req = read_frame(sys.stdin.buffer)
        if req is None:
            break
        method = req.get("method")
        rid = req.get("id")
        if method == "initialize":
            respond(rid, {"capabilities": {"hoverProvider": True, "definitionProvider": True}})
        elif method == "initialized":
            pass
        elif method == "textDocument/didOpen":
            uri = req["params"]["textDocument"]["uri"]
            threading.Thread(target=waves, args=(uri,), daemon=True).start()
            if rid is not None:
                respond(rid, {})
        elif method == "textDocument/didSave":
            if rid is not None:
                respond(rid, {})
        elif method == "textDocument/hover":
            respond(rid, {"contents": {"kind": "plaintext", "value": "fake hover"}})
        elif method == "textDocument/documentSymbol":
            respond(rid, [{"name": "main", "kind": 12, "location": {}}])
        elif method == "workspace/symbol":
            q = req["params"].get("query", "")
            respond(rid, [{"name": "main", "kind": 12, "location": {}}] if "main" in q else [])
        elif method == "exit":
            break
        elif rid is not None:
            respond(rid, None)


main()
