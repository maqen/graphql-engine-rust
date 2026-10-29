#!/usr/bin/env python3
"""Run the official `hasura` CLI against Hasura V2 and against our engine, and diff the outcomes.

Each engine gets a fresh database and a fresh copy of the bench project (`bench/project`, never
modified). The CLI talks to the engine through a recording proxy, so every step is compared on

* the CLI's exit code and its output (timestamps and generated versions normalised),
* every HTTP request the CLI made and the engine's response (status and JSON body),
* the files the step left in the project directory (e.g. `metadata export`).

Steps are listed in SCENARIOS below. A step whose outcome differs between the engines fails the
run unless its id is in `expected-failures.txt`; a listed step that now matches also fails the
run, so the list only shrinks. Stdlib only, except the console scenario (console.py).

    cli-compat/run.py --hasura-cli /path/to/hasura --rust-bin target/debug/graphql-engine \
        --pg postgres://postgres:postgres@localhost:5432/postgres
"""

from __future__ import annotations

import argparse
import difflib
import http.client
import http.server
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
PROJECT = REPO / "bench" / "project"
ADMIN_SECRET = "benchsecret"
V2_IMAGE = "mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce"
V2_CLI_MIGRATIONS_IMAGE = "mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce.cli-migrations-v3"


# --------------------------------------------------------------------------------------------
# Recording proxy


@dataclass
class Exchange:
    seq: int  # order of arrival
    method: str
    path: str
    request: object
    status: int
    response: object
    response_type: str | None = None  # content-type, plus content-encoding when set


class Recorder:
    def __init__(self, upstream_port: int):
        self.upstream_port = upstream_port
        self.exchanges: list[Exchange] = []
        self.seq = 0
        self.lock = threading.Lock()

    def take(self) -> list[Exchange]:
        with self.lock:
            out, self.exchanges = self.exchanges, []
        return sorted(out, key=lambda e: e.seq)


def parse_body(raw: bytes, content_type: str | None) -> object:
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        text = sql_text(raw.decode("utf-8", "replace"))
        return {"<text>": text if len(text) < 4000 else text[:4000] + "…"}


def start_proxy(recorder: Recorder) -> tuple[http.server.ThreadingHTTPServer, int]:
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):
            pass

        def _forward(self):
            with recorder.lock:
                recorder.seq += 1
                seq = recorder.seq
            length = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(length) if length else b""
            conn = http.client.HTTPConnection("127.0.0.1", recorder.upstream_port, timeout=120)
            headers = {
                k: v
                for k, v in self.headers.items()
                if k.lower() not in ("host", "accept-encoding", "connection")
            }
            try:
                conn.request(self.command, self.path, body=body, headers=headers)
                resp = conn.getresponse()
                data = resp.read()
                status, resp_headers = resp.status, resp.getheaders()
            except OSError as e:
                data = json.dumps({"proxy_error": str(e)}).encode()
                status, resp_headers = 599, [("content-type", "application/json")]
            finally:
                conn.close()
            lower = dict((k.lower(), v) for k, v in resp_headers)
            ctype = lower.get("content-type")
            rtype = " ".join(filter(None, [ctype, lower.get("content-encoding")])) or None
            with recorder.lock:
                recorder.exchanges.append(
                    Exchange(
                        seq,
                        self.command,
                        self.path,
                        parse_body(body, self.headers.get("content-type")),
                        status,
                        parse_body(data, ctype),
                        rtype,
                    )
                )
            self.send_response(status)
            for k, v in resp_headers:
                if k.lower() not in ("transfer-encoding", "content-length", "connection"):
                    self.send_header(k, v)
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_OPTIONS = _forward

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


# --------------------------------------------------------------------------------------------
# Engines


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def psql(pg_url: str, sql: str) -> None:
    subprocess.run(["psql", pg_url, "-v", "ON_ERROR_STOP=1", "-qc", sql], check=True)


def fresh_db(pg_url: str, name: str) -> str:
    psql(pg_url, f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
    psql(pg_url, f'CREATE DATABASE "{name}"')
    parts = urllib.parse.urlsplit(pg_url)
    return urllib.parse.urlunsplit(parts._replace(path="/" + name))


def wait_healthy(port: int, proc_alive: Callable[[], bool], timeout: float = 90) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not proc_alive():
            raise RuntimeError("engine exited before becoming healthy")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=2) as r:
                if r.status == 200:
                    return
        except OSError:
            pass
        time.sleep(0.5)
    raise RuntimeError(f"engine on :{port} not healthy after {timeout}s")


