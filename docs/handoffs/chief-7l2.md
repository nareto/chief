# chief-7l2 Handoff and Test Evidence (Backfill)

This document backfills auditable evidence for the `chief-7l2` child-ticket chain:

- `chief-awp`
- `chief-bkk`
- `chief-jgm`
- `chief-vod`
- `chief-itt`
- `chief-4ns`

## Implementation Commits

| Ticket | Commit |
| --- | --- |
| `chief-awp` | `362c38b406756801634c1ea1ee23040491a6a6f5` |
| `chief-bkk` | `b6dee3266d1f7e5fb6b3daf8db5282b4ac23b635` |
| `chief-jgm` | `2af82bc9afe10b9a1e622084e71e7f742921359d` |
| `chief-vod` | `3984e0fc29e43e71d30bbfa5c2e8fe1ce76eb3d5` |
| `chief-itt` | `20662971ef0dd6d615c94b9dc33b5240264e85db` |
| `chief-4ns` | `586df2b9721431d6d719d2038a1e2a836f8a7062` |

## Per-Ticket `cargo test` Evidence

All runs were executed on `2026-03-05` UTC, in the required ticket order, with one full `cargo test` run per ticket.

Raw artifacts:

- `.chief/evidence/chief-7l2-test-runs.tsv`
- `.chief/evidence/chief-awp.cargo-test.log`
- `.chief/evidence/chief-bkk.cargo-test.log`
- `.chief/evidence/chief-jgm.cargo-test.log`
- `.chief/evidence/chief-vod.cargo-test.log`
- `.chief/evidence/chief-itt.cargo-test.log`
- `.chief/evidence/chief-4ns.cargo-test.log`

Execution summary:

| Ticket | Started (UTC) | Finished (UTC) | Result |
| --- | --- | --- | --- |
| `chief-awp` | `2026-03-05T17:37:00Z` | `2026-03-05T17:37:07Z` | `pass` |
| `chief-bkk` | `2026-03-05T17:37:07Z` | `2026-03-05T17:37:14Z` | `pass` |
| `chief-jgm` | `2026-03-05T17:37:14Z` | `2026-03-05T17:37:21Z` | `pass` |
| `chief-vod` | `2026-03-05T17:37:21Z` | `2026-03-05T17:37:28Z` | `pass` |
| `chief-itt` | `2026-03-05T17:37:28Z` | `2026-03-05T17:37:35Z` | `pass` |
| `chief-4ns` | `2026-03-05T17:37:35Z` | `2026-03-05T17:37:42Z` | `pass` |

## Lightweight Mechanism for Future Ticket Chains

Use `ops/per_ticket_cargo_test.sh` to produce consistent per-ticket evidence:

```bash
ops/per_ticket_cargo_test.sh chief-awp chief-bkk chief-jgm chief-vod chief-itt chief-4ns
```

By default this writes:

- `.chief/evidence/<run-id>-cargo-test-runs.tsv`
- `.chief/evidence/<run-id>-<ticket-id>.cargo-test.log`

Optional env vars:

- `CHIEF_EVIDENCE_DIR` to override output directory
- `CHIEF_EVIDENCE_RUN_ID` to set a custom run identifier
