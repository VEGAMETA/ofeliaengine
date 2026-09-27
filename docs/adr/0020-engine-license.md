# 0020. Engine license: MIT as a placeholder until the final license is chosen

- Status: **Accepted as a placeholder** (team decision, 2026-09-27). The final license is
  chosen before the first release; the source-available terms the team described on
  2026-09-26 are recorded below for that decision.
- Date: 2026-09-26, decision 2026-09-27

## Context

- CLAUDE.md §1 left the engine license to the humans. The rule meanwhile: no copyleft
  dependencies, so that any license stays possible.
- On 2026-09-26 the team described source-available terms with tiered royalties (below),
  with open questions and a lawyer still needed.
- On 2026-09-27 the team decided: MIT for now; once the engine is further along, it will be
  clear what is needed.

## Options

1. **No license** ("all rights reserved") until the final text exists. Legally the most
   conservative, but crates, tools and documents keep saying "undecided".
2. **MIT as a placeholder.** A plain, well-known text; nothing to draft now.
3. **The source-available terms now.** Needs the open answers and a lawyer first.

## Decision

Option 2.

- `LICENSE` at the root holds the MIT text; `Cargo.toml` declares `license = "MIT"` for
  every crate of the workspace.
- The repository stays private and nothing is published (`publish = false`).
- The dependency policy does not change: permissive licenses only (§15), no copyleft,
  MPL-2.0 behind an ADR. It keeps every final license possible, including the
  source-available one.

## Consequences

- **A version released under MIT stays MIT.** Anyone who receives a copy under MIT keeps
  the MIT rights to it; a later license applies only to later versions. While the
  repository is private and nothing is distributed, this costs nothing. **Before the first
  build or source leaves the team, the final license must be decided.**
- A later change of license is a change of the `LICENSE` file, the `license` field and
  this ADR. Code from outside contributors would need their agreement (or a CLA) to be
  relicensed; today all code comes from the authors.
- Fonts and icons keep their own licenses (ADR 0036), as do all dependencies
  (`THIRD_PARTY_NOTICES.md`); MIT is compatible with all of them.

## Recorded for the final decision: the team's source-available terms (2026-09-26)

Free to download and redistribute. Source code open to read. Anyone may modify their own
copy; only the original is closed to outside changes. Open to commercial offers.

**The prices are preliminary.** Every tier can be negotiated separately.

| Game | Individual, no publisher | With a publisher |
|---|---|---|
| Free, no in-game transactions, no ads, and not itself an advertisement | free | $10 |
| Free, with ads | (question 2) | negotiated |
| Free, with transactions | (question 2) | 2% of transactions |
| Paid, no transactions | $10 | 4% of sales |
| Paid, with transactions | $10 | 5% of sales + 4% of transactions |

- **What it would be:** source-available, not open source (the OSI and FSF definitions
  exclude royalties on use and restrictions on modification). Closest models: Unreal
  Engine's EULA (source visible, 5% above $1M revenue per product) and Defold's license.
  CLAUDE.md §0.2 ("Fully open source") would need rewording.
- **Team answers:**
  - Anyone may change their own copy; the original repository is changed only by the
    authors. A modified engine does not change the price.
  - Prices are preliminary; the license would state base tiers and "other terms by
    agreement".
  - The licensor is the author, a natural person, until a company exists; the license
    should allow assignment to it.
- **Open questions:**
  1. May a modified engine or editor be redistributed (a published fork, or given to
     another studio)? Recommendation: no, except inside a shipped game.
  2. Individual developers without a publisher: the tier for free games with ads or
     transactions, and a revenue threshold for moving to the publisher tier.
  3. A company that publishes its own game: individual or publisher tier?
  4. The $10: once per game, per year, or per engine version?
  5. Percentages: of gross or net revenue, per product or per company, reporting period
     and payment terms.
  6. Do "transactions" include subscriptions and DLC sold through a store?
  7. Jurisdiction and the languages of the text.
- **Consequences recorded then:** no telemetry or DRM for royalty enforcement (reporting
  is self-declared); outside contributions would need a CLA; the license must say what a
  shipped game may contain (compiled engine code, the editor or not) and which notices it
  carries; the legal text is written by a lawyer.
