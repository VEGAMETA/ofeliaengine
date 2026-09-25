# Architecture Decision Records

Every architectural decision (new subsystem, dependency change, data format, public API,
threading model, build configuration) is recorded here as `NNNN-short-title.md`
(CLAUDE.md §3). Decisions change only through a new ADR that supersedes the old one.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-workspace-and-build.md) | Workspace layout, toolchain and build profiles | Proposed |

## Template

```markdown
# NNNN. Title

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
What problem are we solving, and what constraints apply (link CLAUDE.md sections,
research notes in docs/research).

## Options
At least two. For each: how it works, pros, cons.

## Measurements / arguments
Numbers with hardware, build profile and commit. If something could not be measured
yet, say so explicitly and say where and when it will be measured.

## Decision

## Consequences
What becomes easier, what becomes harder, what must be revisited and when.
```
