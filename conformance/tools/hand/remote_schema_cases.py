#!/usr/bin/env python3
"""Write the hand-ported remote schema suites (upstream test_schema_stitching.py).

Upstream's classes set up remote schemas in custom fixtures and check responses in Python, so
the port tool skips them. Here every case starts from an empty metadata (isolation: database),
runs the class fixture's requests as its first steps, then the test's own requests. Upstream's
GraphQL server runs as the `graphql_server` helper; `gql_server.url` becomes
`{{helper:graphql_server}}` and the engine gets `REMOTE_SCHEMAS_WEBHOOK_DOMAIN` for the URL
templates in upstream's YAML.

Responses are not written here: record them from Hasura afterwards (the status of every step
is upstream's expectation, and the runner reports a step that records a different one):

    python3 conformance/tools/hand/remote_schema_cases.py --upstream <graphql-engine>/server/tests-py
    target/debug/hge-conformance --filter remote-schemas/ hasura --record

Re-running this overwrites the recorded cases: record again afterwards.
"""

import argparse
import json
import os
import shutil
import sys

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
from port_tests_py import dump_yaml  # noqa: E402

ROOT = os.path.join(HERE, "..", "..", "suites")
AREA = "remote-schemas"
FILE = "test_schema_stitching.py"
GQL = "{{helper:graphql_server}}"
DIR = "queries/remote_schemas"
INTROSPECTION = "queries/graphql_introspection/introspection.yaml"

METADATA = {
    "version": 3,
    "sources": [{
        "name": "default",
        "kind": "postgres",
        "tables": [],
        "configuration": {"connection_info": {
            "database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"},
            "isolation_level": "read-committed",
            "use_prepared_statements": False,
        }},
    }],
}

UPSTREAM = None
UNSET = object()
# Case name -> upstream method, for the cases of a parametrized method.
PARAMETRIZED = {}


def load(path):
    with open(os.path.join(UPSTREAM, path)) as f:
        return yaml.safe_load(f)


def http(url, body, status=200, headers=None, description=None):
    step = {}
    if description:
        step["description"] = description
    step["url"] = url
    step["status"] = status
    if headers:
        step["headers"] = headers
    step["query"] = body
    step["response"] = None  # recorded from Hasura
    return step


def v1q(body, status=200):
    return [http("/v1/query", body, status)]


def v1md(body, status=200):
    return [http("/v1/metadata", body, status)]


def v1q_f(path, status=200):
    return v1q(load(path), status)


def check_query_f(path):
    """Upstream's check_query_f: the file's request(s), with their URL, headers and status."""
    conf = load(path)
    out = []
    for doc in conf if isinstance(conf, list) else [conf]:
        step = http(doc["url"], doc["query"], doc.get("status", 200), doc.get("headers"),
                    doc.get("description"))
        if doc.get("response_headers"):
            step["resp_headers"] = {k: None for k in doc["response_headers"]}
        out.append(step)
    return out


def add_remote(name, url, headers=None, client_hdrs=False, timeout=None, customization=None,
               introspection_headers=UNSET):
    q = {"type": "add_remote_schema", "args": {"name": name, "comment": "testing " + name, "definition": {
        "url": url, "headers": headers, "forward_client_headers": client_hdrs,
        "timeout_seconds": timeout, "customization": customization}}}
    if introspection_headers is not UNSET:
        q["args"]["definition"]["introspection_headers"] = introspection_headers
    return q


def update_remote(*args, **kwargs):
    q = add_remote(*args, **kwargs)
    q["type"] = "update_remote_schema"
    return q


def remove_remote(name):
    return {"type": "remove_remote_schema", "args": {"name": name}}


EXPORT = {"type": "export_metadata", "args": {}}


def write_suite(cls, setup, tests, skipped=()):
    """`setup`: steps every case starts with; `tests`: {method: steps}."""
    d = os.path.join(ROOT, AREA, cls)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "cases"))
    suite = {
        "description": f"Hand-ported from hasura/graphql-engine server/tests-py/{FILE}::{cls}",
        "upstream": {"file": FILE, "class": cls, "dir": DIR},
        "env": {"REMOTE_SCHEMAS_WEBHOOK_DOMAIN": GQL},
        "isolation": "database",
        "helpers": ["graphql_server"],
    }
    with open(os.path.join(d, "suite.yaml"), "w") as f:
        dump_yaml(suite, f)
    with open(os.path.join(d, "setup.sql"), "w") as f:
        f.write("-- No schema: the class fixture's steps set up each case.\n")
    with open(os.path.join(d, "metadata.json"), "w") as f:
        json.dump(METADATA, f, indent=2)
        f.write("\n")
    for name, steps in tests.items():
        method = PARAMETRIZED.get(name, name)
        case = {"upstream": {"file": FILE, "class": cls, "method": method, "by_hand": True},
                "steps": setup + steps}
        with open(os.path.join(d, "cases", name + ".yaml"), "w") as f:
            dump_yaml(case, f)


