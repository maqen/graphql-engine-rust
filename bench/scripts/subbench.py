#!/usr/bin/env python3
"""Subscription benchmark: holds thousands of live-query subscribers (plus
streaming subscribers) open against each engine, then writes to Postgres
and measures how fast every subscriber sees the change.

  bench/scripts/subbench.py \
      --engine hasura=http://localhost:8080@hasura \
      --engine rust=http://localhost:8081@rust-engine \
      --subscribers 1000 --subscribers 5000 --label 2026-10-13-subs

Per engine and subscriber count it reports:
  - time until every subscriber is connected and has its initial result;
  - memory: freshly restarted container vs. subscribed (medians of docker
    stats samples), and per subscriber;
  - CPU (cores) while idle-subscribed (pure polling cost) and during writes;
  - Postgres transactions/s in the engine's database while subscribed,
    minus the same engine's rate with no subscribers (its polling load);
  - update latency: write committed -> each subscriber has the new result
    (p50/p95/p99/max), and subscribers that never saw it;
  - streaming: insert committed -> row delivered to each stream subscriber;
  - client event-loop lag, so a saturated client is visible.

Writes go straight to Postgres (psql on the published port), so the write
path is identical for every engine. The engine container is restarted
before each run for a clean memory baseline, engines alternate order per
subscriber count, and client workers are pinned off the engines' cores.
"""

from __future__ import annotations

import argparse
import asyncio
import datetime as dt
import json
import multiprocessing as mp
import os
import random
import resource
import statistics
import subprocess
import sys
import time
from pathlib import Path

from run import Compose, StatsSampler, container_url, machine_info, wait_healthy
from suite import ADMIN_SECRET, RESULTS_DIR, Engine
from wsclient import GraphQLSubscription

PG = os.environ.get("BENCH_PG", "postgres://postgres:postgres@localhost:5432")
ENGINE_CPUSET = os.environ.get("ENGINE_CPUSET", "0,1")

# (name, role, query, variables(j), user_id(j)); j = i // len(SHAPES) so
# every shape sees all of its distinct ids.
SHAPES = [
    ("user_rows", "user",
     "subscription { playlist(order_by: {playlist_id: asc}) { playlist_id name } }",
     lambda j: None, lambda j: str(j % 20 + 1)),
    ("admin_by_pk", None,
     "subscription ($id: Int!) { playlist_by_pk(playlist_id: $id) { playlist_id name owner { first_name } } }",
     lambda j: {"id": j % 100 + 1}, lambda j: None),
    ("user_relationship_aggregate", "user",
     "subscription { customer { customer_id invoices_aggregate { aggregate { count sum { total } } } "
     "playlists(order_by: {playlist_id: asc}, limit: 2) { name } } }",
     lambda j: None, lambda j: str(j % 20 + 1)),
    ("shared", None,
     "subscription { playlist(where: {playlist_id: {_lte: 10}}, order_by: {playlist_id: asc}) { playlist_id name } }",
     lambda j: None, lambda j: None),
]
SHAPE_DESC = ("4 shapes: user-role rows (20 user ids = 20 cohorts), admin playlist_by_pk (100 distinct ids), "
              "user-role relationship + aggregate (20 user ids), one query shared by all")
# A write renames playlists 1..100, which changes every shape's result.
WRITE_SQL = "UPDATE public.playlist SET name = 'Playlist ' || playlist_id || ' #{tok}' WHERE playlist_id <= 100"
RESTORE_SQL = "UPDATE public.playlist SET name = 'Playlist ' || playlist_id WHERE playlist_id <= 100"
STREAM_BASE = 900000  # ids for streamed rows, far above anything else
CLEANUP_SQL = f"DELETE FROM public.playlist WHERE playlist_id >= {STREAM_BASE}"
# replica mode skips the playlist_created event trigger, so streamed rows
# don't add webhook deliveries to the measurement.
STREAM_SQL = ("SET session_replication_role = replica \\; INSERT INTO public.playlist "
              "(playlist_id, name, owner_id, created_at) VALUES ({id}, 'Stream S{k}', 1, '2024-01-01T00:00:00Z') "
              "\\; RESET session_replication_role")