@dataclass
class Engine:
    name: str
    kind: str  # "hasura" | "rust"
    rust_bin: str | None = None
    proc: subprocess.Popen | None = None
    container: str | None = None
    log_path: Path | None = None
    cli: str | None = None  # the CLI to drive this engine with (default: --hasura-cli)

    def start(self, db_url: str, port: int, extra_env: dict[str, str], mounts: dict[str, str],
              cli_migrations: bool = False) -> None:
        env = {
            "HASURA_GRAPHQL_DATABASE_URL": db_url,
            "PG_DATABASE_URL": db_url,
            "HASURA_GRAPHQL_SERVER_PORT": str(port),
            "HASURA_GRAPHQL_ADMIN_SECRET": ADMIN_SECRET,
            "HASURA_GRAPHQL_ENABLE_TELEMETRY": "false",
            "HASURA_GRAPHQL_ENABLE_CONSOLE": "false",
            "ACTION_BASE_URL": "http://localhost:3000",
            "EVENT_WEBHOOK_URL": "http://localhost:3000/events",
            **extra_env,
        }
        self.log_path = Path(tempfile.mkstemp(prefix=f"cli-compat-{self.name}-", suffix=".log")[1])
        log = open(self.log_path, "wb")
        if self.kind == "hasura":
            self.container = f"cli-compat-{self.name}-{port}"
            cmd = ["docker", "run", "--rm", "--name", self.container, "--network", "host"]
            for k, v in env.items():
                cmd += ["-e", f"{k}={v}"]
            for host, inside in mounts.items():
                cmd += ["-v", f"{host}:{inside}:ro"]
            if cli_migrations:
                # The entrypoint's temporary server listens on 9691 by default; avoid clashes.
                cmd += ["-e", f"HASURA_GRAPHQL_MIGRATIONS_SERVER_PORT={free_port()}"]
            cmd.append(V2_CLI_MIGRATIONS_IMAGE if cli_migrations else V2_IMAGE)
            cmd += ["graphql-engine", "serve"]
            self.proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
        else:
            full_env = {**os.environ, **env, "RUST_LOG": os.environ.get("RUST_LOG", "warn")}
            for host, inside in mounts.items():
                # Our binary reads the directories in place; translate the container paths.
                for k, v in list(full_env.items()):
                    if v == inside:
                        full_env[k] = host
            self.proc = subprocess.Popen(
                [self.rust_bin, "serve"], env=full_env, stdout=log, stderr=subprocess.STDOUT
            )
        wait_healthy(port, lambda: self.proc.poll() is None, timeout=180 if cli_migrations else 90)

    def stop(self) -> None:
        if self.container:
            subprocess.run(["docker", "rm", "-f", self.container], capture_output=True)
            self.container = None
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.proc = None


# --------------------------------------------------------------------------------------------
# Normalisation

