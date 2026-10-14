#!/usr/bin/env python3
"""Write the hand-ported action cases (upstream test_actions.py) that the port tool skips.

TestActionsAsync runs an async action, reads the action id from the mutation's response and
queries the result until it is there (upstream's check_query_timeout). Here the mutation step
`capture`s the id, later steps use `{{var:action_id}}`, and the query step has `retry_for: 10`.
TestActionsSyncResponseHeaders checks a response header (`resp_headers`). The port tool
snapshots both classes' setups (they are in its HAND_PORTED list); this script writes their
cases. Responses are recorded from Hasura afterwards; upstream's expected responses are
written here, so a recording that differs shows up in the diff:

    python3 conformance/tools/port_tests_py.py ... --only 'test_actions.py::TestActions(Async|SyncResponseHeaders)$'
    python3 conformance/tools/hand/action_cases.py
    target/debug/hge-conformance --filter actions/TestActions hasura --record
"""

import os
import sys

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
from port_tests_py import dump_yaml  # noqa: E402

ROOT = os.path.join(HERE, "..", "..", "suites")
AREA = "actions"
FILE = "test_actions.py"
ID = "{{var:action_id}}"


def gql(query, variables=None, headers=None, response=None, **extra):
    step = {"url": "/v1/graphql", "status": 200}
    if headers:
        step["headers"] = headers
    step["query"] = {"query": query, "variables": variables or {}}
    step["response"] = response
    step.update(extra)
    return step


def run_async(mutation, field, headers=None):
    """The mutation that starts the action; its response is the action id, which later steps
    compare with `{{var:action_id}}`."""
    return gql(mutation, headers=headers, response={"data": {field: "{{any}}"}},
               capture={"action_id": f"/data/{field}"})


def result(query, response, headers=None):
    """upstream check_query_timeout(..., timeout=10): query until the result is there."""
    return gql(query, {"action_id": ID}, headers, response, retry_for=10)


def write_cases(cls, tests):
    d = os.path.join(ROOT, AREA, cls)
    if not os.path.isfile(os.path.join(d, "suite.yaml")):
        sys.exit(f"{d}: no suite; run port_tests_py.py --only 'test_actions.py::{cls}$' first")
    cases = os.path.join(d, "cases")
    os.makedirs(cases, exist_ok=True)
    for name in os.listdir(cases):
        with open(os.path.join(cases, name)) as f:
            if (yaml.safe_load(f).get("upstream") or {}).get("by_hand"):
                os.remove(os.path.join(cases, name))
    for method, steps in tests.items():
        case = {"upstream": {"file": FILE, "class": cls, "method": method, "by_hand": True}, "steps": steps}
        with open(os.path.join(cases, f"{method}.yaml"), "w") as f:
            dump_yaml(case, f)


def user_query(field):
    return f'''query ($action_id: uuid!){{
  {field}(id: $action_id){{
    __typename
    id
    output {{
      __typename
      id
      user {{
        __typename
        name
        email
        is_admin
      }}
    }}
  }}
}}
'''


def user_result(field, name, email):
    return {"data": {field: {"__typename": field, "id": ID, "output": {
        "__typename": "UserId", "id": 1,
        "user": {"__typename": "user", "name": name, "email": email, "is_admin": False}}}}}


def errors_query(field):
    return f'''query ($action_id: uuid!){{
  {field}(id: $action_id){{
    id
    errors
  }}
}}
'''


def async_actions():
    create = 'mutation {\n  create_user(email: "%s", name: "%s")\n}\n'
    err = "mutation {\n  test_async_action_error_response\n}\n"
    roles_query = "query ($action_id: uuid!){\n  create_user(id: $action_id){\n    id\n    output {\n      id\n    }\n  }\n}\n"
    user1 = {"X-Hasura-Role": "user", "X-Hasura-User-Id": "1"}
    user2 = {"X-Hasura-Role": "user", "X-Hasura-User-Id": "2"}
    nested_query = '''query ($action_id: uuid!){
  create_user_nested(id: $action_id){
    __typename
    id
    output {
      __typename
      userObj {
        __typename
        id
      }
    }
  }
}
'''
    write_cases("TestActionsAsync", {
        "test_create_user_fail": [
            run_async(create % ("random-email", "Clarke"), "create_user"),
            result(errors_query("create_user"), {"data": {"create_user": {"id": ID, "errors": {
                "code": "invalid-email", "path": "$", "error": "Given email address is not valid"}}}}),
        ],
        "test_async_actions_error_response_user_role": [
            run_async(err, "test_async_action_error_response", {"x-hasura-role": "user"}),
            result(errors_query("test_async_action_error_response"),
                   {"data": {"test_async_action_error_response": {"id": ID, "errors": {
                       "code": "unexpected", "path": "$", "error": "not a valid json response from webhook"}}}},
                   {"x-hasura-role": "user"}),
        ],
        # Upstream waits 10 s and only checks that `errors` is there; Hasura gives the admin
        # role the same errors as the user role (no `internal` outside dev mode).
        "test_async_actions_error_response_admin_role": [
            run_async(err, "test_async_action_error_response", {"x-hasura-role": "admin"}),
            result(errors_query("test_async_action_error_response"),
                   {"data": {"test_async_action_error_response": {"id": ID, "errors": {
                       "code": "unexpected", "path": "$", "error": "not a valid json response from webhook"}}}},
                   {"x-hasura-role": "admin"}),
        ],
        "test_create_user_success": [
            run_async(create % ("clarke@hasura.io", "Clarke"), "create_user"),
            result(user_query("create_user"), user_result("create_user", "Clarke", "clarke@hasura.io")),
        ],
        "test_create_user_roles": [
            run_async(create % ("blake@hasura.io", "Blake"), "create_user", user1),
            # User 2 cannot see user 1's action.
            result(roles_query, {"data": {"create_user": None}}, user2),
            result(roles_query, {"data": {"create_user": {"id": ID, "output": {"id": 1}}}}, user1),
        ],
        "test_create_user_transformed_success": [
            run_async('mutation {\n  create_user_transformed(email: "clarke@hasura.io", name: "Clarke")\n}\n',
                      "create_user_transformed"),
            result(user_query("create_user_transformed"),
                   user_result("create_user_transformed", "notClarke", "foo@bar.com")),
        ],
        "test_create_user_nested_success": [
            run_async('mutation {\n  create_user_nested(email: "clarke@hasura.io", name: "Clarke")\n}\n',
                      "create_user_nested"),
            result(nested_query, {"data": {"create_user_nested": {
                "__typename": "create_user_nested", "id": ID, "output": {
                    "__typename": "UserIdNested", "userObj": {"__typename": "UserIdObj", "id": 1}}}}}),
        ],
    })


def response_headers():
    mutation = '\n  mutation {\n    create_user(email: "clarke@gmail.com", name: "Clarke"){\n      id\n    }\n  }\n'
    write_cases("TestActionsSyncResponseHeaders", {
        "test_set_cookie_header": [gql(mutation, resp_headers={"Set-Cookie": "abcd"})],
    })


def main():
    async_actions()
    response_headers()


if __name__ == "__main__":
    main()