STREAM_QUERY = ("subscription ($from: Int!) { playlist_stream(batch_size: 10, "
                "cursor: {initial_value: {playlist_id: $from}, ordering: ASC}) { playlist_id name } }")


def headers(role, user_id):
    h = {"x-hasura-admin-secret": ADMIN_SECRET}
    if role:
        h["x-hasura-role"] = role
    if user_id:
        h["x-hasura-user-id"] = user_id
    return h


def _tokens(raw):
    """Write tokens (" #<n>" -> "w<n>") and stream tokens ("Stream S<n>" -> "s<n>")."""
    toks = set()
    for marker, prefix in ((" #", "w"), ("Stream S", "s")):
        start = 0
        while (j := raw.find(marker, start)) >= 0:
            k = j + len(marker)
            while k < len(raw) and raw[k].isdigit():
                k += 1
            if k > j + len(marker):
                toks.add(prefix + raw[j + len(marker):k])
            start = k
    return toks


# --------------------------------------------------------------------------
# worker processes: hold sockets, record when each token first arrives


async def _subscriber(spec, ws_url, protocol, out, stop, ready):
    kind, i = spec
    first = False
    try:
        if kind == "stream":
            sub = await GraphQLSubscription.open(ws_url, protocol, headers(None, None), STREAM_QUERY,
                                                 {"from": STREAM_BASE}, timeout=120)
        else:
            _, role, query, variables, user_id = SHAPES[i % len(SHAPES)]
            j = i // len(SHAPES)
            sub = await GraphQLSubscription.open(ws_url, protocol, headers(role, user_id(j)), query,
                                                 variables(j), timeout=120)
    except asyncio.CancelledError:
        ready.release()
        raise
    except Exception as e:  # noqa: BLE001 - any failure is a data point, never a hang
        out["errors"].append(f"connect: {type(e).__name__}: {e}"[:200])
        ready.release()
        return
    seen = out["seen"].setdefault(f"{kind}:{i}", {})
    if kind == "stream":  # streams send nothing until a row arrives
        first = True
        out["initial"].append(time.monotonic())
        ready.release()
    try:
        while not stop.is_set():
            msg_kind, raw = await sub.next_raw()
            now = time.monotonic()
            if msg_kind != "data":
                out["errors"].append(f"{msg_kind}: {raw[:200]}")
                break
            if '"errors"' in raw:
                out["errors"].append(f"graphql error: {raw[:200]}")
            if not first:
                first = True
                out["initial"].append(now)
                ready.release()
                # A token already in the initial result was written before
                # this subscriber started: it is no update latency sample.
                for tok in _tokens(raw):
                    seen.setdefault(tok, None)
                continue
            for tok in _tokens(raw):
                seen.setdefault(tok, now)
    except asyncio.CancelledError:
        pass
    except Exception as e:  # noqa: BLE001
        if not stop.is_set():
            out["errors"].append(f"dropped: {type(e).__name__}: {e}"[:200])
    finally:
        if not first:  # failed before its initial result: don't block the start barrier
            ready.release()
        await sub.close()


async def _lag_probe(out, stop, interval=0.01):
    """How late the event loop wakes up: client saturation shows up here."""
    while not stop.is_set():
        t = time.monotonic()
        await asyncio.sleep(interval)
        out["lag"].append(time.monotonic() - t - interval)


def worker_main(specs, ws_url, protocol, conn, rate, cpus):
    try:
        if cpus:
            os.sched_setaffinity(0, cpus)
        _, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        resource.setrlimit(resource.RLIMIT_NOFILE, (hard, hard))
        if hard < len(specs) + 100:
            raise RuntimeError(f"open-file limit {hard} too low for {len(specs)} sockets; raise ulimit -n")
        asyncio.run(_worker(specs, ws_url, protocol, conn, rate))
    except BaseException as e:  # noqa: BLE001 - always tell the parent
        conn.send(("error", f"{type(e).__name__}: {e}"))
        raise


