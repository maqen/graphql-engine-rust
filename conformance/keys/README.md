Throwaway keys for the JWT conformance cases: the runner signs tokens with the private keys
(`hge-conformance` `jwt` headers), and the suites configure `HASURA_GRAPHQL_JWT_SECRET` with the
public ones. Test-only; never use them anywhere else.
