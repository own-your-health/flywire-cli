#!/bin/bash
# Drives a flywire binary through every command and error path against
# scripts/fixture-server.py, on the synthetic fixtures, and prints one row
# per case. Exits 1 if any case fails.
#
#     scripts/dogfood.sh                  # the flywire on PATH (the installed one)
#     scripts/dogfood.sh target/release/flywire
#
# It runs in a throwaway test store (AGENTS.md, "Dogfooding on synthetic
# data"): a vault key under the keychain service piekstra.flywire.dogfood,
# created by the binary under test, so no keychain dialog, and removed at
# the end with `auth reset --force`. It never reads the owner's sessions.
# Ports DOGFOOD_PORT (default 8951) to DOGFOOD_PORT+3 must be free.
set -u

FW=${1:-$(command -v flywire || true)}
[ -x "$FW" ] || { echo "usage: $0 [path to flywire]" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d)
export FLYWIRE_KEYCHAIN_SERVICE=piekstra.flywire.dogfood
export XDG_CONFIG_HOME=$T/config XDG_DATA_HOME=$T/data
unset FLYWIRE_PROFILE FLYWIRE_BASE_URL

P=${DOGFOOD_PORT:-8951}  # the fixtures
Q=$((P + 1))             # signed out on payables, a GraphQL error on one bill
R=$((P + 2))             # HTTP 500 on payables
G=$((P + 3))             # the guarantor query refused
DOWN=$((P + 8))          # nothing listens
SERVER=(python3 -I "$HERE/fixture-server.py")
"${SERVER[@]}" "$P" 2>"$T/server-$P.log" & S1=$!
"${SERVER[@]}" "$Q" --answer getSingleBill.BILL-0000001=error.json --answer previousPayments=signedOut.json \
    2>"$T/server-$Q.log" & S2=$!
"${SERVER[@]}" "$R" --status payables=500 2>"$T/server-$R.log" & S3=$!
"${SERVER[@]}" "$G" --answer payableGuarantors=error.json 2>"$T/server-$G.log" & S4=$!

cleanup() {
    exec 2>/dev/null  # no job notice for the servers killed
    "$FW" auth reset --force >/dev/null
    kill "$S1" "$S2" "$S3" "$S4" 2>/dev/null
    wait 2>/dev/null
    rm -rf "$T"
}
trap cleanup EXIT

for port in "$P" "$Q" "$R" "$G"; do
    up=
    for _ in $(seq 50); do
        curl -s -o /dev/null "http://127.0.0.1:$port/" && { up=1; break; }
        sleep 0.1
    done
    if [ -z "$up" ]; then
        echo "the fixture server on port $port did not start:" >&2
        cat "$T/server-$port.log" >&2
        exit 1
    fi
done