TS_RE = re.compile(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


class Normaliser:
    """Replaces values that legitimately differ between runs (timestamps, uuids, versions the
    CLI generates from the clock) with stable placeholders."""

    def __init__(self, known_versions: set[str], endpoint: str):
        self.known = known_versions
        self.endpoint = endpoint
        self.generated: dict[str, str] = {}

    def text(self, s: str) -> str:
        s = ANSI_RE.sub("", s)
        s = s.replace(self.endpoint, "<endpoint>")
        s = TS_RE.sub("<time>", s)
        s = UUID_RE.sub("<uuid>", s)

        def version(m: re.Match) -> str:
            v = m.group(0)
            if v in self.known:
                return v
            return self.generated.setdefault(v, f"<v{len(self.generated) + 1}>")

        return re.sub(r"(?<!\d)1[6-9]\d{11}(?!\d)", version, s)

    def value(self, v: object) -> object:
        if isinstance(v, str):
            return self.text(v)
        if isinstance(v, int) and not isinstance(v, bool) and len(str(v)) == 13:
            return self.text(str(v))
        if isinstance(v, list):
            items = [self.value(x) for x in v]
            # Inconsistent objects come out in V2's HashMap order; compare them as a set.
            if items and all(isinstance(x, dict) and {"type", "reason"} <= x.keys()
                             for x in items):
                items.sort(key=lambda x: json.dumps(x, sort_keys=True))
            return items
        if isinstance(v, dict):
            return {self.text(k): self.value(x) for k, x in v.items()}
        return v

    def output(self, out: str) -> list[object]:
        # `metadata ic list` prints the inconsistent objects (V2's HashMap order) as a
        # table: sort its rows.
        rows = [l for l in out.splitlines() if "│" in l and not l.lstrip().startswith("NAME")]
        if rows:
            order = iter(sorted(rows))
            out = "\n".join(next(order) if l in rows else l for l in out.splitlines())
        lines = []
        for line in out.splitlines():
            line = line.rstrip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except ValueError:
                obj = None
            if isinstance(obj, dict):
                obj.pop("time", None)
                lines.append(self.value(obj))
            else:
                lines.append(self.text(re.sub(r'time="[^"]*" ', "", line)))
        return lines


# --------------------------------------------------------------------------------------------
# Steps


@dataclass
class Ctx:
    cli: str
    project: Path
    endpoint: str
    pg_url: str
    db_url: str
    created: list[str] = field(default_factory=list)  # migration versions the CLI created
    # --rust-cli mode: record the server state after each step instead of the HTTP exchanges.
    state_port: int | None = None

    def env(self) -> dict[str, str]:
        env = {
            k: v for k, v in os.environ.items() if k not in ("NODE_OPTIONS", "NODE_PATH")
        }  # NODE_OPTIONS breaks the CLI's bundled cli-ext (exit status 4)
        env.update(
            HASURA_GRAPHQL_ENDPOINT=self.endpoint,
            HASURA_GRAPHQL_ADMIN_SECRET=ADMIN_SECRET,
            HASURA_GRAPHQL_ENABLE_TELEMETRY="false",
            HOME=str(self.project.parent / "home"),
        )
        return env


@dataclass
class Step:
    id: str
    args: list[str] | Callable[[Ctx], list[str]] | None = None
    snapshot: list[str] = field(default_factory=list)  # project paths to capture afterwards
    before: Callable[[Ctx], None] | None = None
    after: Callable[[Ctx], None] | None = None
    sql: str | None = None  # instead of the CLI: run this SQL and record its rows


def record_created(ctx: Ctx) -> None:
    root = ctx.project / "migrations" / "default"
    known = set(ctx.created) | {"1700000000000", "1700000000001"}
    for d in sorted(root.iterdir()):
        v = d.name.split("_", 1)[0]
        if v not in known:
            ctx.created.append(v)
            known.add(v)


def last_created(ctx: Ctx) -> str:
    return ctx.created[-1]


DB = ["--database-name", "default"]

SCENARIO_CLI: list[Step] = [
    Step("version", ["version"]),
    Step("metadata-apply-before-migrations", ["metadata", "apply"]),
    Step("metadata-ic-list-before-migrations", ["metadata", "inconsistency", "list"]),
    Step("migrate-status-empty", ["migrate", "status", *DB]),
    Step("migrate-apply-all-databases", ["migrate", "apply", "--all-databases"]),
    Step("metadata-reload", ["metadata", "reload"]),
    Step("metadata-ic-status", ["metadata", "inconsistency", "status"]),
    Step("migrate-status-applied", ["migrate", "status", *DB]),
    Step("migrate-apply-nothing-pending", ["migrate", "apply", *DB]),
    Step("metadata-export", ["metadata", "export"], snapshot=["metadata"]),
    Step("metadata-diff", ["metadata", "diff"]),
    Step("metadata-apply-again", ["metadata", "apply"]),
    Step(
        "migrate-create",
        [
            "migrate", "create", "add_genre_slug", *DB,
            "--up-sql", "ALTER TABLE public.genre ADD COLUMN slug text;",
            "--down-sql", "ALTER TABLE public.genre DROP COLUMN slug;",
        ],
        after=record_created,
    ),
    Step("migrate-status-pending", ["migrate", "status", *DB]),
    Step("migrate-apply-one", ["migrate", "apply", *DB]),
    Step("check-column-added", sql="SELECT column_name FROM information_schema.columns "
         "WHERE table_name = 'genre' ORDER BY ordinal_position"),
    Step("migrate-apply-down-one", ["migrate", "apply", "--down", "1", *DB]),
    Step("check-column-removed", sql="SELECT column_name FROM information_schema.columns "
         "WHERE table_name = 'genre' ORDER BY ordinal_position"),
    Step("migrate-apply-skip-execution",
         lambda c: ["migrate", "apply", "--version", last_created(c), "--skip-execution", *DB]),
    Step("migrate-status-skipped", ["migrate", "status", *DB]),
    Step("migrate-apply-version-down",
         lambda c: ["migrate", "apply", "--version", last_created(c), "--type", "down",
                    "--skip-execution", *DB]),
    Step("migrate-apply-goto", lambda c: ["migrate", "apply", "--goto", last_created(c), *DB]),
    Step("migrate-status-goto", ["migrate", "status", *DB]),
    Step("migrate-delete-server", lambda c: ["migrate", "delete", "--version", last_created(c),
                                             "--server", "--force", *DB]),
    Step("migrate-status-deleted", ["migrate", "status", *DB]),
    Step("migrate-apply-again", ["migrate", "apply", *DB]),
    Step(
        "migrate-squash",
        ["migrate", "squash", "--from", "1700000000000", "--name", "squashed",
         "--delete-source", *DB],
        after=record_created,
    ),
    Step("migrate-status-squashed", ["migrate", "status", *DB]),
    # What the docs say to do after a squash: mark the squashed version as applied.
    Step("migrate-apply-squashed-skip-execution",
         lambda c: ["migrate", "apply", "--version", last_created(c), "--skip-execution", *DB]),
    Step("migrate-status-squashed-applied", ["migrate", "status", *DB],
         snapshot=["migrations"]),
    Step("migrate-create-from-server",
         ["migrate", "create", "from_server", "--from-server", "--schema", "public", *DB],
         after=record_created, snapshot=["migrations"]),
    Step("migrate-status-from-server", ["migrate", "status", *DB]),
    # pg_dump 17.6+ writes `\restrict` lines that run_sql rejects, so V2 itself cannot
    # `seed apply` a seed it created (the dump is compared, then moved aside).
    Step("seed-create-from-table",
         ["seed", "create", "genres", "--from-table", "genre", *DB], snapshot=["seeds"],
         after=lambda c: [p.rename(c.project / p.name) for p in
                          (c.project / "seeds" / "default").glob("*_genres.sql")]),
    Step("seed-apply", ["seed", "apply", *DB],
         before=lambda c: (c.project / "seeds" / "default" / "1_genres.sql").write_text(
             "INSERT INTO public.genre (genre_id, name) VALUES (900, 'Seed one');\n")),
    Step(
        "seed-apply-file",
        ["seed", "apply", "--file", "extra_genres.sql", *DB],
        before=lambda c: (c.project / "seeds" / "default" / "extra_genres.sql").write_text(
            "INSERT INTO public.genre (genre_id, name) VALUES (901, 'Seeded');\n"
        ),
    ),
    Step("check-seeded", sql="SELECT genre_id::text, name FROM public.genre "
         "WHERE genre_id > 100 ORDER BY genre_id"),
    Step("metadata-clear", ["metadata", "clear"]),
    Step("metadata-export-cleared", ["metadata", "export", "--output", "json"]),
    Step("metadata-diff-after-clear", ["metadata", "diff"]),
    Step("metadata-apply-disallow-inconsistent",
         ["metadata", "apply", "--disallow-inconsistent-metadata"]),
    Step("metadata-ic-drop", ["metadata", "inconsistency", "drop"]),
    Step("metadata-ic-list-final", ["metadata", "inconsistency", "list"]),
    Step("deploy", ["deploy"]),
    Step("metadata-export-final", ["metadata", "export"], snapshot=["metadata"]),
]


def run_step(ctx: Ctx, step: Step, recorder: Recorder, norm: Normaliser) -> dict:
    recorder.take()
    if step.before:
        step.before(ctx)
    out: dict = {}
    if step.sql is not None:
        r = subprocess.run(
            ["psql", ctx.db_url, "-AtX", "-c", step.sql], capture_output=True, text=True
        )
        out["rows"] = r.stdout.splitlines()
        out["error"] = r.stderr.strip() or None
    else:
        args = step.args(ctx) if callable(step.args) else step.args
        cmd = [ctx.cli, *args, "--skip-update-check", "--no-color"]
        try:
            r = subprocess.run(cmd, cwd=ctx.project, env=ctx.env(), capture_output=True,
                               text=True, timeout=300, stdin=subprocess.DEVNULL)
            out["exit"] = r.returncode
            out["output"] = norm.output(r.stdout + r.stderr)
        except subprocess.TimeoutExpired:
            out["exit"] = "timeout"
            out["output"] = []
    if step.after:
        step.after(ctx)
    if ctx.state_port:
        recorder.take()
        out["state"] = server_state(ctx.state_port, norm)
    else:
        out["http"] = [
            {
                "request": f"{e.method} {e.path}",
                "body": norm.value(e.request),
                "status": e.status,
                "response": norm.value(e.response),
            }
            for e in recorder.take()
        ]
    for rel in step.snapshot:
        out.setdefault("files", {}).update(snapshot(ctx.project, rel, norm))
    return out


TELEMETRY_NOTICE = "Help us improve Hasura! The cli collects anonymized usage stats"
PG_DUMP_NOISE = re.compile(r"^(\\(un)?restrict |SET |SELECT pg_catalog\.set_config|--)")


def sql_text(s: str) -> str:
    """pg_dump output without what depends on the pg_dump version (header, SET lines)."""
    return "\n".join(l for l in s.splitlines() if l.strip() and not PG_DUMP_NOISE.match(l))


def server_state(port: int, norm: Normaliser) -> dict:
    """What a CLI step left on the server: metadata, inconsistencies and the CLI's state."""

    def call(body: dict) -> object:
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/v1/metadata", json.dumps(body).encode(),
            {"content-type": "application/json", "x-hasura-admin-secret": ADMIN_SECRET})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            return {"status": e.code, "body": json.loads(e.read() or b"null")}

    metadata = call({"type": "export_metadata", "version": 2, "args": {}})
    if isinstance(metadata, dict):
        metadata.pop("resource_version", None)
    return {
        "metadata": norm.value(metadata),
        "inconsistent": norm.value(call({"type": "get_inconsistent_metadata", "args": {}})),
        "catalog_state": norm.value(call({"type": "get_catalog_state", "args": {}})),
    }


