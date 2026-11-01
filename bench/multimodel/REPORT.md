# Multi-model benchmark — measured results

Probe: `lfm3-desktop --benchmark-chat` (production engine, no webview).
Workspace: `F:/lfm3/tmp/bench-multimodel/workspace`.
Every number below comes from one real run on this machine; nothing is scripted
or replayed.

## Verdict

| Area | Result |
| --- | --- |
| Router final accuracy | 21/21 (100.0%) |
| Router accuracy where d1 is consulted | 21/21 (100.0%) |
| Keyword-only accuracy (same cases) | 14/21 (66.7%) |
| d1 actually consulted | 21/21 |
| d1 overrides attempted / won | 7 / 7 |
| d1 regressions against the keyword route | 0 |
| Keyword blind spots (no group fired) | 3 (3 routed correctly) |
| Chat turns | 12/12 passed |
| Vision role | 2/2 passed |
| Audio role | 1/1 passed through the chat tool |
| Audio synthesis + ASR | 2/2 passed |
| Text conversation | 12/12 graded turns passed |
| Write gates consulted / held | 2 / 0 |
| Evidence gates consulted / advised | 8 / 2 |
| Gate answers rejected as inconclusive | 2 |

## Decision-model routing

Heuristic and final routes are both recorded for every case. Every message of
at least 24 characters is shown to d1; only tiny greetings skip the model. A
confident non-general choice replaces the keyword route, while a confident
`general` answer leaves conversation alone. A `heuristic` final source in the
table below therefore means d1 agreed with the keyword reading, not that d1 was
skipped: all router prompts are longer than 24 characters. Keyword blind spots
are cases where no keyword group fires, so only d1 can recover the route; a
decision override is the expected outcome.

| Case | Expected | Heuristic | Final (source) | bps | Outcome |
| --- | --- | --- | --- | --- | --- |
| r01-look-like-vision | summarize | vision | summarize (decision) | 9815 | pass |
| r02-look-vendor | summarize | vision | summarize (decision) | 7815 | pass |
| r03-what-does-policy | summarize | summarize | summarize (decision) | 6165 | pass |
| r04-brief-incident | summarize | general | summarize (decision) | 8839 | pass |
| r05-find-water-filter | explore | explore | explore (decision) | 9350 | pass |
| r06-connected-greenhouse | related | related | related (decision) | 7085 | pass |
| r07-deliverable-policy | report | report | report (decision) | 9437 | pass |
| r08-recording | voice | voice | voice (decision) | 9758 | pass |
| r09-picture | vision | vision | vision (decision) | 9509 | pass |
| r10-analyse-folder | explore | explore | explore (decision) | 8949 | pass |
| r11-figure-out-gift-cards | summarize | vision | summarize (decision) | 8915 | pass |
| r12-explain-expenses | summarize | summarize | summarize (decision) | 9807 | pass |
| r13-where-onboarding | explore | explore | explore (decision) | 7898 | pass |
| r14-which-note | explore | general | explore (decision) | 7170 | pass |
| r15-hello | general | general | general (heuristic) | 9579 | pass |
| r16-summarise-meeting | summarize | summarize | summarize (decision) | 8165 | pass |
| r17-list-docs | explore | explore | explore (decision) | 9898 | pass |
| r18-draft-report | report | report | report (decision) | 9525 | pass |
| r19-take-a-look-meeting | summarize | vision | summarize (decision) | 9822 | pass |
| r20-gist-vendor | summarize | general | summarize (decision) | 9460 | pass |
| r21-transcribe-recording | voice | voice | voice (decision) | 8643 | pass |

- Median route latency: 122 ms, prompt to final route.
- d1 was consulted for 21 of 21 messages; its choice replaced the keyword route in 20 and agreed with it in 1. Every router prompt here is at least 24 characters, so none was skipped.
- Blind spots are cases where no keyword group fires, so d1 is the only signal
  that can recover the route. They are counted as final failures when the
  keyword guess is wrong and d1 does not correct it.

## Decision gates inside chat turns

The probe records every typed gate answer. A write gate runs before a
`write_file` or `edit_file` creates an approval card; a confident
`contradicts` label holds the change and no card appears, so a mismatch is
visible as a hold rather than as a rejected approval. The evidence gate runs
once per turn after the first read-only batch and can only add a note. Neither
gate can approve or execute work, and an inconclusive answer changes nothing.

## Chat turns, tools, and approvals

| Case | Route (source) | Tools | Approvals | Time | Result |
| --- | --- | --- | --- | --- | --- |
| c01-hello | general (heuristic) | - | 0 | 3.2s | pass |
| c02-list-notes | explore (decision) | list_files | 0 | 5.2s | pass |
| c03-greenhouse | summarize (decision) | read_file | 0 | 3.1s | pass |
| c04-related | related (decision) | related_files | 0 | 5.6s | pass |
| c05-report-allowed | report (decision) | search_files, read_file, write_file | 2 | 7.5s | pass |
| c06-write-denied | general (heuristic) | write_file | 2 | 3.9s | pass |
| c07-vision-number | vision (decision) | look | 0 | 7.3s | pass |
| c08-vision-shapes | vision (decision) | look, list_files, list_files, list_files, search_files, search_files, list_files, read_file, look | 0 | 15.7s | pass |
| c09-voice | voice (decision) | transcribe | 0 | 4.0s | pass |
| c10-memory-set | general (heuristic) | - | 0 | 2.0s | pass |
| c10-memory-recall | general (heuristic) | - | 0 | 1.4s | pass |
| c11-missing-file | summarize (decision) | search_files, list_files | 0 | 5.6s | pass |

Failures:

- none

A pass is only granted after route, tool, approval, file, and content checks
all succeed; the failures column above is exhaustive.

## Vision and audio delivery

- Vision turns used the `look` tool and the tool output contained an
  `observation (LFM3-VL-2B)` block in 2/2 cases.
- Voice turns used the `transcribe` tool and the tool output contained a
  `transcript (LFM3-Audio-2B...)` block in 1/1 cases.
- Vision fixtures are deterministic PNGs generated by `bench/multimodel/assets.mjs`
  (the number `42` and a red circle beside a blue square).

## Audio synthesis and transcription

Each case is spoken locally by LFM3-Audio-2B (`llama-tts`), then transcribed by
the same weight through the chat runtime. WER is computed against the spoken
sentence after normalisation.

| Case | Backend | WER | Keywords | Result |
| --- | --- | --- | --- | --- |
| a01-budget | lfm3-tts | 0% | thursday:yes three:yes | pass |
| a02-tomatoes | lfm3-tts | 0% | water:yes tomatoes:yes | pass |

- **a01-budget** spoken: "The quarterly budget review is scheduled for Thursday at three."
transcript: "The quarterly budget review is scheduled for Thursday at 3."
- **a02-tomatoes** spoken: "Please water the tomatoes every two days."
transcript: "Please water the tomatoes every two days."

## Scope and caveats

- This run measures one local machine and one pinned runtime; it does not
  qualify other hardware.
- The router's keyword gate is part of the measured system. Blind-spot cases
  show its cost in accuracy; they are not hidden from the score.
- A chat "pass" requires the production route, the expected tool, the approval
  outcome, and the graded content. Nothing is graded from the probe's own
  claims.
- Audio WER is text-level; it does not measure diarisation or punctuation.
- No data leaves the machine; the probe only calls loopback ports.
