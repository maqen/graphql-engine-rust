Helper servers for the conformance suite, vendored from hasura/graphql-engine
`server/tests-py` (v2.51.0) with a `__main__` added so the runner can start them as
`python3 <name>.py <port>`. See "Helper servers" in `../README.md`.

- `webhook.py`: upstream's `webhook.py`, the auth webhook of `test_webhook.py` (plain HTTP).
- `cookie_auth.py`: the cookie auth webhook of `test_auth_webhook_cookie.py`
  (`auth_webhook_server.py`), echoing `response-set-cookie*` request headers as Set-Cookie.
- `query_echo.py`: the POST-mode webhook of `test_webhook_request_context.py`.
- `graphql_server.py` (with `webserver.py`): upstream's remote GraphQL server, without TLS.
  Needs `pip install -r requirements.txt` (graphene 2).
- `actions_webhook.py`: upstream's `ActionsWebhookHandler` (context.py), the action handler of
  `test_actions.py`. Some actions call back into the engine; it reads the engine's URL and admin
  secret from the JSON file named by `$HGE_CONFORMANCE_ENGINE`.