def basic():
    customization = {"type_names": {"prefix": "Foo", "mapping": {"String": "MyString"}},
                     "field_names": [{"parent_type": "Hello", "prefix": "my_", "mapping": {}}]}
    hello = f"{GQL}/hello-graphql"
    revert = v1q(update_remote("simple 1", hello, None, False, 60))
    tests = {
        "test_add_schema": v1q(EXPORT),
        "test_update_schema_with_no_url_change":
            v1q(update_remote("simple 1", hello, None, True, 120)) + v1q(EXPORT) + revert,
        "test_update_schema_with_url_change":
            v1q(update_remote("simple 1", f"{GQL}/user-graphql", None, True, 80)) + v1q(EXPORT) + revert,
        "test_update_schema_with_customization_change":
            v1q(update_remote("simple 1", hello, None, False, 60, customization=customization))
            + v1q(EXPORT) + check_query_f(INTROSPECTION) + check_query_f(f"{DIR}/basic_query_customized.yaml")
            + revert + v1q(EXPORT),
        "test_update_schema_with_customization_change_invalid":
            v1q(update_remote("simple 1", hello, None, False, 60,
                              customization={"type_names": {"mapping": {"String": "Foo", "Hello": "Foo"}}}), 400)
            + revert,
        "test_introspection": check_query_f(INTROSPECTION),
        "test_introspection_as_user": check_query_f("queries/graphql_introspection/introspection_user_role.yaml"),
        "test_remote_query": check_query_f(f"{DIR}/basic_query.yaml"),
        "test_remote_subscription": check_query_f(f"{DIR}/basic_subscription_not_supported.yaml"),
        # Upstream only checks the error code and message: the inconsistencies listed under
        # `internal` come in hash-map order.
        "test_add_schema_conflicts": [dict(v1q(add_remote("simple 2", hello), 400)[0], response={
            "error": "cannot continue due to new inconsistent metadata", "path": "$.args",
            "code": "unexpected", "internal": "{{any}}"})],
        "test_remove_schema_error": v1q(remove_remote("random name"), 400),
        "test_reload_remote_schema": v1q({"type": "reload_remote_schema", "args": {"name": "simple 1"}}),
        "test_add_second_remote_schema":
            v1q(add_remote("my remote", f"{GQL}/user-graphql")) + v1q(remove_remote("my remote")),
        "test_json_scalar_dict":
            v1q(add_remote("my remote", f"{GQL}/json-scalar-graphql"))
            + check_query_f(f"{DIR}/json_scalar.yaml") + v1q(remove_remote("my remote")),
        "test_add_remote_schema_with_interfaces":
            v1q(add_remote("my remote interface one", f"{GQL}/character-iface-graphql"))
            + check_query_f(f"{DIR}/character_interface_query.yaml") + v1q(remove_remote("my remote interface one")),
        "test_add_remote_schema_with_union":
            v1q(add_remote("my remote union one", f"{GQL}/union-graphql"))
            + check_query_f(f"{DIR}/search_union_type_query.yaml") + v1q(remove_remote("my remote union one")),
        "test_bulk_remove_add_remote_schema": v1q_f(f"{DIR}/basic_bulk_remove_add.yaml"),
    }
    for method, f in [
        ("test_add_remote_schema_with_interface_err_empty_fields_list", "add_remote_schema_with_iface_err_empty_fields_list"),
        ("test_add_remote_schema_err_unknown_interface", "add_remote_schema_err_unknown_interface"),
        ("test_add_remote_schema_with_interface_err_missing_field", "add_remote_schema_err_missing_field"),
        ("test_add_remote_schema_with_interface_err_wrong_field_type", "add_remote_schema_with_iface_err_wrong_field_type"),
        ("test_add_remote_schema_with_interface_err_missing_arg", "add_remote_schema_err_missing_arg"),
        ("test_add_remote_schema_with_interface_err_wrong_arg_type", "add_remote_schema_iface_err_wrong_arg_type"),
        ("test_add_remote_schema_with_interface_err_extra_non_null_arg", "add_remote_schema_with_iface_err_extra_non_null_arg"),
        ("test_add_remote_schema_with_union_err_no_member_types", "add_remote_schema_with_union_err_no_member_types"),
        ("test_add_remote_schema_with_union_err_unkown_types", "add_remote_schema_with_union_err_unknown_types"),
        ("test_add_remote_schema_with_union_err_subtype_iface", "add_remote_schema_with_union_err_member_type_interface"),
        ("test_add_remote_schema_with_union_err_wrapped_type", "add_remote_schema_with_union_err_wrapped_type"),
    ]:
        tests[method] = check_query_f(f"{DIR}/{f}.yaml")
    write_suite("TestRemoteSchemaBasic", v1q(add_remote("simple 1", hello)), tests)

    write_suite("TestRemoteSchemaBasicExtensions",
                v1q(add_remote("simple 1", f"{GQL}/hello-graphql-extensions")),
                {"test_remote_query": check_query_f(f"{DIR}/basic_query.yaml")})