async def _worker(specs, ws_url, protocol, conn, rate):
    out = {"errors": [], "initial": [], "seen": {}, "lag": []}
    stop, ready = asyncio.Event(), asyncio.Semaphore(0)
    tasks = []
    for spec in specs:
        tasks.append(asyncio.create_task(_subscriber(spec, ws_url, protocol, out, stop, ready)))
        await asyncio.sleep(1 / rate)  # pace connects like real clients
    deadline = time.monotonic() + 300
    pending = len(specs)
    while pending:
        try:
            await asyncio.wait_for(ready.acquire(), timeout=max(0.1, deadline - time.monotonic()))
            pending -= 1
        except asyncio.TimeoutError:
            out["errors"].append(f"{pending} subscribers had no initial result after 300 s")
            break
    conn.send(("ready", len(out["errors"])))
    probe = asyncio.create_task(_lag_probe(out, stop))
    await asyncio.get_running_loop().run_in_executor(None, conn.recv)  # wait for "stop"
    stop.set()
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, probe, return_exceptions=True)
    conn.send(("done", out))


class Workers:
    def __init__(self, specs, ws_url, protocol, n, rate, cpus):
        self.procs = []
        for w in range(n):
            parent, child = mp.Pipe()
            p = mp.Process(target=worker_main, args=(specs[w::n], ws_url, protocol, child, rate / n, cpus))
            p.start()
            child.close()
            self.procs.append((p, parent))

    def collect(self, expect, timeout):
        """One message per worker; fails fast if a worker dies or errors."""
        results, deadline = [], time.monotonic() + timeout
        for p, parent in self.procs:
            while not parent.poll(0.5):
                if not p.is_alive():
                    raise RuntimeError(f"client worker {p.pid} died (exit {p.exitcode})")
                if time.monotonic() > deadline:
                    raise RuntimeError(f"client worker {p.pid} timed out waiting for {expect!r}")
            kind, payload = parent.recv()
            if kind == "error":
                raise RuntimeError(f"client worker failed: {payload}")
            results.append(payload)
        return results

    def stop(self):
        for _, parent in self.procs:
            try:
                parent.send("stop")
            except (BrokenPipeError, OSError):
                pass

    def terminate(self):
        for p, _ in self.procs:
            p.join(timeout=10)
            if p.is_alive():
                p.terminate()
                p.join(timeout=5)


# --------------------------------------------------------------------------
# Postgres side


class Psql:
    """Long-lived psql session so each write is timed without process startup."""

    def __init__(self, db):
        self.p = subprocess.Popen(["psql", f"{PG}/{db}", "-X", "-q", "-At", "-v", "ON_ERROR_STOP=1"],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
                                  close_fds=True)
        self.n = 0

    def run(self, sql) -> float:
        """Execute `sql` and a marker in one round trip (`\\;` sends them as a
        single query); returns the monotonic time once it has committed."""
        self.n += 1
        marker = f"__done_{self.n}"
        self.p.stdin.write(f"{sql} \\; SELECT '{marker}';\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("psql exited")
            if line.strip() == marker:
                return time.monotonic()

    def scalar(self, sql) -> str:
        self.p.stdin.write(f"{sql};\n")
        self.p.stdin.flush()
        return self.p.stdout.readline().strip()

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)


def xacts(psql, db) -> int:
    # Flush this backend's pending stats so the counter is current.
    return int(psql.scalar(f"SELECT pg_stat_force_next_flush(), (SELECT xact_commit + xact_rollback "
                           f"FROM pg_stat_database WHERE datname = '{db}')").split("|")[-1])