def run_step_safely(ctx: Ctx, step: Step, recorder: Recorder, norm: Normaliser) -> dict:
    """A step whose own helpers fail (e.g. an earlier step created nothing) fails alone."""
    try:
        return run_step(ctx, step, recorder, norm)
    except Exception as e:
        recorder.take()
        return {"harness_error": f"{type(e).__name__}: {e}"}


def snapshot(project: Path, rel: str, norm: Normaliser) -> dict[str, str]:
    root = project / rel
    files = {}
    if root.exists():
        for p in sorted(root.rglob("*")):
            if p.is_file():
                text = p.read_text()
                if p.suffix == ".sql":
                    text = sql_text(text)
                files[norm.text(str(p.relative_to(project)))] = norm.text(text)
    return files


def edit(path: str, old: str, new: str) -> Callable[[Ctx], None]:
    def run(ctx: Ctx) -> None:
        f = ctx.project / path
        text = f.read_text()
        assert old in text, f"{old!r} not in {path}"
        f.write_text(text.replace(old, new, 1))
    return run


def write(path: str, text: str) -> Callable[[Ctx], None]:
    def run(ctx: Ctx) -> None:
        target = ctx.project / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
    return run


# --- Extra CLI steps for --rust-cli mode: our CLI against the official one on our engine
# (flags, errors, init, diffs of local edits). Not run against V2.