def tbls():
    sql = 'CREATE TABLE "user" (id int); CREATE TABLE innocent1 (id int); CREATE TABLE innocent2 (id int);'
    track = {"type": "pg_track_tables", "args": {"tables": [
        {"table": {"schema": "public", "name": n}} for n in ("user", "innocent1", "innocent2")],
        "allow_warnings": True}}
    tests = {
        "test_add_schema": v1q(EXPORT),
        "test_add_schema_conflicts_with_tables": v1q(add_remote("simple2", f"{GQL}/hello-graphql"), 400),
        "test_add_second_remote_schema":
            v1q(add_remote("my remote2", f"{GQL}/country-graphql")) + v1q(remove_remote("my remote2")),
        "test_remote_query": check_query_f(f"{DIR}/simple2_query.yaml"),
        "test_remote_mutation": check_query_f(f"{DIR}/simple2_mutation.yaml"),
        "test_add_conflicting_table":
            v1q_f(f"{DIR}/create_conflicting_table.yaml", 400) + v1q_f(f"{DIR}/drop_user_table.yaml"),
        "test_bulk_track_tables_conflicting_with_remote_schema":
            v1q({"type": "run_sql", "args": {"sql": sql}}) + v1md(track)
            + v1md({"type": "pg_untrack_table", "args": {"table": {"schema": "public", "name": "innocent1"}}})
            + v1md({"type": "pg_untrack_table", "args": {"table": {"schema": "public", "name": "innocent2"}}}),
        "test_introspection": check_query_f(INTROSPECTION),
        "test_add_schema_duplicate_name": v1q(add_remote("simple2-graphql", f"{GQL}/country-graphql"), 400),
        "test_add_schema_same_type_containing_same_scalar":
            v1q_f(f"{DIR}/person_table.yaml") + v1q(add_remote("person-graphql", f"{GQL}/person-graphql"))
            + v1q_f(f"{DIR}/drop_person_table.yaml") + v1q(remove_remote("person-graphql")),
        "test_remote_schema_forward_headers":
            v1q(add_remote("header-graphql", f"{GQL}/header-graphql",
                           headers=[{"name": "x-hasura-test", "value": "abcd"}], client_hdrs=True))
            + [http("/v1alpha1/graphql", {"query": "{ wassup }"}, 200, {
                "x-hasura-test": "xyzz", "x-hasura-role": "user", "x-hasura-user-id": "abcd1234",
                "Authorization": "Bearer abcdef"})]
            + v1q(remove_remote("header-graphql")),
    }
    write_suite("TestAddRemoteSchemaTbls", v1q_f(f"{DIR}/tbls_setup.yaml"), tests)


def introspection_headers():
    tests = {}
    for mode, ih in [("legacy", UNSET), ("null", None), ("empty", []),
                     ("distinct", [{"name": "x-introspection-header", "value": "introspection"}])]:
        name = f"test_introspection_and_runtime_headers_{mode}"
        PARAMETRIZED[name] = "test_introspection_and_runtime_headers"
        tests[name] = (
            v1q(add_remote("introspection-headers", f"{GQL}/introspection-headers-graphql?mode={mode}",
                           headers=[{"name": "x-runtime-header", "value": "runtime"}], introspection_headers=ih))
            + v1q(EXPORT) + [http("/v1/graphql", {"query": "{ hello }"})])
    write_suite("TestRemoteSchemaIntrospectionHeaders", [], tests)


def websocket():
    def ws(query):
        return [
            {"ws_connect": {"protocol": "graphql-ws"}},
            {"ws_send": {"message": {"type": "connection_init", "payload": {}}}},
            {"ws_expect": {"message": {"type": "connection_ack"}}},
            {"ws_send": {"message": {"type": "start", "id": "1", "payload": {"query": query}}}},
            {"ws_expect": {"message": {"type": "data", "id": "1"}}},  # payload recorded from Hasura
            {"ws_send": {"message": {"type": "stop", "id": "1"}}},
        ]
    write_suite("TestRemoteSchemaQueriesOverWebsocket", v1q_f(f"{DIR}/tbls_setup.yaml"), {
        "test_remote_query": ws("query { user(id: 2) { id username } }"),
        "test_remote_query_error": ws("query { user(id: 2) { generateError username } }"),
        "test_remote_mutation": ws('mutation { createUser(id: 42, username: "foobar") { user { id username } } }'),
    })