def tps(psql, db, seconds) -> float:
    psql.scalar("SELECT pg_sleep(1.5)")  # let backends flush their stats
    x0, t0 = xacts(psql, db), time.monotonic()
    time.sleep(seconds)
    psql.scalar("SELECT pg_sleep(1.5)")
    return (xacts(psql, db) - x0 - 4) / (time.monotonic() - t0)  # minus our own 4 statements


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))] * 1000, 1)


def lat_summary(lats, expected):
    return {"p50_ms": pct(lats, 50), "p95_ms": pct(lats, 95), "p99_ms": pct(lats, 99), "max_ms": pct(lats, 100),
            "samples": len(lats), "missed": expected - len(lats)}


def parse_cpuset(spec: str) -> set[int]:
    """Docker cpuset syntax: "0,1", "0-3", "0-1,4"."""
    cpus = set()
    for part in filter(None, (p.strip() for p in spec.split(","))):
        lo, _, hi = part.partition("-")
        cpus.update(range(int(lo), int(hi or lo) + 1))
    return cpus


def median_mem(cid, seconds):
    with StatsSampler(cid) as s:
        time.sleep(seconds)
    return round(statistics.median(s.mem) / 1024**2, 1) if s.mem else None, s


# --------------------------------------------------------------------------


def run_one(engine, db, n, args, compose):
    with_stats = bool(engine.service)
    psql = Psql(db)
    psql.run(RESTORE_SQL)
    psql.run(CLEANUP_SQL)
    if with_stats:  # clean memory baseline
        compose.run("restart", engine.service)
        wait_healthy(engine)
    cid = compose.container_id(engine.service) if with_stats else None
    # Connect to the container directly: Docker's userland port proxy can't
    # hold thousands of sockets.
    base_url = (container_url(cid) if cid and not args.via_proxy else None) or engine.url
    ws_url = base_url.replace("http", "ws", 1) + "/v1/graphql"
    time.sleep(3)
    base_mem = median_mem(cid, 5)[0] if cid else None
    base_tps = tps(psql, db, args.idle_seconds)

    specs = [("live", i) for i in range(n)] + [("stream", i) for i in range(args.stream_subscribers)]
    all_cpus = set(range(os.cpu_count()))
    engine_cpus = parse_cpuset(ENGINE_CPUSET)
    client_cpus = (all_cpus - engine_cpus) or all_cpus
    writes, streams = {}, {}
    t0 = time.monotonic()
    workers = Workers(specs, ws_url, args.protocol, args.workers, args.connect_rate, client_cpus)
    try:
        connect_errors = sum(workers.collect("ready", timeout=400))
        ready_s = time.monotonic() - t0
        print(f"  {engine.name} n={n}: {len(specs)} subscribers ready in {ready_s:.1f}s "
              f"({connect_errors} errors)", flush=True)

        # Idle-subscribed phase: nothing changes, so CPU and DB load are pure polling.
        time.sleep(3)
        if cid:
            with StatsSampler(cid) as idle_stats:
                sub_tps = tps(psql, db, args.idle_seconds)
            idle = idle_stats.summary()
            sub_mem = round(statistics.median(idle_stats.mem) / 1024**2, 1) if idle_stats.mem else None
        else:
            sub_tps, idle, sub_mem = tps(psql, db, args.idle_seconds), {}, None

        # Write phase: live-query updates, then streamed inserts.
        sampler = StatsSampler(cid) if cid else None
        if sampler:
            sampler.__enter__()
        for k in range(1, args.writes + 1):
            writes[f"w{k}"] = psql.run(WRITE_SQL.format(tok=k))
            # Jitter so writes don't phase-lock with the engines' refetch tick.
            time.sleep(args.write_interval + random.random())
        for k in range(1, args.stream_rows + 1):
            streams[f"s{k}"] = psql.run(STREAM_SQL.format(id=STREAM_BASE + k, k=k))
            time.sleep(args.write_interval / 2 + random.random())
        time.sleep(args.settle)
        busy = sampler.summary() if sampler else {}
        if sampler:
            sampler.__exit__(None, None, None)

        workers.stop()
        outs = workers.collect("done", timeout=120)
    finally:
        workers.stop()
        workers.terminate()
        psql.close()
        cleanup = Psql(db)  # fresh session: the old one may be mid-statement
        cleanup.run(RESTORE_SQL)
        cleanup.run(CLEANUP_SQL)
        cleanup.close()

    errors = [e for o in outs for e in o["errors"]]
    initial = sorted(t for o in outs for t in o["initial"])
    lag = [x for o in outs for x in o["lag"]]
    live_lat, stream_lat, live_expected, stream_expected = [], [], 0, 0
    for o in outs:
        for key, seen in o["seen"].items():
            # Writes already visible in the initial result can't be timed.
            if key.startswith("live"):
                timed = [t for t in writes if seen.get(t, 0) is not None]
                live_expected += len(timed)
                live_lat += [seen[t] - writes[t] for t in timed if t in seen]
            else:
                timed = [t for t in streams if seen.get(t, 0) is not None]
                stream_expected += len(timed)
                stream_lat += [seen[t] - streams[t] for t in timed if t in seen]
    n_all = n + args.stream_subscribers
    res = {
        "subscribers": n,
        "stream_subscribers": args.stream_subscribers,
        "ready_s": round(ready_s, 1),
        "all_initial_results_s": round(initial[-1] - t0, 1) if initial else None,
        "errors": len(errors),
        "error_samples": errors[:5],
        "baseline_mem_mb": base_mem,
        "subscribed_mem_mb": sub_mem,
        "kb_per_subscriber": round((sub_mem - base_mem) * 1024 / n_all, 1) if sub_mem and base_mem else None,
        "idle_cpu_cores": idle.get("mean_cpu_cores"),
        "write_cpu_cores": busy.get("mean_cpu_cores"),
        "peak_mem_mb": busy.get("peak_mem_mb"),
        "db_tps_no_subscribers": round(base_tps, 1),
        "db_tps_subscribed": round(sub_tps, 1),
        "db_tps_polling": round(sub_tps - base_tps, 1),
        "update_latency": lat_summary(live_lat, live_expected),
        "stream_latency": lat_summary(stream_lat, stream_expected),
        "client_loop_lag_p99_ms": pct(lag, 99),
        "client_loop_lag_max_ms": pct(lag, 100),
    }
    ul, sl = res["update_latency"], res["stream_latency"]
    print(f"  {engine.name} n={n}: mem {base_mem}->{sub_mem} MB ({res['kb_per_subscriber']} KB/sub), "
          f"cpu idle {res['idle_cpu_cores']} writes {res['write_cpu_cores']}, polling tx/s {res['db_tps_polling']}, "
          f"update p50 {ul['p50_ms']} p99 {ul['p99_ms']} missed {ul['missed']}, "
          f"stream p50 {sl['p50_ms']} missed {sl['missed']}, client lag p99 {res['client_loop_lag_p99_ms']} ms, "
          f"errors {len(errors)}", flush=True)
    return res