fails=0
check() { # NAME WANT GOT
    local r=ok
    if [ "$2" != "$3" ]; then r=FAIL; fails=$((fails + 1)); fi
    printf '| %s | %s | %s | %s |\n' "$1" "$2" "$3" "$r"
}
# The exit code of a command, stdin closed (so a prompt sees no terminal).
code() { # NAME WANT CMD...
    local name=$1 want=$2
    shift 2
    "$@" </dev/null >"$T/out" 2>"$T/err"
    check "$name" "exit $want" "exit $?"
}
# A Python expression over the command's JSON (`d`: the document, or a
# stream's last line, its trailer; `lines`: every line of a stream).
value() { # NAME WANT EXPR CMD...
    local name=$1 want=$2 expr=$3 got
    shift 3
    "$@" </dev/null >"$T/out" 2>"$T/err"
    got=$(python3 -I -c "
import json, sys
t = open(sys.argv[1]).read()
try:
    d = json.loads(t)
    lines = [d]
except ValueError:
    lines = [json.loads(l) for l in t.strip().splitlines()]
    d = lines[-1]
v = ($expr)
print(*v) if isinstance(v, tuple) else print(v)" "$T/out" 2>&1)
    check "$name" "$want" "$got"
}
at() { # PORT CMD...: read the fixture server on PORT
    local port=$1
    shift
    FLYWIRE_BASE_URL=http://127.0.0.1:$port "$@"
}
login() { # COOKIE [FLAGS...]
    printf '%s' "$1" | "$FW" auth login --stdin --user-agent test --provider exampleclinic --profile synth "${@:2}"
}
SESSION='_simplee_store2=dummy1; dummy_other=x'
DOCS=$T/docs
ALL=$T/all
OUT=$T/export
S=(--profile synth)

echo "flywire: $FW ($("$FW" --version))"
echo
echo '| Case | Want | Got | |'
echo '|---|---|---|---|'
value "info" "cli-info/v1 True" "d['schema'], 'bills' in d['capabilities']" "$FW" info --json
code "NO_COLOR=1" 0 env NO_COLOR=1 "$FW" info
code "no profile" 2 at "$P" "$FW" accounts list
code "profiles add" 0 "$FW" profiles add synth
code "unknown profile" 2 at "$P" "$FW" bills list --profile nobody
code "no session stored" 3 at "$P" "$FW" accounts list "${S[@]}"
code "login --browser with the override" 2 at "$P" "$FW" auth login --browser --provider exampleclinic "${S[@]}"
code "login --stdin" 0 at "$P" login "$SESSION"
code "login again without --overwrite" 2 at "$P" login "$SESSION"
code "login with a signed-out cookie" 3 at "$P" login 'other=1' --overwrite
value "auth status" "True exampleclinic" "d['authenticated'], d.get('provider')" "$FW" auth status "${S[@]}" --json
value "patients list" "1 org-exampleclinic" "len(d['items']), d['items'][0]['organization']" at "$P" "$FW" patients list "${S[@]}" --json
value "accounts list: count, owed" "2 45.00 0.00" "len(d['items']), d['items'][0]['owes'], d['items'][1]['owes']" at "$P" "$FW" accounts list "${S[@]}" --json
code "accounts list: table" 0 at "$P" "$FW" accounts list "${S[@]}"
value "bills list" "4 BILL-0000004" "len(d['items']), d['items'][0]['id']" at "$P" "$FW" bills list "${S[@]}" --json
value "bills list --open" "BILL-0000001" "' '.join(i['id'] for i in d['items'])" at "$P" "$FW" bills list --open "${S[@]}" --json
value "bills list --since --until" "BILL-0000003 BILL-0000002" "' '.join(i['id'] for i in d['items'])" at "$P" "$FW" bills list --since 2026-01-10 --until 2026-01-15 "${S[@]}" --json
value "bills list --account" "1" "len(d['items'])" at "$P" "$FW" bills list --account ACCT-0000002 "${S[@]}" --json
value "bills list --limit" "1" "len(d['items'])" at "$P" "$FW" bills list --limit 1 "${S[@]}" --json
code "bills list --since after --until" 2 at "$P" "$FW" bills list --since 2026-01-20 --until 2026-01-10 "${S[@]}"
value "bills show" "250.00 150.00 45.00 2" "d['totals']['billed'], d['totals']['insurance_paid'], d['totals']['owes'], len(d['charges'])" at "$P" "$FW" bills show BILL-0000001 "${S[@]}" --json
code "bills show: text" 0 at "$P" "$FW" bills show BILL-0000002 "${S[@]}"
code "bills show: unknown bill" 4 at "$P" "$FW" bills show BILL-0000009 "${S[@]}"
value "payments list: count, settled" "3 2" "len(d['items']), sum(i['settled'] for i in d['items'])" at "$P" "$FW" payments list "${S[@]}" --json
value "payments list --since" "1" "len(d['items'])" at "$P" "$FW" payments list --since 2026-01-21 "${S[@]}" --json
value "documents list" "document-list/v1 2" "d['schema'], len(d['items'])" at "$P" "$FW" documents list "${S[@]}" --json
mkdir -p "$DOCS"
value "documents download (PDF)" "True" "d['file'].endswith('.pdf')" at "$P" "$FW" documents download STMT-0000001 -o "$DOCS" "${S[@]}" --json
check "documents download: a PDF on disk" "%PDF-" "$(head -c 5 "$DOCS"/*STMT-0000001.pdf)"
code "documents download again without --overwrite" 2 at "$P" "$FW" documents download STMT-0000001 -o "$DOCS" "${S[@]}"
code "documents download --overwrite" 0 at "$P" "$FW" documents download STMT-0000001 -o "$DOCS" --overwrite "${S[@]}"
code "documents download -o - (stdout)" 0 at "$P" "$FW" documents download STMT-0000001 -o - "${S[@]}"
check "documents download -o -: the bytes" "%PDF-" "$(head -c 5 "$T/out")"
value "documents download --all" "2 0" "d['count'], len(d.get('skipped', []))" at "$P" "$FW" documents download --all -o "$ALL" "${S[@]}" --json
check "documents download --all: saved as sent, said so" "1" "$(grep -c 'saved as sent' "$T/err")"
value "documents download --all again" "0 2" "d['count'], len(d.get('skipped', []))" at "$P" "$FW" documents download --all -o "$ALL" "${S[@]}" --json
code "documents download: a statement with no file" 4 at "$P" "$FW" documents download STMT-0000002 -o "$DOCS" "${S[@]}"
code "documents download --all -o -" 2 at "$P" "$FW" documents download --all -o - "${S[@]}"
value "records export" "True 4 2 1 1" "d['complete'], d['counts']['Invoice'], d['counts']['PaymentReconciliation'], d['counts']['DocumentReference'], d['counts']['Binary']" at "$P" "$FW" records export "${S[@]}" --ndjson
value "records export: header" "records-stream/v1 portal" "lines[0]['schema'], lines[0]['method']" at "$P" "$FW" records export "${S[@]}" --ndjson
value "records export: skipped" "not_settled verify_failed" "' '.join(s['code'] for s in d['skipped'])" at "$P" "$FW" records export "${S[@]}" --ndjson
value "records export --since" "True 2 1" "d['complete'], d['counts']['Invoice'], d['counts']['PaymentReconciliation']" at "$P" "$FW" records export --since 2026-01-15 "${S[@]}" --ndjson
PATIENT=$(at "$P" "$FW" patients list "${S[@]}" --json | python3 -I -c "import json,sys; print(json.load(sys.stdin)['items'][0]['id'])")
code "records export --patient --org" 0 at "$P" "$FW" records export --patient "$PATIENT" --org org-exampleclinic "${S[@]}" --ndjson
code "records export -o" 0 at "$P" "$FW" records export "${S[@]}" -o "$OUT"
check "records export -o: lines" "14" "$(wc -l <"$OUT/records.ndjson" | tr -d ' ')"
code "another patient's --patient" 4 at "$P" "$FW" records export --patient patient-other "${S[@]}" --ndjson
code "another provider's --org" 4 at "$P" "$FW" records export --org org-other "${S[@]}" --ndjson
code "records export without a destination" 2 at "$P" "$FW" records export "${S[@]}"
code "bad --since" 2 at "$P" "$FW" records export --since 2026-13-01 "${S[@]}" --ndjson
code "--ndjson with --json" 2 at "$P" "$FW" records export "${S[@]}" --ndjson --json
code "-o against the real portal" 2 "$FW" records export "${S[@]}" -o "$OUT"
value "api: payables" "2" "len(d['data']['payables']['payables'])" at "$P" "$FW" api POST /graphql --data '{"operationName":"payables","query":"query payables { payables { payables { id } } }"}' "${S[@]}"
code "api: a mutation" 2 at "$P" "$FW" api POST /graphql --data '{"query":"mutation logout { logout }"}' "${S[@]}"
code "api: the sign-out path" 2 at "$P" "$FW" api GET /app/bills/logout "${S[@]}"
code "api: another host" 2 at "$P" "$FW" api GET https://example.com/app/bills "${S[@]}"
code "GraphQL error on a bill" 5 at "$Q" "$FW" bills show BILL-0000001 "${S[@]}"
check "GraphQL error: its text not printed" "0" "$(grep -c 'could not be loaded' "$T/err")"
code "signed out mid-read" 3 at "$Q" "$FW" payments list "${S[@]}"
code "signed out mid-export" 3 at "$Q" "$FW" records export "${S[@]}" --ndjson
value "a bill skipped, then signed out: trailer" "False upstream 3" "d['complete'], d['skipped'][0]['code'], d['error']['exit_code']" at "$Q" "$FW" records export "${S[@]}" --ndjson
code "HTTP 500" 5 at "$R" "$FW" accounts list "${S[@]}"
code "HTTP 500 before the stream" 5 at "$R" "$FW" records export "${S[@]}" --ndjson
check "HTTP 500 before the stream: nothing on stdout" "0" "$(wc -c <"$T/out" | tr -d ' ')"
code "guarantor refused: patients list" 5 at "$G" "$FW" patients list "${S[@]}"
code "guarantor refused: records export" 5 at "$G" "$FW" records export "${S[@]}" --ndjson
check "guarantor refused: nothing on stdout" "0" "$(wc -c <"$T/out" | tr -d ' ')"
value "guarantor refused: bills list still reads" "4" "len(d['items'])" at "$G" "$FW" bills list "${S[@]}" --json
code "server down" 5 at "$DOWN" "$FW" accounts list "${S[@]}"
code "a base that is not loopback" 2 env FLYWIRE_BASE_URL=http://example.com "$FW" accounts list "${S[@]}"
code "config path" 0 "$FW" config path
code "config show" 0 "$FW" config show
code "profiles remove without a terminal" 6 "$FW" profiles remove synth
code "auth logout" 0 "$FW" auth logout "${S[@]}"
code "a read after logout" 3 at "$P" "$FW" bills list "${S[@]}"
code "auth reset without a terminal" 6 "$FW" auth reset
code "auth reset --force" 0 "$FW" auth reset --force

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails case(s) failed"
    exit 1
fi
echo "every case passed"
