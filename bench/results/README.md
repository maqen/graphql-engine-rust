# Results

| run | engine image | notes |
|---|---|---|
| `2026-10-13-hasura-ce-baseline/` | `v2.51.0-ce.cli-migrations-v3` | **reference baseline** (Community Edition, the compatibility target) |
| `2026-10-13-hasura-baseline/` | `v2.51.0.cli-migrations-v3` (EE build) | first run; kept for comparison, superseded by the CE run |
| `2026-10-13-hasura-vs-rust-main/` | CE vs Rust `main` at 7154861 | first full HTTP comparison; load sent straight to container IPs |
| `2026-10-13-subs-hasura-vs-rust-main/` | CE vs Rust `main` at 2b2501c | subscriptions: 1k/5k live queries + 50 streams, jittered writes |

The Rust images for these runs were a host-built release binary on
`ubuntu:24.04` (about 150 MB), not the repo's `Dockerfile`, so image size is
not representative of the published image.