def write_summary(path: Path, result: dict):
    m, c = result["machine"], result["config"]
    L = [f"# Subscription benchmark: {result['label']}", "",
         f"- Date: {result['started_at']}",
         f"- Machine: {m['cpus']} CPUs ({m['cpu_model']}); engines pinned to cpuset {c['engine_cpuset']}, "
         f"client workers ({c['workers']}) to the other cores",
         f"- Protocol {c['protocol']}; {c['writes']} live-query writes {c['write_interval']} s apart, "
         f"{c['stream_rows']} streamed inserts; {c['stream_subscribers']} stream subscribers per run",
         f"- Live queries: {SHAPE_DESC}.",
         "- Latency runs from the write's commit to the subscriber's socket delivering the new result, so it "
         "includes each engine's refetch interval (1 s by default in both); writes are jittered so they land at "
         "random points of that tick, which puts the expected p50 near 0.5 s.",
         "- Memory: median while subscribed minus median of the freshly restarted engine. Polling tx/s: "
         "transactions/s in the engine's database while subscribed minus the same engine with no subscribers.",
         "- Client lag: how late the client's event loop woke up (p99); high values mean the client, not the "
         "engine, limited the latency numbers.",
         "",
         "| engine | subs | ready s | KB/sub | idle CPU | write CPU | polling tx/s | update p50 | update p99 | "
         "update max | missed | stream p50 | stream p99 | client lag p99 | errors |",
         "|" + "---|" * 15]
    for r in result["runs"]:
        u, s = r["update_latency"], r["stream_latency"]
        L.append(f"| {r['engine']} | {r['subscribers']} | {r['ready_s']} | {r['kb_per_subscriber']} | "
                 f"{r['idle_cpu_cores']} | {r['write_cpu_cores']} | {r['db_tps_polling']} | {u['p50_ms']} ms | "
                 f"{u['p99_ms']} ms | {u['max_ms']} ms | {u['missed']} | {s['p50_ms']} ms | {s['p99_ms']} ms | "
                 f"{r['client_loop_lag_p99_ms']} ms | {r['errors']} |")
    path.write_text("\n".join(L) + "\n")


