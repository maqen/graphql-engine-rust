Helper servers for the conformance suite, vendored from hasura/graphql-engine
`server/tests-py` (v2.51.0) with a `__main__` added so the runner can start them as
`python3 <name>.py <port>`. See "Helper servers" in `../README.md`.

- `webhook.py`: upstream's `webhook.py`, the auth webhook of `test_webhook.py` (plain HTTP).
- `cookie_auth.py`: the cookie auth webhook of `test_auth_webhook_cookie.py`
  (`auth_webhook_server.py`), echoing `response-set-cookie*` request headers as Set-Cookie.
- `query_echo.py`: the POST-mode webhook of `test_webhook_request_context.py`.
