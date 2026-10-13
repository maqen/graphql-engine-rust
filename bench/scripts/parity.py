#!/usr/bin/env python3
"""Parity checker: sends every case in bench/queries to two engines (or to
one engine and the recorded golden responses in bench/expected) and diffs
the JSON responses. Doubles as the compatibility test suite.

  # live: original Hasura vs our engine
  bench/scripts/parity.py --a hasura=http://localhost:8080 --b rust=http://localhost:8081

  # our engine vs the recorded Hasura responses (no Hasura needed)
  bench/scripts/parity.py --a rust=http://localhost:8081 --golden

  # re-record the golden responses from Hasura
  bench/scripts/parity.py --a hasura=http://localhost:8080 --record

Comparison is strict: HTTP status, every value and its JSON type, and the
order of object keys (GraphQL requires responses to follow selection order).
Event-trigger deliveries counted by bench/handler are compared too.

Exit status is 1 if any case differs.
"""

from __future__ import annotations

import argparse
import json
import sys
import time

from suite import (
    EXPECTED_DIR,
    HANDLER_URL,
    RESULTS_DIR,
    Engine,
    _OrderedObject,
    http_get,
    http_post_json,
    load_suite,
    send,
)

EVENTS_FILE = "_events.json"
MAX_DIFFS = 20


def diff(a, b, path="$", out=None):
    """Collect human-readable differences between two parsed JSON values."""
    out = [] if out is None else out
    if len(out) >= MAX_DIFFS:
        return out
    if isinstance(a, dict) and isinstance(b, dict):
        missing = [k for k in a if k not in b]
        extra = [k for k in b if k not in a]
        for k in missing:
            out.append(f"{path}.{k}: missing in B")
        for k in extra:
            out.append(f"{path}.{k}: unexpected in B")
        for k in a:
            if k in b:
                diff(a[k], b[k], f"{path}.{k}", out)
        common_a = [k for k in _keys(a) if k in b]
        common_b = [k for k in _keys(b) if k in a]
        if common_a != common_b and not missing and not extra:
            out.append(f"{path}: key order differs: A={common_a} B={common_b}")
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            out.append(f"{path}: array length A={len(a)} B={len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f"{path}[{i}]", out)
    elif _jtype(a) != _jtype(b):
        out.append(f"{path}: type A={_jtype(a)}({_short(a)}) B={_jtype(b)}({_short(b)})")
    elif a != b:
        out.append(f"{path}: A={_short(a)} B={_short(b)}")
    return out


def _keys(d):
    return d.order if isinstance(d, _OrderedObject) else list(d)


def _jtype(v):
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, (int, float)):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    return "object"


def _short(v, n=80):
    s = json.dumps(v)
    return s if len(s) <= n else s[: n - 3] + "..."


def _parse(resp):
    try:
        return resp.json()
    except ValueError:
        return {"__non_json_body__": resp.body.decode(errors="replace")}


def _load_golden(name):
    with open(EXPECTED_DIR / f"{name}.json") as f:
        return json.load(f, object_pairs_hook=_OrderedObject)


def reset_events():
    try:
        http_post_json(HANDLER_URL + "/events/reset", {})
        return True
    except OSError:
        return False


def wait_events(target=None, timeout=20.0, settle=3.0):
    """Poll the handler's delivery counters. With a target, wait until they
    match it; otherwise wait until they stop changing for `settle` seconds."""
    deadline = time.monotonic() + timeout
    last, last_change = None, time.monotonic()
    while time.monotonic() < deadline:
        _, body = http_get(HANDLER_URL + "/events/stats")
        cur = json.loads(body)
        if target is not None and cur == target:
            return cur
        if cur != last:
            last, last_change = cur, time.monotonic()
        elif target is None and time.monotonic() - last_change >= settle:
            return cur
        time.sleep(0.25)
    return last


