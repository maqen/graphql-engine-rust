# Benchmark: 2026-10-13-hasura-baseline

- Date: 2026-10-13T17:12:43+00:00
- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.80GHz), Linux 6.18.44-fc-v114
- Load: oha, 10s per case after 3s warmup, concurrency 32, 128
- Engine limits: cpuset=0,1, mem=2g

## Engines

| engine | image | image size | idle memory | startup (median) |
|---|---|---|---|---|
| hasura | mirror.gcr.io/hasura/graphql-engine:v2.51.0.cli-migrations-v3 | 631.0 MB | 144.4 MB | 5.194 s |
| hasura-serve (startup only) | mirror.gcr.io/hasura/graphql-engine:v2.51.0.cli-migrations-v3 | 631.0 MB | - | 1.033 s |

## Latency and throughput

Columns per engine: requests/s, latency percentiles, failed requests (non-200, transport errors), peak memory (MB), mean CPU (cores). `!` marks rows whose response size under load differed from the validated response (likely GraphQL errors); `drain` is how long the event-trigger backlog took to deliver after a mutation case.

| case | c | hasura rps | hasura p50 | hasura p95 | hasura p99 | hasura err | hasura MB | hasura CPU |
|---|---|---|---|---|---|---|---|---|
| 01_simple_select | 32 | 1373 | 22.37 ms | 34.97 ms | 45.63 ms | 0 | 163.7 | 1.3 |
| 01_simple_select | 128 | 1307 | 92.81 ms | 136.42 ms | 167.69 ms | 0 | 174.6 | 1.33 |
| 02_select_by_pk | 32 | 1321 | 23.65 ms | 36.28 ms | 44.73 ms | 0 | 171.2 | 1.33 |
| 02_select_by_pk | 128 | 1169 | 107.51 ms | 138.64 ms | 159.57 ms | 0 | 175.1 | 1.37 |
| 03_filter_paginate | 32 | 843 | 36.78 ms | 53.74 ms | 63.14 ms | 0 | 173.4 | 1.21 |
| 03_filter_paginate | 128 | 776 | 163.65 ms | 205.69 ms | 266.75 ms | 0 | 180.4 | 1.27 |
| 04_object_relationships | 32 | 881 | 35.21 ms | 54.96 ms | 70.17 ms | 0 | 177.8 | 1.3 |
| 04_object_relationships | 128 | 739 | 171.19 ms | 212.79 ms | 235.98 ms | 0 | 186.7 | 1.31 |
| 05_array_relationships | 32 | 833 | 37.02 ms | 53.3 ms | 61.74 ms | 0 | 181.7 | 1.2 |
| 05_array_relationships | 128 | 762 | 167.04 ms | 212.59 ms | 231.44 ms | 0 | 184.4 | 1.25 |
| 06_deep_nesting | 32 | 596 | 52.46 ms | 74.87 ms | 92.73 ms | 0 | 182.9 | 1.24 |
| 06_deep_nesting | 128 | 625 | 209.53 ms | 363.94 ms | 530.98 ms | 0 | 190.1 | 1.39 |
| 07_aggregate | 32 | 562 | 56.14 ms | 93.2 ms | 119.44 ms | 0 | 181.4 | 1.02 |
| 07_aggregate | 128 | 416 | 308.22 ms | 356.06 ms | 380.71 ms | 0 | 184.1 | 1.05 |
| 08_nested_aggregate | 32 | 709 | 43.88 ms | 63.37 ms | 76.08 ms | 0 | 181.9 | 1.25 |
| 08_nested_aggregate | 128 | 740 | 171.61 ms | 213.4 ms | 237.23 ms | 0 | 186.3 | 1.29 |
| 09_user_permissions | 32 | 569 | 57.23 ms | 80.22 ms | 103.47 ms | 0 | 181.4 | 1.34 |
| 09_user_permissions | 128 | 562 | 229.45 ms | 395.16 ms | 651.98 ms | 0 | 190.6 | 1.46 |
| 10_user_aggregate | 32 | 756 | 41.25 ms | 59.35 ms | 69.22 ms | 0 | 181.9 | 1.34 |
| 10_user_aggregate | 128 | 815 | 163.15 ms | 197.3 ms | 311.41 ms | 0 | 185.1 | 1.46 |
| 11_computed_view | 32 | 174 | 192.63 ms | 294.87 ms | 327.36 ms | 0 | 192.6 | 0.38 |
| 11_computed_view | 128 | 169 | 648.05 ms | 1643.12 ms | 2389.46 ms | 0 | 201.2 | 0.57 |
| 12_variables_fragments | 32 | 769 | 40.32 ms | 55.13 ms | 65.91 ms | 0 | 183.7 | 1.33 |
| 12_variables_fragments | 128 | 722 | 175.74 ms | 216.08 ms | 317.84 ms | 0 | 186.4 | 1.41 |
| 13_action | 32 | 660 | 48.35 ms | 59.1 ms | 70.37 ms | 0 | 184.3 | 1.18 |
| 13_action | 128 | 783 | 118.68 ms | 203.43 ms | 1410.88 ms | 0 | 187.7 | 1.35 |
| 14_jsonb_ilike | 32 | 210 | 157.96 ms | 251.79 ms | 301.06 ms | 0 | 196.0 | 0.44 |
| 14_jsonb_ilike | 128 | 191 | 584.01 ms | 1453.96 ms | 1936.66 ms | 0 | 203.0 | 0.66 |
| 15_distinct_on | 32 | 112 | 298.65 ms | 475.9 ms | 535.82 ms | 0 | 200.7 | 0.21 |
| 15_distinct_on | 128 | 109 | 875.51 ms | 2721.27 ms | 4142.77 ms | 0 | 201.5 | 0.28 |
| 16_large_result | 32 | 407 | 74.96 ms | 133.13 ms | 164.56 ms | 0 | 270.7 | 1.29 |
| 16_large_result | 128 | 345 | 361.42 ms | 437.68 ms | 480.36 ms | 0 | 279.1 | 1.21 |
| 17_relationship_filter_order | 32 | 684 | 45.56 ms | 62.83 ms | 71.59 ms | 0 | 198.9 | 1.09 |
| 17_relationship_filter_order | 128 | 608 | 209.69 ms | 277.37 ms | 389.72 ms | 0 | 201.9 | 1.15 |
| 18_introspection | 32 | 137 | 144.13 ms | 642.73 ms | 987.63 ms | 0 | 206.6 | 1.61 |
| 18_introspection | 128 | 118 | 1101.1 ms | 3675.08 ms | 3723.57 ms | 0 | 276.1 | 1.49 |
| 19_mutation_update | 32 | 343 | 60.39 ms | 285.05 ms | 448.78 ms | 0 (drain 0.0 s) | 201.7 | 0.81 |
| 19_mutation_update | 128 | 262 | 400.49 ms | 1216.83 ms | 1762.01 ms | 0 (drain 0.0 s) | 205.8 | 0.85 |
| 20_mutation_insert_delete | 32 | 298 | 83.3 ms | 201.54 ms | 230.14 ms | 0 (drain 2.0 s) | 199.9 | 1.21 |
| 20_mutation_insert_delete | 128 | 296 | 416.31 ms | 593.88 ms | 677.82 ms | 0 (drain 2.0 s) | 205.3 | 1.17 |
| 21_rest_endpoint | 32 | 1284 | 24.09 ms | 33.4 ms | 44.61 ms | 0 | 199.4 | 1.28 |
| 21_rest_endpoint | 128 | 1272 | 97.38 ms | 125.47 ms | 144.96 ms | 0 | 201.8 | 1.34 |
