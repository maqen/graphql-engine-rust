"""The `console/…` scenario of run.py: browse the server-hosted console in Chromium.

Each engine serves the console (`HASURA_GRAPHQL_ENABLE_CONSOLE`) with V2's own console build as
`HASURA_GRAPHQL_CONSOLE_ASSETS_DIR` (copied out of the V2 image), behind run.py's recording
proxy, with the bench project deployed. A headless Chromium logs in with the admin secret and
opens each page in PAGES. A step compares every API request the page made with the engine's
response, and the page's uncaught JavaScript errors. Requests to anything but the engine (fonts,
release checks) are blocked so both runs see the same thing.

Needs the `playwright` Python package and a Chromium (`--chromium`, or Playwright's own).
"""

from __future__ import annotations

import json
import shutil
import subprocess
from pathlib import Path

import run

ASSETS_IN_IMAGE = "/srv/console-assets"
LOGIN = "input[placeholder='Enter admin-secret']"

# (step id, console path). Paths under /console; tables are from bench/project.
PAGES: list[tuple[str, str]] = [
    ("api-explorer", ""),
    ("data-schema", "data/default/schema/public"),
    ("table-browse", "data/default/schema/public/tables/artist/browse"),
    ("table-modify", "data/default/schema/public/tables/album/modify"),
    ("table-relationships", "data/default/schema/public/tables/album/relationships"),
    ("table-permissions", "data/default/schema/public/tables/track/permissions"),
    ("view-browse", "data/default/schema/public/views/artist_stats/browse"),
    ("sql", "data/sql"),
    ("actions", "actions/manage/actions"),
    ("remote-schemas", "remote-schemas/manage/schemas"),
    ("event-triggers", "events/data/manage"),
    ("cron-triggers", "events/cron/manage"),
    ("rest-endpoints", "api/rest/list"),
    ("allow-list", "api/allow-list"),
    ("metadata-status", "settings/metadata-status"),
    ("metadata-actions", "settings/metadata-actions"),
]


def console_assets(work: Path) -> Path:
    """V2's console build, copied out of the image once per run."""
    dest = work / "console-assets"
    if not dest.exists():
        cid = subprocess.run(["docker", "create", run.V2_IMAGE], check=True,
                             capture_output=True, text=True).stdout.strip()
        try:
            subprocess.run(["docker", "cp", f"{cid}:{ASSETS_IN_IMAGE}", str(dest)], check=True,
                           capture_output=True)
        finally:
            subprocess.run(["docker", "rm", cid], capture_output=True)
    return dest


def deploy(ctx: run.Ctx) -> None:
    r = subprocess.run([ctx.cli, "deploy", "--skip-update-check", "--no-color"], cwd=ctx.project,
                       env=ctx.env(), capture_output=True, text=True, timeout=300,
                       stdin=subprocess.DEVNULL)
    if r.returncode != 0:
        raise RuntimeError(f"hasura deploy failed: {r.stdout}{r.stderr}")


def canonical_cell(cell: object) -> object:
    """A `run_sql` cell without the order Postgres happened to aggregate in: the console's
    catalog queries build JSON with `json_agg` and lists with `string_agg`, unordered."""
    if not isinstance(cell, str):
        return cell
    try:
        value = json.loads(cell)
    except ValueError:
        return ",".join(sorted(cell.split(","))) if "," in cell else cell

    def canon(v: object) -> object:
        if isinstance(v, dict):  # object ids differ between any two databases
            return {k: "<oid>" if k.endswith("oid") and isinstance(x, int) else canon(x)
                    for k, x in sorted(v.items())}
        if isinstance(v, list):
            return sorted((canon(x) for x in v), key=lambda x: json.dumps(x, sort_keys=True))
        return v

    return canon(value) if isinstance(value, (dict, list)) else cell


def canonical_sql_results(response: object) -> object:
    """`run_sql` answers (alone or in a bulk) with rows in a stable order."""
    if isinstance(response, list):
        return [canonical_sql_results(r) for r in response]
    if isinstance(response, dict) and response.get("result_type") == "TuplesOk":
        header, *rows = response.get("result") or [[]]
        rows = [[canonical_cell(c) for c in row] for row in rows]
        rows.sort(key=lambda row: json.dumps(row, sort_keys=True))
        return {**response, "result": [header, *rows]}
    return response


def canonical_response(body: object, response: object) -> object:
    response = canonical_sql_results(response)
    # V2 lists suggested relationships in hash map order.
    if (isinstance(body, dict) and body.get("type") == "pg_suggest_relationships"
            and isinstance(response, dict) and isinstance(response.get("relationships"), list)):
        rels = sorted(response["relationships"], key=lambda r: json.dumps(r, sort_keys=True))
        response = {**response, "relationships": rels}
    return response


