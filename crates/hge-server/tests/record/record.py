"""Record a scenario from a running Hasura V2 for the replay tests in `tests/`.

usage: record.py <steps.py> <base-url> <out.json>

<steps.py> defines STEPS, a list of (url, request body), and DESCRIPTION. Each request is
sent in order and its status and response are written to <out.json>. Start from a fresh
database: for events_api.json,

    docker run -d --name hge2 --network host \\
      -e HASURA_GRAPHQL_DATABASE_URL=postgres://postgres:postgres@localhost:5432/ev_record \\
      -e HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL=86400000 \\
      -e HASURA_GRAPHQL_SERVER_PORT=8099 hasura/graphql-engine:v2.51.0-ce
    python3 record.py events_api_steps.py http://localhost:8099 ../events_api.json

(the long fetch interval keeps V2 from delivering the events the scenario inspects). Steps
marked `todo` in the old file must be marked again by hand.
"""

import json
import runpy
import sys
import urllib.error
import urllib.request

steps_file, base, out = sys.argv[1:4]
module = runpy.run_path(steps_file)
recorded = []
for url, query in module["STEPS"]:
    req = urllib.request.Request(
        base + url,
        data=json.dumps(query).encode(),
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req) as r:
            status, body = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, body = e.code, e.read()
    try:
        response = json.loads(body)
    except ValueError:
        response = body.decode()
    recorded.append({"url": url, "query": query, "status": status, "response": response})

with open(out, "w") as f:
    json.dump({"description": module["DESCRIPTION"], "steps": recorded}, f, indent=1)
    f.write("\n")
for i, s in enumerate(recorded):
    print(i, s["status"], json.dumps(s["response"])[:200])
