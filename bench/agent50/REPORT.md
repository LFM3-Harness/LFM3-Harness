# LFM3 agent benchmark: 50 real tasks

Generated 2026-10-31T12:36:55.122Z. Model `lfm3-1.7b` served by the pinned llama.cpp build; sampling temperature 0.1, top-k 50, repetition penalty 1.1. Each job ran with `--max-turns 16`, `--max-calls 3`, a five-minute deadline, and one isolated workspace under `tmp/bench-agent50/runs/`.

The harness pass is its own contract: a non-empty deliverable verified and re-read. The semantic pass is an external, deterministic content check (required facts, numbers, JSON fields, word bounds). A false pass means the harness accepted output whose content is wrong.

## Summary

| Measure | Result |
| --- | --- |
| Tasks | 50 |
| Harness pass | 43/50 (86.0%) |
| Semantic pass | 41/50 (82.0%) |
| True passes (both) | 41 |
| False passes (harness pass, content wrong) | 2 |
| False failures (content right, harness failed) | 0 |
| Both failed | 7 |
| Opened with a plan turn | 50/50 |
| Reached a review turn | 43/50 |
| Average turns | 5.7 |
| Average / median / p95 duration | 51.5s / 30.5s / 146.8s |
| Total wall time | 2577.0s |

Weakest category by semantic pass rate: **search** (1/5). Slowest task: **t50** at 251.9s.

## By category

| Category | Tasks | Harness | Semantic | Semantic rate | Avg turns | Avg time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| extract | 8 | 8/8 | 8/8 | 100% | 4.9 | 37.4s |
| summarize | 8 | 7/8 | 7/8 | 88% | 5.8 | 48.3s |
| structured | 8 | 8/8 | 8/8 | 100% | 5.4 | 49.0s |
| compute | 8 | 8/8 | 7/8 | 88% | 5.8 | 67.3s |
| synthesis | 8 | 6/8 | 5/8 | 63% | 5.3 | 40.6s |
| search | 5 | 1/5 | 1/5 | 20% | 8.2 | 34.3s |
| format | 5 | 5/5 | 5/5 | 100% | 6.0 | 93.0s |

## Per task

| Task | Category | Harness | Content | Turns | Time | First failure |
| --- | --- | --- | --- | ---: | ---: | --- |
| t01 | extract | pass | pass | 4 | 43.3s |  |
| t02 | extract | pass | pass | 5 | 22.5s |  |
| t03 | extract | pass | pass | 6 | 36.8s |  |
| t04 | extract | pass | pass | 5 | 23.2s |  |
| t05 | extract | pass | pass | 4 | 46.5s |  |
| t06 | extract | pass | pass | 4 | 28.1s |  |
| t07 | extract | pass | pass | 6 | 70.0s |  |
| t08 | extract | pass | pass | 5 | 28.4s |  |
| t09 | summarize | pass | pass | 4 | 25.9s |  |
| t10 | summarize | pass | pass | 4 | 16.3s |  |
| t11 | summarize | pass | pass | 6 | 30.3s |  |
| t12 | summarize | pass | pass | 5 | 37.2s |  |
| t13 | summarize | pass | pass | 5 | 18.6s |  |
| t14 | summarize | pass | pass | 5 | 46.0s |  |
| t15 | summarize | pass | pass | 8 | 65.4s |  |
| t16 | summarize | fail (stalled) | fail | 9 | 146.8s | deliverable missing |
| t17 | structured | pass | pass | 8 | 120.1s |  |
| t18 | structured | pass | pass | 5 | 23.1s |  |
| t19 | structured | pass | pass | 4 | 27.3s |  |
| t20 | structured | pass | pass | 4 | 22.6s |  |
| t21 | structured | pass | pass | 6 | 75.4s |  |
| t22 | structured | pass | pass | 6 | 45.6s |  |
| t23 | structured | pass | pass | 5 | 35.2s |  |
| t24 | structured | pass | pass | 5 | 42.5s |  |
| t25 | compute | pass | pass | 5 | 24.3s |  |
| t26 | compute | pass | pass | 6 | 42.3s |  |
| t27 | compute | pass | fail | 5 | 28.2s | too long (146 > 140 words) |
| t28 | compute | pass | pass | 4 | 21.6s |  |
| t29 | compute | pass | pass | 5 | 29.5s |  |
| t30 | compute | pass | pass | 8 | 217.4s |  |
| t31 | compute | pass | pass | 8 | 134.8s |  |
| t32 | compute | pass | pass | 5 | 40.5s |  |
| t33 | synthesis | fail (stalled) | fail | 3 | 18.3s | deliverable missing |
| t34 | synthesis | pass | pass | 6 | 57.7s |  |
| t35 | synthesis | pass | pass | 5 | 76.4s |  |
| t36 | synthesis | pass | fail | 4 | 22.9s | too long (196 > 160 words) |
| t37 | synthesis | fail (stalled) | fail | 5 | 10.8s | deliverable missing |
| t38 | synthesis | pass | pass | 7 | 58.7s |  |
| t39 | synthesis | pass | pass | 6 | 40.1s |  |
| t40 | synthesis | pass | pass | 6 | 39.9s |  |
| t41 | search | pass | pass | 9 | 77.5s |  |
| t42 | search | fail (stalled) | fail | 8 | 18.4s | deliverable missing |
| t43 | search | fail (stalled) | fail | 12 | 19.0s | deliverable missing |
| t44 | search | fail (stalled) | fail | 9 | 28.7s | deliverable missing |
| t45 | search | fail (stalled) | fail | 3 | 27.9s | deliverable missing |
| t46 | format | pass | pass | 8 | 133.8s |  |
| t47 | format | pass | pass | 4 | 27.3s |  |
| t48 | format | pass | pass | 5 | 30.5s |  |
| t49 | format | pass | pass | 4 | 21.5s |  |
| t50 | format | pass | pass | 9 | 251.9s |  |

