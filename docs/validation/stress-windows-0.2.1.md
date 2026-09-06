# Multi-GB Windows benchmark

Vault `codex-vault 0.2.1`; decimal GB. Synthetic tool-heavy data.

| Input | Compact | Peak RAM (all operations) | Net saved, backups + index included | Exact restore |
| --- | ---: | ---: | ---: | --- |
| 1 GB | 7.99 s | 23.2 MB | 73.39% | PASS |
| 5 GB | 166.75 s | 27.0 MB | 73.37% | PASS |
| 10 GB | 158.41 s | 27.2 MB | 73.37% | PASS |

## 1 GB

| Operation | Seconds | Peak RAM (MB) | Read (GB) | Written (GB) |
| --- | ---: | ---: | ---: | ---: |
| scan | 0.870 | 10.0 | 0.000 | 0.000 |
| analyze | 11.431 | 18.4 | 1.000 | 0.000 |
| archive | 13.631 | 12.3 | 2.496 | 0.248 |
| doctor_archive_deep | 10.382 | 16.5 | 2.496 | 0.000 |
| compact_dry_run | 5.832 | 18.7 | 1.248 | 0.000 |
| compact | 7.993 | 18.7 | 2.526 | 0.010 |
| doctor | 0.368 | 9.0 | 0.258 | 0.000 |
| doctor_compacted_deep | 2.365 | 16.1 | 0.516 | 0.000 |
| index | 3.635 | 23.2 | 0.780 | 0.008 |
| search | 0.025 | 9.6 | 0.000 | 0.000 |
| read | 0.269 | 9.8 | 0.248 | 0.000 |
| restore | 21.143 | 16.9 | 3.521 | 1.002 |
| doctor_final_deep | 7.490 | 16.6 | 2.501 | 0.000 |
| index_restored | 11.752 | 21.4 | 3.297 | 0.004 |
| read_restored | 0.620 | 9.8 | 0.248 | 0.000 |

## 5 GB

| Operation | Seconds | Peak RAM (MB) | Read (GB) | Written (GB) |
| --- | ---: | ---: | ---: | ---: |
| scan | 0.233 | 8.5 | 0.000 | 0.000 |
| analyze | 41.716 | 26.8 | 5.000 | 0.000 |
| archive | 88.841 | 12.3 | 12.481 | 1.240 |
| doctor_archive_deep | 64.976 | 16.5 | 12.481 | 0.000 |
| compact_dry_run | 78.483 | 27.0 | 6.240 | 0.000 |
| compact | 166.754 | 27.0 | 12.631 | 0.050 |
| doctor | 6.875 | 8.9 | 1.290 | 0.000 |
| doctor_compacted_deep | 31.715 | 16.1 | 2.580 | 0.000 |
| index | 47.257 | 25.2 | 4.021 | 0.158 |
| search | 0.035 | 9.8 | 0.000 | 0.000 |
| read | 1.634 | 9.8 | 1.240 | 0.000 |
| restore | 171.193 | 16.9 | 17.605 | 5.013 |
| doctor_final_deep | 86.676 | 16.6 | 12.505 | 0.000 |
| index_restored | 108.001 | 22.2 | 16.670 | 0.021 |
| read_restored | 3.433 | 9.8 | 1.240 | 0.000 |

## 10 GB

| Operation | Seconds | Peak RAM (MB) | Read (GB) | Written (GB) |
| --- | ---: | ---: | ---: | ---: |
| scan | 0.103 | 8.5 | 0.000 | 0.000 |
| analyze | 90.318 | 26.5 | 10.000 | 0.000 |
| archive | 190.828 | 12.2 | 24.960 | 2.480 |
| doctor_archive_deep | 132.239 | 16.5 | 24.960 | 0.000 |
| compact_dry_run | 105.940 | 27.1 | 12.480 | 0.000 |
| compact | 158.410 | 27.2 | 25.261 | 0.100 |
| doctor | 11.638 | 8.9 | 2.580 | 0.000 |
| doctor_compacted_deep | 43.973 | 16.2 | 5.160 | 0.000 |
| index | 90.363 | 26.0 | 8.209 | 0.477 |
| search | 0.052 | 9.7 | 0.000 | 0.000 |
| read | 9.849 | 9.8 | 2.480 | 0.000 |
| restore | 346.935 | 16.7 | 35.210 | 10.025 |
| doctor_final_deep | 157.081 | 15.7 | 25.010 | 0.000 |
| index_restored | 183.784 | 22.6 | 33.509 | 0.049 |
| read_restored | 8.601 | 9.8 | 2.480 | 0.000 |