SCENARIO_RUST_CLI: list[Step] = [
    Step("metadata-ic-list-ls-alias", ["md", "ic", "ls"]),
    Step("migrate-status-no-database", ["migrate", "status"]),
    Step("migrate-status-unknown-database", ["migrate", "status", "--database-name", "nope"]),
    Step("migrate-status-unknown-flag", ["migrate", "status", "--bogus", *DB]),
    Step("migrate-squash-no-from", ["migrate", "squash", *DB]),
    Step("migrate-create-no-name", ["migrate", "create", *DB]),
    Step("migrate-delete-no-flags", ["migrate", "delete", *DB]),
    Step("migrate-apply-dry-run", ["migrate", "apply", "--dry-run", *DB]),
    Step("migrate-apply-all", ["migrate", "apply", "--all-databases"]),
    Step("metadata-apply", ["metadata", "apply"]),
    Step("metadata-apply-dry-run", ["metadata", "apply", "--dry-run"]),
    Step("metadata-apply-dry-run-yaml", ["metadata", "apply", "--dry-run", "-o", "yaml"]),
    Step("metadata-export-json", ["metadata", "export", "-o", "json"]),
    Step("metadata-export-yaml", ["metadata", "export", "--output", "yaml"]),
    Step("metadata-ic-list-json", ["metadata", "inconsistencies", "list", "-o", "json"]),
    Step("metadata-diff-unchanged", ["metadata", "diff"]),
    Step(
        "metadata-diff-local-edits",
        ["metadata", "diff"],
        before=lambda c: [
            edit("metadata/databases/default/tables/public_album.yaml", "limit: 500", "limit: 50")(c),
            edit("metadata/databases/default/tables/public_genre.yaml", "table:",
                 "configuration:\n  custom_name: Genre\ntable:")(c),
            edit("metadata/actions.graphql", "currency: String!", "currency: String!\n  note: String")(c),
            edit("metadata/query_collections.yaml", "album_id\n", "album_id\n                  released_on\n")(c),
        ],
    ),
    Step("metadata-diff-type-json", ["metadata", "diff", "--type", "json"]),
    Step("metadata-diff-type-yaml", ["metadata", "diff", "--type", "yaml"]),
    Step("metadata-diff-type-unified-json", ["metadata", "diff", "--type", "unified-json"]),
    Step("metadata-diff-type-unknown", ["metadata", "diff", "--type", "nope"]),
    Step("init", ["init", "newproj"], snapshot=["newproj"]),
    Step("init-existing-directory", ["init", "newproj"]),
    Step("metadata-diff-two-directories", ["metadata", "diff", "metadata", "newproj/metadata"]),
    Step("metadata-diff-not-a-directory", ["metadata", "diff", "config.yaml"]),
    Step("init-from-server",
         lambda c: ["init", "fromserver", "--endpoint", c.endpoint, "--admin-secret", ADMIN_SECRET],
         snapshot=["fromserver/metadata", "fromserver/config.yaml"]),
    Step("seed-apply-all-databases", ["seed", "apply", "--all-databases"],
         before=write("seeds/default/1_genres.sql",
                      "INSERT INTO public.genre (genre_id, name) VALUES (950, 'All dbs');\n")),
    Step("migrate-delete-all", ["migrate", "delete", "--all", "--force", *DB],
         snapshot=["migrations"]),
    Step("migrate-status-after-delete-all", ["migrate", "status", *DB]),
    Step("metadata-reset-alias", ["metadata", "reset"]),
]


