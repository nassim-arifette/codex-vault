# Multi-GB Windows benchmark

Vault `codex-vault 0.2.4`; decimal GB. Synthetic tool-heavy data.

| Input | Compact | Peak RAM (all operations) | Net saved, backups + index included | Exact restore |
| --- | ---: | ---: | ---: | --- |
| 0.02 GB | 0.53 s | 20.9 MB | 64.14% | PASS |

## 0.02 GB

| Operation | Seconds | Peak RAM (MB) | Read (GB) | Read × input | Written (GB) | Write × input |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| scan | 0.059 | 8.6 | 0.000 | 0.000× | 0.000 | 0.000× |
| analyze | 0.107 | 16.4 | 0.022 | 1.000× | 0.000 | 0.000× |
| archive | 0.231 | 12.4 | 0.050 | 2.248× | 0.005 | 0.248× |
| doctor_archive_deep | 0.236 | 16.5 | 0.055 | 2.497× | 0.000 | 0.000× |
| compact_dry_run | 0.233 | 16.7 | 0.028 | 1.249× | 0.000 | 0.000× |
| compact | 0.526 | 17.3 | 0.100 | 4.550× | 0.002 | 0.101× |
| doctor | 0.055 | 9.0 | 0.008 | 0.350× | 0.000 | 0.000× |
| doctor_compacted_deep | 0.087 | 15.0 | 0.015 | 0.698× | 0.000 | 0.000× |
| index | 0.320 | 20.9 | 0.023 | 1.056× | 0.000 | 0.017× |
| index_incremental_noop | 0.040 | 10.3 | 0.008 | 0.363× | 0.000 | 0.000× |
| index_incremental_refresh | 0.085 | 16.1 | 0.012 | 0.567× | 0.000 | 0.005× |
| search_incremental | 0.037 | 9.7 | 0.000 | 0.002× | 0.000 | 0.000× |
| search | 0.044 | 9.7 | 0.000 | 0.002× | 0.000 | 0.000× |
| read | 0.052 | 9.8 | 0.006 | 0.250× | 0.000 | 0.000× |
| restore | 0.574 | 16.7 | 0.084 | 3.823× | 0.023 | 1.026× |
| doctor_final_deep | 0.193 | 16.5 | 0.056 | 2.547× | 0.000 | 0.000× |
| index_restored | 0.251 | 18.8 | 0.074 | 3.347× | 0.000 | 0.008× |
| read_restored | 0.047 | 9.8 | 0.006 | 0.250× | 0.000 | 0.000× |

### FTS index

| Metric | Result |
| --- | ---: |
| Creation | 0.320 s |
| Creation peak RAM | 20.9 MB |
| Incremental refresh (1 changed source) | 0.085 s |
| Incremental refresh peak RAM | 16.1 MB |
| Incremental no-op refresh | 0.040 s |
| Search latency | 0.044 s |
| Verified read latency | 0.052 s |
| index.sqlite | 0.217 MB |
| Indexed text | 0.014 MB |
| Index / indexed text | 16.031x |
| Sources | 2 |
| Passages | 257 |
| Occurrences | 388 |
| Deduplicated occurrences | 33.76% |

Verified backing-source read: PASS (backup).
Duplicate occurrences stored without duplicate passage bodies: 131.
