Helper servers for the conformance suite, vendored from hasura/graphql-engine
`server/tests-py` (v2.51.0) with a `__main__` added so the runner can start them as
`python3 <name>.py <port>`. See "Helper servers" in `../README.md`.

- `webhook.py`: upstream's `webhook.py`, the auth webhook of `test_webhook.py` (plain HTTP).
