#!/usr/bin/env python3
"""Load runner: drives every bench case in bench/queries against one or more
engines with oha (https://github.com/hatoo/oha) and reports latency
percentiles, throughput, CPU and memory per engine, plus cold-start time.

  bench/scripts/run.py \
      --engine hasura=http://localhost:8080@hasura \
      --engine rust=http://localhost:8081@rust-engine \
      --label 2026-10-13-baseline

`name=url@service`: the optional compose service enables memory/CPU sampling
(docker stats) and startup measurement (docker compose stop/start). Engines
are interleaved per case so drift on a shared machine hits both equally.

Results go to bench/results/<label>/results.json and summary.md.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

from suite import COMPOSE_FILE, RESULTS_DIR, Engine, http_get, load_suite, send


# --------------------------------------------------------------------------
# docker helpers


class Compose:
    def __init__(self, extra_args: list[str]):
        self.base = ["docker", "compose", "-f", str(COMPOSE_FILE), *extra_args]

    def run(self, *args: str, check=True) -> str:
        p = subprocess.run([*self.base, *args], capture_output=True, text=True)
        if check and p.returncode != 0:
            raise RuntimeError(f"docker compose {' '.join(args)} failed: {p.stderr.strip()}")
        return p.stdout.strip()

    def container_id(self, service: str) -> str | None:
        out = self.run("ps", "-q", service, check=False)
        return out.splitlines()[0] if out else None


def _parse_size(s: str) -> float:
    """'123.4MiB' -> bytes."""
    s = s.strip()
    units = {"B": 1, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3, "TiB": 1024**4,
             "kB": 1e3, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12}
    for u in sorted(units, key=len, reverse=True):
        if s.endswith(u):
            return float(s[: -len(u)]) * units[u]
    return float(s)


class StatsSampler:
    """Streams `docker stats` for one container in the background and keeps
    peak memory and mean CPU while running."""

    def __init__(self, container: str):
        self.container = container
        self.mem: list[float] = []
        self.cpu: list[float] = []
        self._proc = None
        self._thread = None

    def __enter__(self):
        self._proc = subprocess.Popen(
            ["docker", "stats", "--format", "{{json .}}", self.container],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
        )
        self._thread = threading.Thread(target=self._read, daemon=True)
        self._thread.start()
        return self

    def _read(self):
        for line in self._proc.stdout:
            # Streaming output wraps each JSON line in terminal escape codes.
            i, j = line.find("{"), line.rfind("}")
            if i < 0 or j < i:
                continue
            try:
                d = json.loads(line[i : j + 1])
                self.mem.append(_parse_size(d["MemUsage"].split("/")[0]))
                self.cpu.append(float(d["CPUPerc"].rstrip("%")))
            except (ValueError, KeyError):
                continue

    def __exit__(self, *exc):
        self._proc.terminate()
        self._proc.wait()
        self._thread.join(timeout=2)

    def summary(self) -> dict:
        # docker stats' first sample has no CPU delta yet; skip it.
        cpu = self.cpu[1:] or self.cpu
        return {
            "peak_mem_mb": round(max(self.mem) / 1024**2, 1) if self.mem else None,
            "mean_cpu_cores": round(statistics.mean(cpu) / 100, 2) if cpu else None,
            "samples": len(self.mem),
        }


def snapshot_mem_mb(container: str) -> float | None:
    p = subprocess.run(
        ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", container],
        capture_output=True, text=True,
    )
    if p.returncode != 0 or not p.stdout.strip():
        return None
    return round(_parse_size(p.stdout.split("/")[0]) / 1024**2, 1)


def image_info(container: str) -> dict:
    p = subprocess.run(["docker", "inspect", "-f", "{{.Config.Image}}|{{.Image}}", container],
                       capture_output=True, text=True)
    if p.returncode != 0:
        return {}
    name, digest = p.stdout.strip().split("|")
    q = subprocess.run(["docker", "image", "inspect", "-f", "{{.Size}}", digest],
                       capture_output=True, text=True)
    size = int(q.stdout.strip()) if q.returncode == 0 and q.stdout.strip() else None
    return {"image": name, "image_size_mb": round(size / 1024**2, 1) if size else None}


# --------------------------------------------------------------------------
# measurements


def wait_healthy(engine: Engine, timeout: float = 180.0, interval: float = 0.02) -> float:
    """Seconds until GET /healthz returns 200; raises on timeout."""
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout:
        try:
            status, _ = http_get(engine.url + "/healthz", timeout=1)
            if status == 200:
                return time.monotonic() - t0
        except OSError:
            pass
        time.sleep(interval)
    raise TimeoutError(f"{engine.name} not healthy after {timeout}s")


def measure_startup(compose: Compose, engine: Engine, runs: int) -> dict:
    times = []
    for _ in range(runs):
        compose.run("stop", engine.service)
        t0 = time.monotonic()
        compose.run("start", engine.service)
        wait_healthy(engine)
        times.append(time.monotonic() - t0)
    return {"runs_s": [round(t, 3) for t in times], "median_s": round(statistics.median(times), 3)}


def oha_cmd(oha: str, engine: Engine, case, duration: str, concurrency: int) -> list[str]:
    cmd = [oha, "-z", duration, "-c", str(concurrency), "--no-tui", "--output-format", "json",
           "-w", "-m", case.method()]
    for k, v in case.headers().items():
        cmd += ["-H", f"{k}: {v}"]
    body = case.body()
    if body is not None:
        cmd += ["-d", body.decode()]
    return cmd + [engine.url + case.path()]


def run_oha(cmd: list[str]) -> dict:
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"oha failed: {p.stderr.strip()[:500]}")
    return json.loads(p.stdout)


def validate(engine: Engine, case) -> str | None:
    """A load run is only meaningful if the request succeeds; GraphQL errors
    come back as HTTP 200, so check the body too."""
    resp = send(engine, case)
    if resp.status != 200:
        return f"HTTP {resp.status}: {resp.body[:200]!r}"
    try:
        body = json.loads(resp.body)
    except ValueError:
        return f"non-JSON body: {resp.body[:200]!r}"
    if isinstance(body, dict) and "errors" in body:
        return f"GraphQL errors: {json.dumps(body['errors'])[:200]}"
    return None


def summarise_oha(d: dict) -> dict:
    s, lp = d["summary"], d["latencyPercentiles"]
    codes = d.get("statusCodeDistribution", {})
    total = sum(codes.values())
    ok = codes.get("200", 0)
    return {
        "rps": round(s["requestsPerSec"], 1),
        "p50_ms": round(lp["p50"] * 1000, 2),
        "p95_ms": round(lp["p95"] * 1000, 2),
        "p99_ms": round(lp["p99"] * 1000, 2),
        "max_ms": round(s["slowest"] * 1000, 2),
        "requests": total,
        "non_200": total - ok,
        "transport_errors": sum(d.get("errorDistribution", {}).values()),
        "bytes_per_request": s.get("sizePerRequest"),
    }


# --------------------------------------------------------------------------
# reporting


def write_summary(path: Path, result: dict):
    engines = [e["name"] for e in result["engines"]]
    lines = [f"# Benchmark: {result['label']}", ""]
    m = result["machine"]
    lines += [
        f"- Date: {result['started_at']}",
        f"- Machine: {m['cpus']} CPUs ({m['cpu_model']}), {m['os']}",
        f"- Load: oha, {result['config']['duration']} per case after {result['config']['warmup']} warmup, "
        f"concurrency {', '.join(map(str, result['config']['concurrency']))}",
        f"- Engine limits: {result['config']['engine_limits']}",
        "",
        "## Engines",
        "",
        "| engine | image | image size | idle memory | startup (median) |",
        "|---|---|---|---|---|",
    ]
    for e in result["engines"]:
        st = e.get("startup") or {}
        lines.append(
            f"| {e['name']} | {e.get('image', '-')} | {_fmt(e.get('image_size_mb'), ' MB')} | "
            f"{_fmt(e.get('idle_mem_mb'), ' MB')} | {_fmt(st.get('median_s'), ' s')} |"
        )
    for extra in result.get("startup_only", []):
        st = extra["startup"]
        lines.append(f"| {extra['name']} (startup only) | {extra.get('image', '-')} | "
                     f"{_fmt(extra.get('image_size_mb'), ' MB')} | - | {_fmt(st.get('median_s'), ' s')} |")
    lines += ["", "## Latency and throughput", ""]
    header = "| case | c | " + " | ".join(
        f"{e} rps | {e} p50 | {e} p95 | {e} p99 | {e} peak MB | {e} CPU" for e in engines)
    if len(engines) == 2:
        header += f" | rps {engines[1]}/{engines[0]}"
    lines.append(header + " |")
    ncols = 2 + 6 * len(engines) + (1 if len(engines) == 2 else 0)
    lines.append("|" + "---|" * ncols)
    for row in result["cases"]:
        cells = [row["case"], str(row["concurrency"])]
        rps = []
        for e in engines:
            r = row["engines"].get(e)
            if not r or "error" in r:
                cells += [f"error: {r['error'][:40]}" if r else "-"] + [""] * 5
                rps.append(None)
                continue
            cells += [f"{r['rps']:.0f}", f"{r['p50_ms']} ms", f"{r['p95_ms']} ms", f"{r['p99_ms']} ms",
                      _fmt(r.get("peak_mem_mb")), _fmt(r.get("mean_cpu_cores"))]
            rps.append(r["rps"])
        if len(engines) == 2:
            cells.append(f"{rps[1] / rps[0]:.2f}x" if all(rps) else "-")
        lines.append("| " + " | ".join(cells) + " |")
    path.write_text("\n".join(lines) + "\n")


def _fmt(v, suffix=""):
    return "-" if v is None else f"{v}{suffix}"


# --------------------------------------------------------------------------


def machine_info() -> dict:
    model = platform.processor() or "unknown"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    return {"cpus": os.cpu_count(), "cpu_model": model, "os": f"{platform.system()} {platform.release()}"}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--engine", action="append", required=True, help="name=url[@compose-service]")
    ap.add_argument("--startup-only", action="append", default=[],
                    help="name=url@service measured for startup time only (e.g. hasura-serve)")
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--warmup", default="3s")
    ap.add_argument("--concurrency", type=int, action="append", help="repeatable; default 32")
    ap.add_argument("--only", action="append", help="regex filter on case names (repeatable)")
    ap.add_argument("--startup-runs", type=int, default=3, help="0 to skip startup measurement")
    ap.add_argument("--label", default=dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d-%H%M"))
    ap.add_argument("--compose-arg", action="append", default=[],
                    help="extra docker compose argument, e.g. --compose-arg=--env-file=bench/.env.mirror")
    ap.add_argument("--oha", default=shutil.which("oha") or os.path.expanduser("~/.cargo/bin/oha"))
    args = ap.parse_args()
    concurrency = args.concurrency or [32]

    if not Path(args.oha).exists():
        sys.exit("oha not found: install with `cargo install oha` or pass --oha")

    compose = Compose(args.compose_arg)
    engines = [Engine.parse(s) for s in args.engine]
    cases = [c for c in load_suite(args.only) if c.bench]
    out_dir = RESULTS_DIR / args.label
    out_dir.mkdir(parents=True, exist_ok=True)

    result = {
        "label": args.label,
        "started_at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "machine": machine_info(),
        "config": {
            "duration": args.duration, "warmup": args.warmup, "concurrency": concurrency,
            "engine_limits": f"cpus={os.environ.get('ENGINE_CPUS', '2')}, mem={os.environ.get('ENGINE_MEM', '2g')}",
        },
        "engines": [],
        "cases": [],
    }

    containers = {}
    for e in engines:
        wait_healthy(e)
        info = {"name": e.name, "url": e.url, "service": e.service}
        if e.service:
            cid = compose.container_id(e.service)
            if cid:
                containers[e.name] = cid
                info.update(image_info(cid))
                info["idle_mem_mb"] = snapshot_mem_mb(cid)
        result["engines"].append(info)

    for case in cases:
        for c in concurrency:
            row = {"case": case.name, "description": case.description, "concurrency": c, "engines": {}}
            for e in engines:
                err = validate(e, case)
                if err:
                    row["engines"][e.name] = {"error": err}
                    print(f"{case.name:32} c={c:<4} {e.name:10} SKIP {err}", flush=True)
                    continue
                run_oha(oha_cmd(args.oha, e, case, args.warmup, c))
                cid = containers.get(e.name)
                if cid:
                    with StatsSampler(cid) as sampler:
                        d = run_oha(oha_cmd(args.oha, e, case, args.duration, c))
                    stats = sampler.summary()
                else:
                    d, stats = run_oha(oha_cmd(args.oha, e, case, args.duration, c)), {}
                r = summarise_oha(d) | stats
                row["engines"][e.name] = r
                print(f"{case.name:32} c={c:<4} {e.name:10} {r['rps']:>9.0f} rps  p50 {r['p50_ms']:>7} ms  "
                      f"p99 {r['p99_ms']:>7} ms  mem {r.get('peak_mem_mb')} MB", flush=True)
            result["cases"].append(row)
            (out_dir / "results.json").write_text(json.dumps(result, indent=2) + "\n")

    if args.startup_runs > 0:
        for info, e in zip(result["engines"], engines):
            if e.service:
                info["startup"] = measure_startup(compose, e, args.startup_runs)
                print(f"startup {e.name}: {info['startup']}", flush=True)
        result["startup_only"] = []
        for spec in args.startup_only:
            e = Engine.parse(spec)
            compose.run("up", "-d", "--no-deps", e.service)
            wait_healthy(e)
            extra = {"name": e.name, "service": e.service}
            cid = compose.container_id(e.service)
            if cid:
                extra.update(image_info(cid))
            extra["startup"] = measure_startup(compose, e, args.startup_runs)
            compose.run("stop", e.service)
            print(f"startup {e.name}: {extra['startup']}", flush=True)
            result["startup_only"].append(extra)

    (out_dir / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    write_summary(out_dir / "summary.md", result)
    print(f"\nwrote {out_dir / 'summary.md'}")


if __name__ == "__main__":
    main()
