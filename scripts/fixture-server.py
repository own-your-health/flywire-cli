#!/usr/bin/env python3
"""Serve tests/fixtures/graphql/ on 127.0.0.1 the way a Flywire portal's
GraphQL endpoint answers flywire's reads, for driving an installed `flywire`
on synthetic data:

    python3 -I scripts/fixture-server.py 8951 &
    export FLYWIRE_KEYCHAIN_SERVICE=piekstra.flywire.dogfood XDG_CONFIG_HOME=$(mktemp -d)
    export FLYWIRE_BASE_URL=http://127.0.0.1:8951
    flywire profiles add synth
    printf '_simplee_store2=dummy1' | flywire auth login --stdin --user-agent test \\
        --provider exampleclinic --profile synth
    flywire bills list --profile synth
    flywire auth reset --force       # removes the dogfood vault key

`POST /graphql` is answered by its `operationName`: first
`<operationName>.<id>.json`, the id its variables name (`id`, `statementId`
or the first of `payableIds`), then `<operationName>.json`. A request
without the `_simplee_store2=dummy1` cookie gets `signedOut.json`.

For the error paths, `--answer KEY=FILE` answers KEY (`<operationName>` or
`<operationName>.<id>`) with another fixture (`signedOut.json`,
`error.json`) ahead of its own, and `--status KEY=CODE` with that HTTP
status and an empty JSON body. `GET /` answers 200, so a script can wait for the server.

`scripts/dogfood.sh` (`make dogfood`) runs every command and error path this
way.
"""

import argparse
import http.server
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "..", "tests", "fixtures", "graphql")
SESSION = "_simplee_store2=dummy1"


def pairs(values):
    out = {}
    for v in values:
        key, value = v.rsplit("=", 1)
        out[key] = value
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("--answer", action="append", default=[], metavar="KEY=FILE")
    ap.add_argument("--status", action="append", default=[], metavar="KEY=CODE")
    args = ap.parse_args()
    answers = pairs(args.answer)
    statuses = {k: int(v) for k, v in pairs(args.status).items()}

    def fixture(name):
        path = os.path.join(FIXTURES, os.path.basename(name))
        if not os.path.isfile(path):
            return None
        with open(path, "rb") as f:
            return f.read()

    class Handler(http.server.BaseHTTPRequestHandler):
        def answer(self, code, body, mime="application/json"):
            self.send_response(code)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/":
                self.answer(200, b"ok", "text/plain")
            else:
                self.answer(404, b"<p>not found</p>", "text/html")

        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length)
            if self.path != "/graphql":
                return self.answer(404, b"<p>not found</p>", "text/html")
            try:
                doc = json.loads(raw)
            except ValueError:
                return self.answer(400, b"{}")
            op = doc.get("operationName") or ""
            variables = doc.get("variables") or {}
            ids = variables.get("payableIds") or [None]
            ident = variables.get("id") or variables.get("statementId") or ids[0]
            keys = ([f"{op}.{ident}"] if ident else []) + [op]
            for key in keys:
                if key in statuses:
                    return self.answer(statuses[key], b"{}")
            if SESSION not in self.headers.get("Cookie", ""):
                return self.answer(200, fixture("signedOut.json"))
            for name in [answers[k] for k in keys if k in answers] + [f"{k}.json" for k in keys]:
                body = fixture(name)
                if body is not None:
                    return self.answer(200, body)
            self.answer(200, b'{"data":null,"errors":[{"message":"no fixture"}]}')

        def log_message(self, fmt, *a):
            sys.stderr.write("fixture-server: " + (fmt % a) + "\n")

    http.server.HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
