# Benchmark: 2026-10-13-hasura-ce-baseline

- Date: 2026-10-13T17:38:59+00:00
- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.80GHz), Linux 6.18.44-fc-v114
- Load: oha, 10s per case after 3s warmup, concurrency 32, 128
- Engine limits: cpuset=0,1, mem=2g

## Engines

| engine | image | image size | idle memory | startup (median) |
|---|---|---|---|---|
| hasura | mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce.cli-migrations-v3 | 603.0 MB | 103.7 MB | 5.041 s |
| hasura-serve (startup only) | mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce.cli-migrations-v3 | 603.0 MB | - | 0.804 s |

## Latency and throughput

Columns per engine: requests/s, latency percentiles, failed requests (non-200, transport errors), peak memory (MB), mean CPU (cores). `!` marks rows whose response size under load differed from the validated response (likely GraphQL errors); `drain` is how long the event-trigger backlog took to deliver after a mutation case.

| case | c | hasura rps | hasura p50 | hasura p95 | hasura p99 | hasura err | hasura MB | hasura CPU |
|---|---|---|---|---|---|---|---|---|
| 01_simple_select | 32 | 1501 | 20.62 ms | 29.19 ms | 36.63 ms | 0 | 126.9 | 1.31 |
| 01_simple_select | 128 | 1348 | 93.83 ms | 126.9 ms | 192.6 ms | 0 | 134.5 | 1.39 |
| 02_select_by_pk | 32 | 1365 | 23.02 ms | 33.61 ms | 40.34 ms | 0 | 133.2 | 1.33 |
| 02_select_by_pk | 128 | 1235 | 101.36 ms | 137.11 ms | 147.89 ms | 0 | 135.3 | 1.4 |
| 03_filter_paginate | 32 | 850 | 36.75 ms | 51.66 ms | 60.87 ms | 0 | 134.8 | 1.23 |
| 03_filter_paginate | 128 | 782 | 160.53 ms | 201.81 ms | 226.86 ms | 0 | 139.8 | 1.3 |
| 04_object_relationships | 32 | 895 | 35.06 ms | 52.68 ms | 62.5 ms | 0 | 140.1 | 1.34 |
| 04_object_relationships | 128 | 802 | 159.35 ms | 192.67 ms | 248.34 ms | 0 | 150.1 | 1.36 |
| 05_array_relationships | 32 | 826 | 37.86 ms | 51.49 ms | 60.11 ms | 0 | 139.4 | 1.22 |
| 05_array_relationships | 128 | 781 | 167.38 ms | 206.59 ms | 256.69 ms | 0 | 147.3 | 1.36 |
| 06_deep_nesting | 32 | 617 | 52.29 ms | 79.67 ms | 94.72 ms | 0 | 142.3 | 1.31 |
| 06_deep_nesting | 128 | 589 | 214.28 ms | 263.05 ms | 572.68 ms | 0 | 147.6 | 1.28 |
| 07_aggregate | 32 | 551 | 55.01 ms | 100.65 ms | 130.15 ms | 0 | 143.0 | 1.03 |
| 07_aggregate | 128 | 421 | 302.25 ms | 371.22 ms | 431.74 ms | 0 | 144.3 | 1.08 |
| 08_nested_aggregate | 32 | 773 | 40.82 ms | 58.19 ms | 70.94 ms | 0 | 142.2 | 1.26 |
| 08_nested_aggregate | 128 | 701 | 179.84 ms | 234.56 ms | 437.7 ms | 0 | 148.8 | 1.33 |
| 09_user_permissions | 32 | 532 | 58.47 ms | 78.26 ms | 95.34 ms | 0 | 144.4 | 1.32 |
| 09_user_permissions | 128 | 498 | 250.93 ms | 327.08 ms | 383.03 ms | 0 | 151.2 | 1.39 |
| 10_user_aggregate | 32 | 804 | 38.66 ms | 55.22 ms | 63.97 ms | 0 | 143.9 | 1.35 |
| 10_user_aggregate | 128 | 714 | 178.13 ms | 217.75 ms | 240.64 ms | 0 | 145.3 | 1.41 |
| 11_computed_view | 32 | 182 | 176.26 ms | 305.34 ms | 360.18 ms | 0 | 153.5 | 0.36 |
| 11_computed_view | 128 | 168 | 652.35 ms | 1647.84 ms | 2177.54 ms | 0 | 162.1 | 0.59 |
| 12_variables_fragments | 32 | 860 | 36.44 ms | 49.54 ms | 56.28 ms | 0 | 146.5 | 1.37 |
| 12_variables_fragments | 128 | 809 | 157.22 ms | 190.26 ms | 202.9 ms | 0 | 150.2 | 1.46 |
| 13_action | 32 | 657 | 48.06 ms | 59.56 ms | 68.43 ms | 0 | 147.6 | 1.15 |
| 13_action | 128 | 882 | 131.61 ms | 171.09 ms | 1109.85 ms | 0 | 150.0 | 1.4 |
| 14_jsonb_ilike | 32 | 221 | 149.98 ms | 240.22 ms | 300.1 ms | 0 | 157.4 | 0.45 |
| 14_jsonb_ilike | 128 | 206 | 530.36 ms | 1348.3 ms | 1854.34 ms | 0 | 164.7 | 0.67 |
| 15_distinct_on | 32 | 116 | 285.49 ms | 442.16 ms | 521.09 ms | 0 | 161.8 | 0.21 |
| 15_distinct_on | 128 | 114 | 852.64 ms | 2660.81 ms | 3794.95 ms | 0 | 163.2 | 0.27 |
| 16_large_result | 32 | 410 | 76.04 ms | 130.35 ms | 159.74 ms | 0 | 230.3 | 1.28 |
| 16_large_result | 128 | 372 | 348.45 ms | 621.17 ms | 967.45 ms | 0 | 232.5 | 1.28 |
| 17_relationship_filter_order | 32 | 793 | 40.25 ms | 63.32 ms | 78.14 ms | 0 | 162.6 | 1.22 |
| 17_relationship_filter_order | 128 | 665 | 192.63 ms | 232.24 ms | 306.07 ms | 0 | 162.2 | 1.16 |
| 18_introspection | 32 | 158 | 127.83 ms | 570.57 ms | 783.08 ms | 0 | 166.8 | 1.63 |
| 18_introspection | 128 | 134 | 791.99 ms | 1308.9 ms | 7091.44 ms | 0 | 212.1 | 1.61 |
| 19_mutation_update | 32 | 392 | 51.32 ms | 272.48 ms | 409.53 ms | 0 (drain 0.0 s) | 160.4 | 0.82 |
| 19_mutation_update | 128 | 299 | 345.7 ms | 1111.27 ms | 1609.63 ms | 0 (drain 0.0 s) | 166.3 | 0.87 |
| 20_mutation_insert_delete | 32 | 360 | 64.26 ms | 180.15 ms | 216.17 ms | 0 (drain 1.5 s) | 162.8 | 1.24 |
| 20_mutation_insert_delete | 128 | 330 | 390.86 ms | 583.41 ms | 916.62 ms | 0 (drain 2.0 s) | 168.5 | 1.21 |
| 21_rest_endpoint | 32 | 1695 | 18.23 ms | 25.42 ms | 31.91 ms | 0 | 161.1 | 1.33 |
| 21_rest_endpoint | 128 | 1612 | 77.89 ms | 104.33 ms | 116.59 ms | 0 | 162.7 | 1.35 |