def exchanges(recorder: run.Recorder, norm: run.Normaliser) -> list[dict]:
    """The page's requests and the engine's answers. The console fires some requests more
    than once depending on timing, so identical exchanges count once. Assets are compared
    on status and type only; the console page itself (its `window.__env`) in full."""
    seen: dict[str, dict] = {}
    for e in recorder.take():
        if e.path.startswith("/console/assets/"):
            item = {"request": f"{e.method} {e.path}", "status": e.status,
                    "type": e.response_type}
            if e.status != 200:
                item["response"] = norm.value(e.response)
            seen.setdefault(json.dumps(item, sort_keys=True), item)
            continue
        item = {
            "request": f"{e.method} {e.path}",
            "body": norm.value(e.request),
            "status": e.status,
            "response": canonical_response(e.request, norm.value(e.response)),
        }
        if e.path == "/" or e.path.startswith("/console"):
            item["type"] = e.response_type
        seen.setdefault(json.dumps(item, sort_keys=True), item)
    return [seen[k] for k in sorted(seen)]


def run_console_scenario(engine: run.Engine, args, work: Path) -> dict[str, dict]:
    from playwright.sync_api import sync_playwright  # only this scenario needs it

    assets = console_assets(work)
    project = work / f"{engine.name}-console" / "project"
    shutil.copytree(run.PROJECT, project)
    (project.parent / "home").mkdir()
    db_name = f"cli_compat_console_{engine.name}"
    db_url = run.fresh_db(args.pg, db_name)
    port = run.free_port()
    env = {"HASURA_GRAPHQL_ENABLE_CONSOLE": "true",
           "HASURA_GRAPHQL_CONSOLE_ASSETS_DIR": ASSETS_IN_IMAGE}
    engine.start(db_url, port, env, {str(assets): ASSETS_IN_IMAGE})
    recorder = run.Recorder(port)
    proxy, proxy_port = run.start_proxy(recorder)
    base = f"http://127.0.0.1:{proxy_port}"
    ctx = run.Ctx(args.hasura_cli, project, base, args.pg, db_url)
    norm = run.Normaliser({"1700000000000", "1700000000001"}, base)
    results: dict[str, dict] = {}
    try:
        deploy(ctx)
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=args.chromium or None)
            context = browser.new_context(viewport={"width": 1400, "height": 900})
            context.route("**/*", lambda route: route.continue_()
                          if route.request.url.startswith(base) else route.abort())
            page = context.new_page()
            errors: list[str] = []
            page.on("pageerror", lambda e: errors.append(norm.text(str(e))))
            page.goto(f"{base}/console", wait_until="networkidle")
            page.fill(LOGIN, run.ADMIN_SECRET)
            remember = page.get_by_role("checkbox")  # keep the secret across page loads
            if remember.first.get_attribute("aria-checked") != "true":
                page.click("text=Remember on the browser")
            page.press(LOGIN, "Enter")
            try:
                page.locator(LOGIN).wait_for(state="detached", timeout=30000)
            except Exception as e:
                raise RuntimeError("the console did not accept the admin secret") from e
            page.wait_for_load_state("networkidle")
            recorder.take()
            for step_id, path in PAGES:
                if args.filter and args.filter not in step_id:
                    continue
                errors.clear()
                out: dict = {}
                try:
                    page.goto(f"{base}/console/{path}".rstrip("/"), wait_until="networkidle")
                    page.wait_for_timeout(1500)
                    page.wait_for_load_state("networkidle")
                    out["url"] = norm.text(page.url)
                except Exception as e:  # a page that never settles fails alone
                    out["harness_error"] = f"{type(e).__name__}: {e}"
                if args.screenshots:
                    shots = Path(args.out) / "screenshots"
                    shots.mkdir(parents=True, exist_ok=True)
                    page.screenshot(path=str(shots / f"{engine.name}-{step_id}.png"))
                out["http"] = exchanges(recorder, norm)
                out["page_errors"] = sorted(set(errors))
                # A page on the login screen, or one that never asked the engine anything,
                # would otherwise match trivially.
                if "harness_error" not in out:
                    if page.locator(LOGIN).count():
                        out["harness_error"] = "logged out"
                    elif not any(x["request"] == "POST /v1/metadata" for x in out["http"]):
                        out["harness_error"] = "the page made no metadata request"
                results[f"console/{step_id}"] = out
            browser.close()
    finally:
        proxy.shutdown()
        engine.stop()
    # `/v1alpha1/config` reports the assets directory, which is a host path for our engine.
    # Catalog queries report the database name.
    text = json.dumps(results).replace(json.dumps(str(assets))[1:-1], ASSETS_IN_IMAGE)
    return json.loads(text.replace(db_name, "<database>"))