def run_cli_scenario(engine: Engine, args, work: Path, steps: list[Step] | None = None,
                     prefix: str = "cli") -> dict[str, dict]:
    project = work / (engine.name if prefix == "cli" else f"{engine.name}-{prefix}") / "project"
    shutil.copytree(PROJECT, project)
    # The CLI reads endpoint and secret from the environment (no edits to the copy needed).
    (project.parent / "home").mkdir()
    db_url = fresh_db(args.pg, f"cli_compat_{engine.name}" if prefix == "cli"
                      else f"cli_compat_{prefix.replace('-', '_')}_{engine.name}")
    port = free_port()
    engine.start(db_url, port, {}, {})
    recorder = Recorder(port)
    proxy, proxy_port = start_proxy(recorder)
    ctx = Ctx(engine.cli or args.hasura_cli, project, f"http://127.0.0.1:{proxy_port}", args.pg,
              db_url, state_port=port if args.rust_cli else None)
    norm = Normaliser({"1700000000000", "1700000000001"}, ctx.endpoint)
    results = {}
    try:
        for step in SCENARIO_CLI if steps is None else steps:
            if args.filter and args.filter not in step.id:
                continue
            results[f"{prefix}/{step.id}"] = run_step_safely(ctx, step, recorder, norm)
    finally:
        proxy.shutdown()
        engine.stop()
    return results


# --- Boot scenario: the cli-migrations-v3 image flow (metadata + migrations dirs on boot).

