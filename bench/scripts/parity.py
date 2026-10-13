#!/usr/bin/env python3
"""Parity checker: sends every case in bench/queries to two engines (or to
one engine and the recorded golden responses in bench/expected) and diffs
the responses. Doubles as the compatibility test suite.

  # live: original Hasura vs our engine
  bench/scripts/parity.py --a hasura=http://localhost:8080 --b rust=http://localhost:8081

  # our engine vs the recorded Hasura responses (no Hasura needed)
  bench/scripts/parity.py --a rust=http://localhost:8081 --golden

  # re-record the golden responses from Hasura (on a fresh stack)
  bench/scripts/parity.py --a hasura=http://localhost:8080 --record

Comparison is strict: HTTP status, Content-Encoding (requests send
Accept-Encoding: gzip), every value, JSON types, the exact text of numbers
(`15.90` is not `15.9`) and the order of object keys (GraphQL requires
responses to follow selection order). Pass --loose-numbers to compare
numbers by value.

Event triggers: each engine posts to its own namespace on bench/handler
(/events/<engine name>, set by EVENT_WEBHOOK_URL in docker-compose.yml), so
the engine names given here must match those namespaces ("hasura", "rust").
Delivery counts and payloads (minus volatile fields) are compared too.

Exit status is 1 if anything differs.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

from suite import (
    EXPECTED_DIR,
    HANDLER_URL,
    Engine,
    RawNum,
    _OrderedObject,
    dumps,
    http_get,
    http_post_json,
    load_suite,
    loads,
    send,
)

EVENTS_FILE = "_events.json"
MAX_DIFFS = 20
# Fields in event payloads that legitimately differ between runs/engines.
VOLATILE_EVENT_KEYS = {"id", "created_at", "trace_context", "playlist_id"}


def diff(a, b, path="$", out=None, *, check_order=True, loose_numbers=False):
    """Collect human-readable differences between two values from `loads`."""
    out = [] if out is None else out
    kw = {"check_order": check_order, "loose_numbers": loose_numbers}

    def add(msg):
        if len(out) < MAX_DIFFS:
            out.append(msg)

    if len(out) >= MAX_DIFFS:
        return out
    if isinstance(a, dict) and isinstance(b, dict):
        missing = [k for k in a if k not in b]
        extra = [k for k in b if k not in a]
        for k in missing:
            add(f"{path}.{k}: missing in B")
        for k in extra:
            add(f"{path}.{k}: unexpected in B")
        for k in a:
            if k in b:
                diff(a[k], b[k], f"{path}.{k}", out, **kw)
        if check_order and not missing and not extra and _keys(a) != _keys(b):
            add(f"{path}: key order differs: A={_keys(a)} B={_keys(b)}")
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            add(f"{path}: array length A={len(a)} B={len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f"{path}[{i}]", out, **kw)
    elif _jtype(a) != _jtype(b):
        add(f"{path}: type A={_jtype(a)}({_short(a)}) B={_jtype(b)}({_short(b)})")
    elif isinstance(a, RawNum) and loose_numbers:
        if a.value() != b.value():
            add(f"{path}: A={a.text} B={b.text}")
    elif a != b:
        add(f"{path}: A={_short(a)} B={_short(b)}")
    return out


def _keys(d):
    return d.order if isinstance(d, _OrderedObject) else list(d)


def _jtype(v):
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, (RawNum, int, float)):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    return "object"


def _short(v, n=80):
    s = dumps(v, indent=0).replace("\n", "")
    return s if len(s) <= n else s[: n - 3] + "..."


# --------------------------------------------------------------------------
# event-trigger deliveries


def _strip_volatile(v):
    if isinstance(v, dict):
        return {k: _strip_volatile(x) for k, x in v.items() if k not in VOLATILE_EVENT_KEYS}
    if isinstance(v, list):
        return [_strip_volatile(x) for x in v]
    return v


def reset_events(engine_name):
    try:
        http_post_json(f"{HANDLER_URL}/events/reset/{engine_name}", {})
    except OSError as e:
        sys.exit(f"cannot reach event handler at {HANDLER_URL} ({e}); pass --no-events to skip")


def _event_counts(engine_name):
    try:
        _, body = http_get(HANDLER_URL + "/events/stats")
    except OSError:
        return None
    return json.loads(body).get(engine_name, {})


def wait_events(engine_name, target=None, timeout=30.0, settle=3.0, min_wait=0.0):
    """Wait for an engine's delivery counters to stop changing for `settle`
    seconds (and, with a target, to equal it), so late or duplicate
    deliveries are seen too. Returns the last counts."""
    start = time.monotonic()
    last, last_change = None, start
    while time.monotonic() - start < timeout:
        cur = _event_counts(engine_name)
        now = time.monotonic()
        if cur != last:
            last, last_change = cur, now
        elif (now - last_change >= settle and now - start >= min_wait
              and (target is None or cur == target)):
            return cur
        time.sleep(0.25)
    return last


def event_payloads(engine_name):
    _, body = http_get(f"{HANDLER_URL}/events/payloads/{engine_name}")
    return _strip_volatile(loads(body))


# --------------------------------------------------------------------------


def run_engine(engine, cases):
    results = {}
    for case in cases:
        resp = send(engine, case)
        try:
            body = resp.json()
        except ValueError:
            body = {"__non_json_body__": resp.body.decode(errors="replace")}
        results[case.name] = {
            "status": resp.status,
            "content_encoding": resp.headers.get("content-encoding"),
            "body": body,
        }
    return results


def record_events(engine, cases, strict):
    """Collect an engine's deliveries. Only when recording goldens is "no
    deliveries" fatal; when comparing, it is just a failed event row, so the
    report still covers every case."""
    has_mutations = any(c.is_mutation for c in cases)
    counts = wait_events(engine.name, min_wait=5.0 if has_mutations else 0.0) or {}
    if strict and has_mutations and not counts:
        sys.exit("suite has mutations but no event deliveries arrived; is the handler reachable from the engine?")
    # Round-trip through loads so live counts compare equal to golden RawNums.
    return {"counts": loads(json.dumps(counts)), "payloads": event_payloads(engine.name)}


def compare_events(ref, cmp_, loose_numbers):
    diffs = []
    if ref["counts"] != cmp_["counts"]:
        diffs.append(f"delivery counts A={_short(ref['counts'])} B={_short(cmp_['counts'])}")
    diffs += diff(ref["payloads"], cmp_["payloads"], "$events", check_order=False, loose_numbers=loose_numbers)
    return diffs


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--a", required=True, help="reference engine, name=url")
    ap.add_argument("--b", help="engine to compare, name=url")
    ap.add_argument("--golden", action="store_true", help="compare --a against bench/expected")
    ap.add_argument("--record", action="store_true", help="write --a responses to bench/expected")
    ap.add_argument("--only", action="append", help="regex filter on case names (repeatable)")
    ap.add_argument("--no-events", action="store_true", help="skip the event-trigger delivery check")
    ap.add_argument("--loose-numbers", action="store_true", help="compare numbers by value, not text")
    ap.add_argument("--report", help="write a markdown report to this path")
    ap.add_argument("--known-failures", help="file of case names expected to fail (one per line, # comments); "
                    "only failures not listed, or listed cases that now pass, make the exit status non-zero")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    if sum(bool(x) for x in (args.b, args.golden, args.record)) != 1:
        ap.error("pass exactly one of --b, --golden, --record")

    cases = [c for c in load_suite(args.only) if c.parity]
    eng_a = Engine.parse(args.a)
    check_events = not args.no_events and not args.only

    def run_with_events(engine):
        if check_events:
            reset_events(engine.name)
        res = run_engine(engine, cases)
        return res, (record_events(engine, cases, strict=args.record) if check_events else None)

    res_a, events_a = run_with_events(eng_a)

    if args.record:
        EXPECTED_DIR.mkdir(exist_ok=True)
        for name, r in res_a.items():
            (EXPECTED_DIR / f"{name}.json").write_text(dumps(r) + "\n")
        if events_a is not None:
            (EXPECTED_DIR / EVENTS_FILE).write_text(dumps(events_a) + "\n")
        print(f"recorded {len(res_a)} responses from {eng_a.name} into {EXPECTED_DIR}")
        if events_a is not None:
            print(f"event deliveries: {events_a['counts']}")
        return 0

    if args.golden:
        res_ref = {c.name: loads((EXPECTED_DIR / f"{c.name}.json").read_text()) for c in cases}
        res_cmp, name_ref, name_cmp = res_a, "golden", eng_a.name
        events_ref = events_cmp = None
        if check_events:
            path = EXPECTED_DIR / EVENTS_FILE
            if not path.exists():
                sys.exit(f"{path} missing: re-record goldens or pass --no-events")
            events_ref, events_cmp = loads(path.read_text()), events_a
    else:
        eng_b = Engine.parse(args.b)
        res_b, events_b = run_with_events(eng_b)
        res_ref, res_cmp, name_ref, name_cmp = res_a, res_b, eng_a.name, eng_b.name
        events_ref, events_cmp = events_a, events_b

    rows, failures = [], 0
    for case in cases:
        ref, cmp_ = res_ref[case.name], res_cmp[case.name]
        diffs = []
        if str(ref["status"]) != str(cmp_["status"]):  # goldens hold RawNum
            diffs.append(f"HTTP status A={ref['status']} B={cmp_['status']}")
        if ref.get("content_encoding") != cmp_.get("content_encoding"):
            diffs.append(f"Content-Encoding A={ref.get('content_encoding')} B={cmp_.get('content_encoding')}")
        diffs += diff(ref["body"], cmp_["body"], loose_numbers=args.loose_numbers)
        failures += bool(diffs)
        rows.append((case.name, diffs))

    if check_events:
        diffs = compare_events(events_ref, events_cmp, args.loose_numbers)
        failures += bool(diffs)
        rows.append(("event_trigger_deliveries", diffs))

    known = set()
    if args.known_failures:
        for line in Path(args.known_failures).read_text().splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                known.add(line)

    # Checked after the report is written, so CI still publishes it.
    unknown = sorted(known - {label for label, _ in rows}) if not (args.only or args.no_events) else []

    def verdict(label, diffs):
        if label in known:
            return "XFAIL" if diffs else "XPASS"
        return "FAIL" if diffs else "PASS"

    passed = len(rows) - failures
    print(f"parity {name_ref} (A) vs {name_cmp} (B): {passed}/{len(rows)} match")
    for label, diffs in rows:
        print(f"  {verdict(label, diffs):5} {label}")
        for d in diffs[: (MAX_DIFFS if args.verbose else 5)]:
            print(f"          {d}")

    new_failures = [label for label, diffs in rows if verdict(label, diffs) == "FAIL"]
    flipped = [label for label, diffs in rows if verdict(label, diffs) == "XPASS"]
    if known:
        print(f"known failures: {sum(verdict(l, d) == 'XFAIL' for l, d in rows)} still failing, "
              f"{len(new_failures)} new failures, {len(flipped)} now passing")
        for label in new_failures:
            print(f"  REGRESSION: {label} fails and is not in {args.known_failures}")
        for label in flipped:
            print(f"  NOW PASSING: delete {label} from {args.known_failures} in this PR")
        for label in unknown:
            print(f"  UNKNOWN CASE: {label} is listed in {args.known_failures} but not in the suite")

    if args.report:
        lines = [
            f"# Parity: {name_ref} vs {name_cmp}",
            "",
            f"{passed}/{len(rows)} cases match.",
            "",
            "| case | result | first difference |",
            "|---|---|---|",
        ]
        labels = {"PASS": "pass", "FAIL": "**fail**", "XFAIL": "known gap", "XPASS": "**now passes, delete from known failures**"}
        for label, diffs in rows:
            first = diffs[0].replace("|", "\\|") if diffs else ""
            lines.append(f"| {label} | {labels[verdict(label, diffs)]} | {first} |")
        Path(args.report).parent.mkdir(parents=True, exist_ok=True)
        Path(args.report).write_text("\n".join(lines) + "\n")

    if known:
        return 1 if new_failures or flipped or unknown else 0
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