## False passes

### t27 (compute)

Goal: Read data/inventory.csv and write out/report.md. State the total stock value, the total number of units, and which item has the largest quantity with that quantity.

Harness: passed after 5 turns. Checks failed: too long (146 > 140 words)

Output excerpt:

> # Inventory Report **Data source:** data/inventory.csv ## Summary - **Total stock value:** $38.40 - **Total number of units:** 560 - **Item with the largest quantity:** washers (quantity: 240) ## Details from CSV | Item | Quantity | Unit Price | |---------|----------|------------| | bolts | 120 | 0.10 | | nuts | 120 | 0.08 | | washers | 240 | 0.03 | |

### t36 (synthesis)

Goal: Read notes/incident.md and docs/policy.md, then write out/report.md. Summarise the incident cause and impact, then state the refund processing time from the policy.

Harness: passed after 4 turns. Checks failed: too long (196 > 160 words)

Output excerpt:

> # Incident Report 214 ## Summary On **14:05** the checkout service returned errors, causing a surge of failed checkouts. The root cause was an **expired database certificate**. The issue was detected at **14:12** and service was restored at **14:47**. ## Cause - An expired database certificate led to errors in the checkout service. - Root cause identified by monitoring/alerting. ## Impact - **1


## All content failures

- **t16** (summarize, harness fail): deliverable missing
- **t27** (compute, harness pass): too long (146 > 140 words)
- **t33** (synthesis, harness fail): deliverable missing
- **t36** (synthesis, harness pass): too long (196 > 160 words)
- **t37** (synthesis, harness fail): deliverable missing
- **t42** (search, harness fail): deliverable missing
- **t43** (search, harness fail): deliverable missing
- **t44** (search, harness fail): deliverable missing
- **t45** (search, harness fail): deliverable missing

## Scope

These are real-model runs of the stated-job loop. They do not qualify chat routing, vision, audio, or d1 classification, and they are not a semantic mission holdout. The content checks are stricter than the harness contract in some places (exact numbers and JSON fields) and looser in others (substring matches, no prose-quality judgement). Raw per-task results, event traces, stderr, and deliverables remain under `tmp/bench-agent50/runs/`.