SCENARIO_BOOT: list[Step] = [
    Step("migrate-status", ["migrate", "status", *DB]),
    Step("metadata-ic-list", ["metadata", "inconsistency", "list"]),
    Step("metadata-export", ["metadata", "export"], snapshot=["metadata"]),
    Step("metadata-diff", ["metadata", "diff"]),
    Step("migrate-apply-nothing-pending", ["migrate", "apply", "--all-databases"]),
    Step("check-data", sql="SELECT count(*)::text FROM public.track"),
]


def run_boot_scenario(engine: Engine, args, work: Path) -> dict[str, dict]:
    project = work / f"{engine.name}-boot" / "project"
    shutil.copytree(PROJECT, project)
    (project.parent / "home").mkdir()
    db_url = fresh_db(args.pg, f"cli_compat_boot_{engine.name}")
    port = free_port()
    mounts = {str(project / "metadata"): "/hasura-metadata",
              str(project / "migrations"): "/hasura-migrations"}
    env = {"HASURA_GRAPHQL_METADATA_DIR": "/hasura-metadata",
           "HASURA_GRAPHQL_MIGRATIONS_DIR": "/hasura-migrations"}
    engine.start(db_url, port, env, mounts, cli_migrations=True)
    recorder = Recorder(port)
    proxy, proxy_port = start_proxy(recorder)
    ctx = Ctx(engine.cli or args.hasura_cli, project, f"http://127.0.0.1:{proxy_port}", args.pg,
              db_url, state_port=port if args.rust_cli else None)
    norm = Normaliser({"1700000000000", "1700000000001"}, ctx.endpoint)
    results = {}
    try:
        for step in SCENARIO_BOOT:
            if args.filter and args.filter not in step.id:
                continue
            results[f"boot/{step.id}"] = run_step_safely(ctx, step, recorder, norm)
    finally:
        proxy.shutdown()
        engine.stop()
    return results


# --------------------------------------------------------------------------------------------
# Comparison and report


def pretty(v: object) -> list[str]:
    return json.dumps(v, indent=2, ensure_ascii=False).splitlines()


def compare(a: dict, b: dict) -> list[str]:
    if a == b:
        return []
    return list(difflib.unified_diff(pretty(a), pretty(b), "hasura", "rust", lineterm="", n=2))