def others():
    write_suite("TestRemoteSchemaResponseHeaders", v1q(add_remote("sample-auth", f"{GQL}/auth-graphql")), {
        "test_response_headers_from_remote": [dict(http("/v1/graphql", {"query": 'query { hello (arg: "me") }'}),
                                                   resp_headers={"Set-Cookie": None})],
    })
    # Upstream compares the remote's root fields with Hasura's; the recorded introspection
    # pins Hasura's whole schema instead.
    write_suite("TestAddRemoteSchemaCompareRootQueryFields",
                v1q(add_remote("default_value_test", f"{GQL}/default-value-echo-graphql")), {
                    "test_schema_check_arg_default_values_and_field_and_arg_types": check_query_f(INTROSPECTION),
                })
    write_suite("TestRemoteSchemaTimeout",
                v1q(add_remote("simple 1", "{{REMOTE_SCHEMAS_WEBHOOK_DOMAIN}}/hello-graphql", timeout=5)), {
                    # Upstream checks the error's message and internal type and message; the
                    # request under `internal` is Haskell http-client's rendering.
                    "test_remote_query_timeout": [dict(check_query_f(f"{DIR}/basic_timeout_query.yaml")[0], response={
                        "errors": [{
                            "message": 'HTTP exception occurred while sending the request to '
                                       '"{{REMOTE_SCHEMAS_WEBHOOK_DOMAIN}}/hello-graphql"',
                            "extensions": {"path": "$", "code": "remote-schema-error", "internal": {
                                "message": "Response timeout", "request": "{{any}}", "type": "http_exception"}},
                        }]})],
                })
    # TestValidateRemoteSchemaQuery only calls check_query_f: port_tests_py.py ports it.
    write_suite("TestRemoteSchemaTypePrefix",
                v1q(add_remote("simple 2", f"{GQL}/user-graphql",
                               customization={"type_names": {"prefix": "Foo", "mapping": {}}})), {
                    "test_add_schema": v1q(EXPORT),
                    "test_introspection": check_query_f(INTROSPECTION),
                })


def validation():
    vdir = f"{DIR}/validation"
    iface = f"{GQL}/character-iface-graphql"

    def suite(cls, customization, tests):
        write_suite(cls, v1q(add_remote("character-foo", iface, customization=customization)),
                    {m: check_query_f(f"{vdir}/{f}.yaml") for m, f in tests.items()})

    suite("TestValidateRemoteSchemaTypePrefixQuery", {"type_names": {"prefix": "Foo", "mapping": {}}},
          {"test_remote_schema_type_prefix_validation": "type_prefix_validation"})
    suite("TestValidateRemoteSchemaFieldPrefixQuery",
          {"field_names": [{"parent_type": t, "prefix": "foo_"} for t in ("Character", "Human", "Droid")]},
          {"test_remote_schema_field_prefix_validation": "field_prefix_validation"})
    suite("TestValidateRemoteSchemaNamespaceQuery", {"root_fields_namespace": "foo"}, {
        "test_remote_schema_namespace_validation": "namespace_validation",
        "test_multiple_remote_schema_with_namespace": "multiple_remote_schema_with_namespace",
    })
    suite("TestValidateRemoteSchemaCustomizeAllTheThings", {
        "root_fields_namespace": "star_wars",
        "type_names": {"prefix": "Foo", "suffix": "_x", "mapping": {"Droid": "Android", "Int": "MyInt"}},
        "field_names": [
            {"parent_type": "Character", "prefix": "foo_", "suffix": "_f", "mapping": {"id": "ident"}},
            {"parent_type": "Human", "mapping": {"id": "ident", "name": "foo_name_f", "droid": "android"}},
            {"parent_type": "Droid", "prefix": "foo_", "suffix": "_f", "mapping": {"id": "ident"}},
            {"parent_type": "CharacterIFaceQuery", "prefix": "super_"},
        ]}, {"test_remote_schema_customize_all_the_things": "customize_all_the_things"})
    write_suite("TestValidateRemoteSchemaCustomization", [], {
        "test_remote_schema_interface_field_validation": check_query_f(f"{vdir}/interface_field_validation.yaml"),
    })
    write_suite("TestRemoteSchemaRequestPayload",
                v1q(add_remote("echo request", f"{GQL}/hello-echo-request-graphql")), {
                    "test_remote_schema_operation_name_in_response": check_query_f(f"{DIR}/basic_query_with_op_name.yaml"),
                })
    # Not ported: TestRemoteSchemaReload stops and restarts the remote server mid-test.


def main():
    global UPSTREAM
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--upstream", required=True, help="graphql-engine's server/tests-py")
    UPSTREAM = p.parse_args().upstream
    basic()
    tbls()
    introspection_headers()
    websocket()
    others()
    validation()


if __name__ == "__main__":
    main()
