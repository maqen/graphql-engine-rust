# Subscription benchmark: 2026-10-13-subs-hasura-vs-rust-main

- Date: 2026-10-13T21:58:32+00:00
- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.80GHz); engines pinned to cpuset 0,1, client workers (2) to the other cores
- Protocol graphql-transport-ws; 10 live-query writes 2.0 s apart, 10 streamed inserts; 50 stream subscribers per run
- Live queries: 4 shapes: user-role rows (20 user ids = 20 cohorts), admin playlist_by_pk (100 distinct ids), user-role relationship + aggregate (20 user ids), one query shared by all.
- Latency runs from the write's commit to the subscriber's socket delivering the new result, so it includes each engine's refetch interval (1 s by default in both); writes are jittered so they land at random points of that tick, which puts the expected p50 near 0.5 s.
- Memory: median while subscribed minus median of the freshly restarted engine. Polling tx/s: transactions/s in the engine's database while subscribed minus the same engine with no subscribers.
- Client lag: how late the client's event loop woke up (p99); high values mean the client, not the engine, limited the latency numbers.

| engine | subs | ready s | KB/sub | idle CPU | write CPU | polling tx/s | update p50 | update p99 | update max | missed | stream p50 | stream p99 | client lag p99 | errors |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| hasura | 1000 | 3.3 | 60.5 | 0.09 | 0.1 | 16.6 | 550.8 ms | 996.0 ms | 998.5 ms | 0 | 635.4 ms | 981.7 ms | 1.8 ms | 0 |
| rust | 1000 | 2.5 | 30.5 | 0.03 | 0.04 | 4.3 | 288.4 ms | 1005.6 ms | 1006.9 ms | 0 | 466.7 ms | 979.9 ms | 1.4 ms | 0 |
| rust | 5000 | 11.9 | 29.1 | 0.11 | 0.14 | 4.3 | 595.2 ms | 1030.6 ms | 1035.8 ms | 0 | 416.5 ms | 786.2 ms | 1.7 ms | 0 |
| hasura | 5000 | 12.6 | 69.8 | 0.27 | 0.33 | 14.0 | 600.5 ms | 1034.2 ms | 1052.9 ms | 0 | 389.8 ms | 967.8 ms | 2.3 ms | 0 |