def load_expected(name: str) -> list[str]:
    path = ROOT / name
    if not path.exists():
        return []
    return [l.split("#", 1)[0].strip() for l in path.read_text().splitlines()
            if l.split("#", 1)[0].strip()]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--hasura-cli", required=True, help="the official hasura CLI binary")
    ap.add_argument("--rust-bin", default=str(REPO / "target" / "debug" / "graphql-engine"))
    ap.add_argument("--pg", default=os.environ.get(
        "CLI_COMPAT_PG_URL", "postgres://postgres:postgres@localhost:5432/postgres"),
        help="Postgres superuser URL; reachable from containers with --network host")
    ap.add_argument("--filter", help="only steps whose id contains this")
    ap.add_argument("--scenario", choices=["cli", "boot", "console", "all"], default="all")
    ap.add_argument("--chromium", help="Chromium for the console scenario (default: Playwright's)")
    ap.add_argument("--screenshots", action="store_true", help="save console page screenshots")
    ap.add_argument("--rust-cli", help="compare our CLI (this binary) with the official one, "
                    "both against our engine; uses expected-failures-rust-cli.txt")
    ap.add_argument("--hasura-results", help="reuse a hasura results JSON instead of running V2")
    ap.add_argument("--out", default="cli-compat-results", help="directory for results/report")
    ap.add_argument("--write-expected-failures", action="store_true")
    args = ap.parse_args()
    args.hasura_cli = str(Path(args.hasura_cli).resolve())

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="cli-compat-"))
    rust_bin = str(Path(args.rust_bin).resolve())
    if args.rust_cli:
        # Our CLI against the official one, both driving our engine. "hasura" is the official
        # CLI's run here.
        args.rust_cli = str(Path(args.rust_cli).resolve())
        if args.scenario in ("console", "all"):
            args.scenario = "cli+boot"
        engines = [Engine("rust", "rust", rust_bin=rust_bin, cli=args.rust_cli)]
        if not args.hasura_results:
            engines.insert(0, Engine("hasura", "rust", rust_bin=rust_bin, cli=args.hasura_cli))
    else:
        engines = [Engine("rust", "rust", rust_bin=rust_bin)]
        if not args.hasura_results:
            engines.insert(0, Engine("hasura", "hasura"))
    results: dict[str, dict[str, dict]] = {}
    for engine in engines:
        r: dict[str, dict] = {}
        try:
            if args.scenario in ("cli", "cli+boot", "all"):
                r.update(run_cli_scenario(engine, args, work))
            if args.scenario in ("boot", "cli+boot", "all"):
                r.update(run_boot_scenario(engine, args, work))
            if args.rust_cli and args.scenario in ("cli", "cli+boot"):
                r.update(run_cli_scenario(engine, args, work, SCENARIO_RUST_CLI, "rust-cli"))
            if args.scenario in ("console", "all"):
                import console  # needs playwright

                r.update(console.run_console_scenario(engine, args, work))
        except Exception as e:  # an engine that fails to boot fails every step
            print(f"{engine.name}: {e}; log: {engine.log_path}", file=sys.stderr)
            if engine.log_path:
                print(engine.log_path.read_text()[-3000:], file=sys.stderr)
            r["engine-start"] = {"error": str(e)}
            engine.stop()
        results[engine.name] = r
        (out / f"{engine.name}.json").write_text(json.dumps(r, indent=2, ensure_ascii=False))
    if args.hasura_results:
        results["hasura"] = json.loads(Path(args.hasura_results).read_text())

    # Each engine's project copy lives in its own directory.
    project_re = re.compile(r"/[^\s\"']*/(hasura|rust)(-boot|-rust-cli)?/project")
    results = {k: json.loads(project_re.sub("<project>", json.dumps(v))) for k, v in results.items()}
    if args.rust_cli:
        # The official CLI announces its usage stats on first run; ours collects none.
        for steps in results.values():
            for step in steps.values():
                if isinstance(step.get("output"), list):
                    step["output"] = [l for l in step["output"] if not (
                        isinstance(l, dict) and TELEMETRY_NOTICE in str(l.get("msg", "")))]
    step_ids = list(dict.fromkeys([*results["hasura"], *results["rust"]]))
    expected_file = "expected-failures-rust-cli.txt" if args.rust_cli else "expected-failures.txt"
    expected = set(load_expected(expected_file))
    reference = "the official hasura CLI" if args.rust_cli else "Hasura v2.51.0-ce"
    failing, report = [], ["# Hasura CLI compatibility", ""]
    rows = []
    for sid in step_ids:
        diff = compare(results["hasura"].get(sid, {}), results["rust"].get(sid, {}))
        # A scenario that could not start fails even when it failed the same way on both.
        # So does a step the harness could not drive (e.g. a console page that never loaded).
        ok = (not diff and sid != "engine-start"
              and not any("harness_error" in results[e].get(sid, {}) for e in ("hasura", "rust")))
        if not ok:
            failing.append(sid)
        rows.append((sid, ok))
        if diff:
            report += [f"## {sid}", "", "```diff", *diff[:200], "```", ""]
            print(f"--- {sid}", *diff[:60], sep="\n")  # visible in CI logs too
    passed = sum(ok for _, ok in rows)
    summary = [f"{passed} of {len(rows)} steps match {reference}.", "",
               "| step | result |", "|---|---|"]
    summary += [f"| {sid} | {'✅' if ok else '❌'} |" for sid, ok in rows]
    report[2:2] = summary + [""]
    (out / "report.md").write_text("\n".join(report) + "\n")
    print("\n".join(summary))

    if args.write_expected_failures:
        (ROOT / expected_file).write_text(
            "# Steps whose outcome still differs from Hasura (see README). Only ever shrinks.\n"
            + "".join(f"{s}\n" for s in failing))
        return 0
    regressions = [s for s in failing if s not in expected]
    fixed = [s for s in expected if s in step_ids and s not in failing]
    for s in regressions:
        print(f"REGRESSION: {s} differs from {reference} (not in {expected_file})")
    for s in fixed:
        print(f"NOW PASSING: {s}; delete it from cli-compat/{expected_file}")
    print(f"report: {out / 'report.md'}")
    return 1 if regressions or fixed else 0


if __name__ == "__main__":
    sys.exit(main())