def main():
    engine_cores = len(parse_cpuset(ENGINE_CPUSET))
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--engine", action="append", required=True, help="name=url[@compose-service]")
    ap.add_argument("--db", action="append", default=[], help="name=database (default: the engine name)")
    ap.add_argument("--subscribers", type=int, action="append", help="repeatable; default 1000")
    ap.add_argument("--stream-subscribers", type=int, default=50)
    ap.add_argument("--protocol", default="graphql-transport-ws", choices=["graphql-transport-ws", "graphql-ws"])
    ap.add_argument("--workers", type=int, default=max(1, (os.cpu_count() or 2) - engine_cores),
                    help="client processes (default: cores not used by the engines)")
    ap.add_argument("--connect-rate", type=float, default=500, help="new connections per second")
    ap.add_argument("--idle-seconds", type=float, default=20)
    ap.add_argument("--writes", type=int, default=10)
    ap.add_argument("--write-interval", type=float, default=2.0)
    ap.add_argument("--stream-rows", type=int, default=10)
    ap.add_argument("--settle", type=float, default=5.0, help="wait after the last write")
    ap.add_argument("--label", default=dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d-%H%M") + "-subs")
    ap.add_argument("--compose-arg", action="append", default=[])
    ap.add_argument("--via-proxy", action="store_true", help="connect via the published ports")
    args = ap.parse_args()

    compose = Compose(args.compose_arg)
    engines = [Engine.parse(s) for s in args.engine]
    dbs = dict(s.split("=", 1) for s in args.db)
    out_dir = RESULTS_DIR / args.label
    out_dir.mkdir(parents=True, exist_ok=True)
    result = {
        "label": args.label,
        "started_at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "machine": machine_info(),
        "config": {k: getattr(args, k) for k in ("protocol", "writes", "write_interval", "stream_rows",
                                                  "stream_subscribers", "workers", "connect_rate", "idle_seconds")}
        | {"engine_cpuset": ENGINE_CPUSET},
        "runs": [],
    }
    for step, n in enumerate(args.subscribers or [1000]):
        for e in (engines if step % 2 == 0 else engines[::-1]):
            if not e.service:
                wait_healthy(e)  # with a service, run_one restarts it and waits
            print(f"{e.name}: {n} live subscribers", flush=True)
            r = {"engine": e.name} | run_one(e, dbs.get(e.name, e.name), n, args, compose)
            result["runs"].append(r)
            (out_dir / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    write_summary(out_dir / "summary.md", result)
    print(f"\nwrote {out_dir / 'summary.md'}")


if __name__ == "__main__":
    mp.set_start_method("fork")
    sys.exit(main())
