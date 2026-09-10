## Coverage

| Coverage | Count |
|---|---:|
| Projects | 6 |
| Languages | 3 |
| PTY sessions | 28 |
| Replay sessions | 242 |
| Long sessions | 3 |
| Blocked / incomplete outcomes | 27 |
| Cancel | 1 |
| Resume | 2 |
| Permission approve | 1 |
| Permission deny | 1 |
| Resize cycles | 14 |
| Scroll interactions | 2 |
| Large patches (>=1000 char args) | 327 |
| New files created | 95 |
| Plans >= 7 steps | 0 |
| Longest plan seen | 5 |
| Total engine events (replay) | 59216 |
| Total tool calls (replay) | 9926 |
| PTY findings | 1 |

Projects: codeleveler, commander, mux, ripgrep, rust-semver, yq
Languages: Go, Rust, TypeScript/JavaScript

## Agent runtime

| Measure | Value |
|---|---:|
| Sessions measured | 35 |
| Median turn wall (s) | 33.7 |
| Median rounds | 2 |
| Median model-wait share | 0.995 |
| Median tool share | 0.002 |
| Median verification share | 0.0 |
| Median harness share | 0.001 |
| Sessions with zero harness residue | 27 |
| Worst turn wall (s) | 1800.5 (H2-impossible) |
| Worst rounds | 73 |
| Worst harness residue (s) | 110.9 (R-resume-b) |
| Model errors | 0 |
| Retries | 0 |
| Input tokens | 38,580,857 (36,696,576 cached) |
| Output tokens | 228,576 |
| Cost (USD) | 5.4225 |

## Terminal process

| Measure | Value |
|---|---:|
| Runs sampled | 28 |
| Peak RSS, median (MB) | 44.9 |
| Peak RSS, worst (MB) | 66.7 |
| End RSS minus peak, worst (kB) | 0 |
| Peak CPU during answer (%) | 0.6 |
| Peak CPU during cancel (%) | 0.4 |
| Peak CPU during diff (%) | 0.8 |
| Peak CPU during idle (%) | 2.0 |
| Peak CPU during resize (%) | 0.6 |
| Peak CPU during scroll (%) | 0.9 |
| Peak CPU during switch (%) | 0.2 |
| Peak CPU during turn (%) | 5.0 |

## Per-session runtime rows

| Run | Turn wall | Model | Tool | Verify | Harness | Rounds | Cost | Outcome |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| H2-impossible | 1800.5 | 1367.5 | 144.2 | 0.0 | 0.8 | 0 | 2.9485 | interrupted/None |
| H1-contradictory | 803.4 | 445.6 | 57.7 | 32.3 | 0.1 | 38 | 0.2108 | completed/answered |
| L1-implementation | 652.3 | 565.7 | 86.3 | 1.2 | 0.2 | 73 | 0.4751 | completed/answered |
| L2-investigation | 464.5 | 739.5 | 8.9 | 27.8 | 0.0 | 59 | 0.7375 | completed/answered |
| PLAN-long | 265.1 | 246.0 | 19.0 | 0.6 | 0.1 | 46 | 0.2554 | completed/answered |
| R-resume-b | 236.9 | 125.9 | 0.1 | 0.0 | 110.9 | 8 | 0.0429 | completed/answered |
| A-commander | 196.3 | 195.4 | 0.9 | 5.0 | 0.0 | 21 | 0.079 | completed/answered |
| R-resume-b | 111.4 | 111.0 | 0.1 | 0.0 | 0.3 | 20 | 0.1968 | completed/answered |
| A-rust-semver | 96.2 | 80.6 | 15.5 | 10.3 | 0.0 | 21 | 0.0644 | completed/answered |
| A-yq | 89.5 | 77.0 | 12.5 | 0.5 | 0.0 | 17 | 0.0637 | completed/answered |
| A-ripgrep | 79.0 | 78.9 | 0.1 | 0.0 | 0.0 | 18 | 0.0625 | completed/answered |
| A-rust-semver | 74.7 | 58.4 | 16.3 | 10.5 | 0.0 | 16 | 0.0526 | completed/answered |
| A-rust-semver | 68.1 | 51.8 | 16.2 | 10.8 | 0.0 | 14 | 0.0388 | completed/answered |
| B-interaction | 54.9 | 51.6 | 0.0 | 0.0 | 3.3 | 13 | 0.0388 | interrupted/None |
| B-interaction | 54.0 | 50.6 | 0.0 | 0.0 | 3.3 | 14 | 0.0399 | interrupted/None |
| A-codeleveler | 42.2 | 42.0 | 0.2 | 375.4 | 0.0 | 8 | 0.0263 | completed/answered |
| A-mux | 36.7 | 32.4 | 4.2 | 0.3 | 0.0 | 6 | 0.0197 | completed/answered |
| P-approve | 33.7 | 9.0 | 24.7 | 22.5 | 0.0 | 2 | 0.0047 | completed/answered |
| A-mux | 32.3 | 27.3 | 5.0 | 0.3 | 0.0 | 6 | 0.0197 | completed/answered |
| P-deny | 31.1 | 6.4 | 24.7 | 0.0 | 0.0 | 2 | 0.0046 | completed/answered |
| R-resume-a | 18.2 | 18.1 | 0.0 | 0.0 | 0.0 | 0 | 0.0028 | None/None |
| P-deny | 9.1 | 8.8 | 0.3 | 0.0 | 0.0 | 2 | 0.0047 | completed/answered |
| R-resume-a | 8.3 | 8.3 | 0.0 | 0.0 | 0.0 | 0 | 0.0025 | None/None |
| P-approve | 6.2 | 5.7 | 0.5 | 15.8 | 0.0 | 2 | 0.0046 | completed/answered |
| S-mux-2 | 5.7 | 5.7 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| X-session-switch | 5.3 | 5.3 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| X-session-switch | 3.9 | 3.9 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| S-ripgrep-1 | 3.5 | 3.5 | 0.0 | 0.0 | 0.0 | 1 | 0.0026 | completed/answered |
| S-rust-semver-2 | 3.4 | 3.4 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| S-mux-1 | 3.3 | 3.3 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| S-ripgrep-3 | 3.3 | 3.3 | 0.0 | 0.0 | 0.0 | 1 | 0.0026 | completed/answered |
| S-rust-semver-1 | 3.2 | 3.2 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| S-mux-3 | 3.1 | 3.1 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
| S-ripgrep-2 | 3.0 | 3.0 | 0.0 | 0.0 | 0.0 | 1 | 0.0026 | completed/answered |
| S-rust-semver-3 | 2.6 | 2.6 | 0.0 | 0.0 | 0.0 | 1 | 0.0023 | completed/answered |