def run_engine(engine, cases):
    results = {}
    for case in cases:
        resp = send(engine, case)
        results[case.name] = {"status": resp.status, "body": _parse(resp)}
    return results


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--a", required=True, help="reference engine, name=url")
    ap.add_argument("--b", help="engine to compare, name=url")
    ap.add_argument("--golden", action="store_true", help="compare --a against bench/expected")
    ap.add_argument("--record", action="store_true", help="write --a responses to bench/expected")
    ap.add_argument("--only", action="append", help="regex filter on case names (repeatable)")
    ap.add_argument("--no-events", action="store_true", help="skip the event-trigger delivery check")
    ap.add_argument("--report", help="write a markdown report to this path")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    if sum(bool(x) for x in (args.b, args.golden, args.record)) != 1:
        ap.error("pass exactly one of --b, --golden, --record")

    cases = [c for c in load_suite(args.only) if c.parity]
    eng_a = Engine.parse(args.a)
    check_events = not args.no_events and not args.only

    if check_events:
        reset_events()
    res_a = run_engine(eng_a, cases)
    events_a = wait_events() if check_events else None

    if args.record:
        EXPECTED_DIR.mkdir(exist_ok=True)
        for name, r in res_a.items():
            with open(EXPECTED_DIR / f"{name}.json", "w") as f:
                json.dump(r, f, indent=2, ensure_ascii=False)
                f.write("\n")
        if events_a is not None:
            with open(EXPECTED_DIR / EVENTS_FILE, "w") as f:
                json.dump(events_a, f, indent=2, sort_keys=True)
                f.write("\n")
        print(f"recorded {len(res_a)} responses from {eng_a.name} into {EXPECTED_DIR}")
        if events_a is not None:
            print(f"event deliveries: {events_a}")
        return 0

    if args.golden:
        name_b = "golden"
        res_ref = {c.name: _load_golden(c.name) for c in cases}
        res_cmp = res_a
        name_ref, name_cmp = "golden", eng_a.name
        events_ref = json.loads((EXPECTED_DIR / EVENTS_FILE).read_text()) if check_events else None
        events_cmp = events_a
        if check_events and events_cmp != events_ref:
            events_cmp = wait_events(target=events_ref)
    else:
        eng_b = Engine.parse(args.b)
        name_b = eng_b.name
        if check_events:
            reset_events()
        res_b = run_engine(eng_b, cases)
        events_b = wait_events(target=events_a) if check_events else None
        res_ref, res_cmp = res_a, res_b
        name_ref, name_cmp = eng_a.name, name_b
        events_ref, events_cmp = events_a, events_b

    rows, failures = [], 0
    for case in cases:
        ref, cmp_ = res_ref[case.name], res_cmp[case.name]
        diffs = []
        if ref["status"] != cmp_["status"]:
            diffs.append(f"HTTP status A={ref['status']} B={cmp_['status']}")
        diffs += diff(ref["body"], cmp_["body"])
        ok = not diffs
        failures += not ok
        rows.append((case, ok, diffs))

    if check_events:
        ev_ok = events_ref == events_cmp
        failures += not ev_ok
        rows.append((None, ev_ok, [] if ev_ok else [f"event deliveries A={events_ref} B={events_cmp}"]))

    passed = len(rows) - failures
    print(f"parity {name_ref} (A) vs {name_cmp} (B): {passed}/{len(rows)} match")
    for case, ok, diffs in rows:
        label = case.name if case else "event_trigger_deliveries"
        print(f"  {'PASS' if ok else 'FAIL'}  {label}")
        if not ok:
            for d in diffs[: (MAX_DIFFS if args.verbose else 5)]:
                print(f"          {d}")

    if args.report:
        lines = [
            f"# Parity: {name_ref} vs {name_cmp}",
            "",
            f"{passed}/{len(rows)} cases match.",
            "",
            "| case | result | first difference |",
            "|---|---|---|",
        ]
        for case, ok, diffs in rows:
            label = case.name if case else "event_trigger_deliveries"
            first = diffs[0].replace("|", "\\|") if diffs else ""
            lines.append(f"| {label} | {'pass' if ok else '**fail**'} | {first} |")
        RESULTS_DIR.mkdir(exist_ok=True)
        with open(args.report, "w") as f:
            f.write("\n".join(lines) + "\n")

    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
