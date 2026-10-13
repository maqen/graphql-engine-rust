# Benchmark: 2026-10-13-hasura-vs-rust-main

- Date: 2026-10-13T21:33:38+00:00
- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.80GHz), Linux 6.18.44-fc-v114
- Load: oha, 10s per case after 3s warmup, concurrency 32, 128
- Engine limits: cpuset=0,1, mem=2g

## Engines

| engine | image | image size | idle memory | startup (median) |
|---|---|---|---|---|
| hasura | mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce.cli-migrations-v3 | 603.0 MB | 221.5 MB | 5.361 s |
| rust | graphql-engine-rust:main-7154861 | 151.5 MB | 7.9 MB | 0.969 s |
| hasura-serve (startup only) | mirror.gcr.io/hasura/graphql-engine:v2.51.0-ce.cli-migrations-v3 | 603.0 MB | - | 0.839 s |

## Latency and throughput

Columns per engine: requests/s, latency percentiles, failed requests (non-200, transport errors), peak memory (MB), mean CPU (cores). `!` marks rows whose response size under load differed from the validated response (likely GraphQL errors); `drain` is how long the event-trigger backlog took to deliver after a mutation case.

| case | c | hasura rps | hasura p50 | hasura p95 | hasura p99 | hasura err | hasura MB | hasura CPU | rust rps | rust p50 | rust p95 | rust p99 | rust err | rust MB | rust CPU | rps rust/hasura |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 01_simple_select | 32 | 1540 | 19.88 ms | 28.99 ms | 37.16 ms | 0 | 147.7 | 1.36 | 10928 | 2.78 ms | 4.63 ms | 6.47 ms | 0 | 10.6 | 1.56 | 7.10x |
| 01_simple_select | 128 | 1455 | 86.09 ms | 112.7 ms | 129.23 ms | 0 | 147.9 | 1.41 | 10060 | 12.18 ms | 19.28 ms | 24.99 ms | 0 | 16.1 | 1.48 | 6.92x |
| 02_select_by_pk | 32 | 1373 | 22.85 ms | 34.5 ms | 40.67 ms | 0 | 145.1 | 1.36 | 9725 | 3.06 ms | 5.31 ms | 7.87 ms | 0 | 16.3 | 1.51 | 7.08x |
| 02_select_by_pk | 128 | 1380 | 92.42 ms | 127.78 ms | 142.71 ms | 0 | 146.2 | 1.44 | 9221 | 13.3 ms | 20.73 ms | 25.11 ms | 0 | 16.2 | 1.54 | 6.68x |
| 03_filter_paginate | 32 | 972 | 31.56 ms | 45.74 ms | 55.87 ms | 0 | 146.8 | 1.24 | 3610 | 7.42 ms | 20.94 ms | 33.05 ms | 0 | 16.2 | 0.89 | 3.71x |
| 03_filter_paginate | 128 | 855 | 148.76 ms | 188.29 ms | 210.73 ms | 0 | 147.6 | 1.33 | 3568 | 32.44 ms | 62.42 ms | 81.94 ms | 0 | 17.1 | 1.02 | 4.17x |
| 04_object_relationships | 32 | 929 | 33.41 ms | 49.62 ms | 58.99 ms | 0 | 147.3 | 1.29 | 2461 | 10.95 ms | 29.77 ms | 43.5 ms | 0 | 16.1 | 0.72 | 2.65x |
| 04_object_relationships | 128 | 782 | 161.17 ms | 201.62 ms | 224.71 ms | 0 | 149.9 | 1.35 | 2491 | 46.07 ms | 84.81 ms | 102.46 ms | 0 | 17.8 | 0.78 | 3.18x |
| 05_array_relationships | 32 | 883 | 35.58 ms | 52.4 ms | 65.83 ms | 0 | 149.3 | 1.37 | 2422 | 10.2 ms | 34.22 ms | 49.46 ms | 0 | 15.8 | 0.74 | 2.74x |
| 05_array_relationships | 128 | 904 | 143.37 ms | 189.89 ms | 347.0 ms | 0 | 153.7 | 1.41 | 2525 | 46.08 ms | 82.57 ms | 99.71 ms | 0 | 18.2 | 0.78 | 2.79x |
| 06_deep_nesting | 32 | 661 | 46.91 ms | 66.37 ms | 74.46 ms | 0 | 152.1 | 1.27 | 1528 | 16.76 ms | 50.2 ms | 71.9 ms | 0 | 16.3 | 0.6 | 2.31x |
| 06_deep_nesting | 128 | 610 | 212.12 ms | 249.66 ms | 284.25 ms | 0 | 150.7 | 1.3 | 1570 | 73.46 ms | 130.11 ms | 203.34 ms | 0 | 19.8 | 0.65 | 2.57x |
| 07_aggregate | 32 | 608 | 51.41 ms | 88.04 ms | 109.5 ms | 0 | 146.7 | 1.06 | 645 | 49.08 ms | 90.22 ms | 113.48 ms | 0 | 17.9 | 0.23 | 1.06x |
| 07_aggregate | 128 | 480 | 271.73 ms | 366.68 ms | 609.82 ms | 0 | 150.4 | 1.1 | 655 | 193.18 ms | 272.36 ms | 314.3 ms | 0 | 20.1 | 0.22 | 1.36x |
| 08_nested_aggregate | 32 | 718 | 44.13 ms | 60.86 ms | 72.93 ms | 0 | 149.1 | 1.32 | 2250 | 11.6 ms | 33.45 ms | 48.47 ms | 0 | 18.1 | 0.7 | 3.13x |
| 08_nested_aggregate | 128 | 776 | 164.56 ms | 255.7 ms | 335.9 ms | 0 | 152.1 | 1.34 | 2175 | 53.08 ms | 97.16 ms | 117.82 ms | 0 | 20.0 | 0.76 | 2.80x |
| 09_user_permissions | 32 | 568 | 54.88 ms | 79.17 ms | 93.08 ms | 0 | 149.2 | 1.33 | 4018 | 7.45 ms | 14.43 ms | 21.67 ms | 0 | 17.1 | 1.3 | 7.08x |
| 09_user_permissions | 128 | 541 | 231.26 ms | 292.14 ms | 461.13 ms | 0 | 155.1 | 1.39 | 4194 | 29.56 ms | 43.53 ms | 54.63 ms | 0 | 20.1 | 1.29 | 7.75x |
| 10_user_aggregate | 32 | 753 | 40.84 ms | 58.1 ms | 69.46 ms | 0 | 146.3 | 1.33 | 6677 | 4.51 ms | 8.11 ms | 11.57 ms | 0 | 19.1 | 1.33 | 8.87x |
| 10_user_aggregate | 128 | 714 | 180.7 ms | 220.81 ms | 234.99 ms | 0 | 150.1 | 1.43 | 6438 | 19.14 ms | 29.55 ms | 37.84 ms | 0 | 19.4 | 1.4 | 9.02x |
| 11_computed_view | 32 | 173 | 194.75 ms | 295.2 ms | 358.75 ms | 0 | 156.2 | 0.39 | 181 | 178.36 ms | 273.04 ms | 323.37 ms | 0 | 17.3 | 0.1 | 1.05x |
| 11_computed_view | 128 | 166 | 659.67 ms | 1686.95 ms | 2312.01 ms | 0 | 165.5 | 0.62 | 178 | 709.89 ms | 854.1 ms | 937.3 ms | 0 | 19.3 | 0.11 | 1.07x |
| 12_variables_fragments | 32 | 731 | 42.72 ms | 59.46 ms | 71.15 ms | 0 | 149.3 | 1.37 | 6829 | 4.42 ms | 7.57 ms | 10.62 ms | 0 | 16.8 | 1.49 | 9.34x |
| 12_variables_fragments | 128 | 771 | 164.52 ms | 194.9 ms | 248.15 ms | 0 | 147.3 | 1.45 | 7125 | 17.36 ms | 26.29 ms | 33.02 ms | 0 | 18.2 | 1.46 | 9.24x |
| 13_action | 32 | 635 | 48.87 ms | 64.46 ms | 78.0 ms | 0 | 145.4 | 1.24 | 704 | 44.29 ms | 49.02 ms | 51.69 ms | 0 | 17.7 | 0.47 | 1.11x |
| 13_action | 128 | 611 | 158.53 ms | 242.37 ms | 1336.07 ms | 0 | 143.8 | 1.32 | 2467 | 50.37 ms | 65.22 ms | 79.51 ms | 0 | 29.1 | 1.23 | 4.04x |
| 14_jsonb_ilike | 32 | 202 | 163.22 ms | 270.91 ms | 321.71 ms | 0 | 149.4 | 0.45 | 237 | 136.71 ms | 205.65 ms | 239.63 ms | 0 | 27.0 | 0.13 | 1.17x |
| 14_jsonb_ilike | 128 | 195 | 575.37 ms | 1407.41 ms | 2013.15 ms | 0 | 158.3 | 0.7 | 244 | 515.87 ms | 656.39 ms | 712.74 ms | 0 | 29.3 | 0.13 | 1.25x |
| 15_distinct_on | 32 | 118 | 291.31 ms | 399.13 ms | 439.52 ms | 0 | 153.6 | 0.21 | 118 | 271.21 ms | 387.85 ms | 458.4 ms | 0 | 29.3 | 0.07 | 1.00x |
| 15_distinct_on | 128 | 115 | 934.61 ms | 2428.73 ms | 3507.73 ms | 0 | 155.2 | 0.3 | 121 | 1036.93 ms | 1255.08 ms | 1383.5 ms | 0 | 29.4 | 0.07 | 1.05x |
| 16_large_result | 32 | 420 | 72.77 ms | 133.56 ms | 170.45 ms | 0 | 213.6 | 1.34 | 552 | 56.43 ms | 107.41 ms | 137.82 ms | 0 | 42.6 | 0.58 | 1.31x |
| 16_large_result | 128 | 375 | 342.35 ms | 400.73 ms | 471.77 ms | 0 | 242.7 | 1.31 | 580 | 216.03 ms | 310.25 ms | 364.63 ms | 0 | 40.4 | 0.56 | 1.55x |
| 17_relationship_filter_order | 32 | 668 | 45.86 ms | 64.01 ms | 75.48 ms | 0 | 162.0 | 1.12 | 1622 | 17.1 ms | 42.38 ms | 60.45 ms | 0 | 35.6 | 0.51 | 2.43x |
| 17_relationship_filter_order | 128 | 614 | 208.28 ms | 257.58 ms | 269.1 ms | 0 | 156.4 | 1.23 | 1569 | 76.58 ms | 127.77 ms | 160.83 ms | 0 | 28.9 | 0.51 | 2.56x |
| 18_introspection | 32 | 108 | 295.09 ms | 373.77 ms | 740.64 ms | 0 | 162.3 | 1.57 | 314 | 100.02 ms | 152.67 ms | 180.25 ms | 0 | 26.8 | 1.8 | 2.91x |
| 18_introspection | 128 | 110 | 1143.86 ms | 1404.5 ms | 3557.21 ms | 0 | 204.8 | 1.59 | 303 | 423.82 ms | 565.97 ms | 627.14 ms | 0 | 29.0 | 1.8 | 2.76x |
| 19_mutation_update | 32 | 336 | 58.68 ms | 306.34 ms | 488.33 ms | 0 (drain 0.0 s) | 156.9 | 0.81 | 683 | 28.67 ms | 154.92 ms | 251.34 ms | 0 (drain 0.0 s) | 27.1 | 0.39 | 2.03x |
| 19_mutation_update | 128 | 252 | 405.13 ms | 1291.43 ms | 1880.37 ms | 0 (drain 0.0 s) | 161.5 | 0.82 | 592 | 189.2 ms | 395.64 ms | 537.95 ms | 0 (drain 0.0 s) | 29.1 | 0.34 | 2.35x |
| 20_mutation_insert_delete | 32 | 312 | 77.75 ms | 198.39 ms | 220.34 ms | 0 (drain 5.0 s) | 154.6 | 1.3 | 906 | 33.17 ms | 62.2 ms | 76.03 ms | 0 (drain 0.5 s) | 31.3 | 0.8 | 2.90x |
| 20_mutation_insert_delete | 128 | 318 | 336.2 ms | 741.57 ms | 786.17 ms | 0 (drain 1.5 s) | 162.2 | 1.29 | 1232 | 101.09 ms | 131.11 ms | 147.92 ms | 0 (drain 5.1 s) | 29.9 | 0.86 | 3.87x |
| 21_rest_endpoint | 32 | 1486 | 20.85 ms | 29.49 ms | 37.67 ms | 0 | 155.7 | 1.33 | 8577 | 3.52 ms | 6.05 ms | 9.31 ms | 0 | 29.9 | 1.33 | 5.77x |
| 21_rest_endpoint | 128 | 1628 | 76.03 ms | 106.6 ms | 149.34 ms | 0 | 155.3 | 1.39 | 9109 | 13.57 ms | 21.3 ms | 26.86 ms | 0 | 29.9 | 1.34 | 5.59x |
