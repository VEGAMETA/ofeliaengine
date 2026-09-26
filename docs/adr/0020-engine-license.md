# 0020. Engine license: source-available with tiered royalties

- Status: **Proposed**: terms from the team (answers of 2026-09-26 recorded below); the
  legal text must be written by a lawyer
- Date: 2026-09-26

## Context

CLAUDE.md §1 leaves the engine license to the humans. Until now the rule was: no copyleft
dependencies, so that any license stays possible. The team has now described the terms
they want.

This ADR records those terms and what follows from them for the code base. It is not
the license itself: the enforceable text (definitions, reporting, audit, jurisdiction)
needs a lawyer.

## The terms (from the team, 2026-09-26)

Free to download and redistribute. Source code open to read. Anyone may modify their own
copy; only the original is closed to outside changes (see answer 1). Open to commercial
offers.

**The prices below are preliminary.** Every tier can be negotiated separately.

| Game | Individual, no publisher | With a publisher |
|---|---|---|
| Free, no in-game transactions, no ads, and not itself an advertisement | free | $10 |
| Free, with ads | (question 2) | negotiated |
| Free, with transactions | (question 2) | 2% of transactions |
| Paid, no transactions | $10 | 4% of sales |
| Paid, with transactions | $10 | 5% of sales + 4% of transactions |

## What this is

- **Source-available, not open source.** The OSI definition (and the FSF's) excludes
  royalties on use and restrictions on modification. The model is closest to:
  - Unreal Engine's EULA: source visible, 5% royalty above $1M revenue per product;
  - Defold's license: source visible, free, restrictions on redistributing the engine.
- **CLAUDE.md §0.2 ("Fully open source") must be reworded** once this is accepted. For
  example: "All engine code is public and readable; the license is source-available
  (ADR 0020)."

## Team answers (2026-09-26)

- **Modification (question 1).**
  - Anyone may change their own copy of the engine.
  - The original repository is closed to outside changes: only the authors change it.
  - A modified engine does not change the price. A game on a modified engine pays the
    same tier as a game on the original.
  - Still open: may a modified engine or editor itself be redistributed (published as a
    fork, or given to another studio)? Recommendation: no, except as part of a shipped
    game. Otherwise the tiers could be bypassed through a renamed fork.
- **Prices (questions 2, 4, 5, 6).** The prices are preliminary and can be discussed
  individually. The license text should state the base tiers and "other terms by
  agreement". The lawyer still needs answers to 2 and 4–6 for the base tiers.
- **Licensor (question 7).** There is no company yet: the licensor is the author, a
  natural person. Jurisdiction and languages are still open. Revisit when a company is
  founded; the license should allow assignment to it.

## Consequences for the code base

- **Dependencies.**
  - The permissive-only policy (§15) stays and is now required, not just cautious. MIT,
    Apache-2.0, BSD, Zlib, ISC and similar licenses allow inclusion in a
    source-available, royalty-bearing product with their notices kept
    (`THIRD_PARTY_NOTICES.md`).
  - MPL-2.0 (file-level copyleft) stays behind an ADR.
  - GPL, LGPL and AGPL stay forbidden.
  - Everything vendored or linked today (Jolt, JoltC, ash, glam, bytemuck, Tracy,
    windows-sys, …) is compatible.
- **The original is changed only by the authors.** Outside patches are not accepted
  without a separate agreement. If that changes, contributions need a contributor
  agreement (CLA): without one, contributed code cannot be relicensed or sold under
  commercial terms.
- **No telemetry or DRM for royalty enforcement.** Reporting is self-declared, as with
  Unreal. The engine stays free of phone-home code (performance and trust).
- **Distribution.**
  - The repository stays private and "all rights reserved" until the license text
    exists.
  - A `LICENSE` file is added only with the lawyer's text.
- **Games ship the engine's runtime.** The license must say what a shipped game may
  contain (compiled engine code, the editor or not) and which notices it must carry.

## Open questions for the team

Answered or partly answered on 2026-09-26: 1, 7 (see "Team answers"). Prices are
preliminary and negotiable.

1. **"Closed to modification"** (answered; redistribution of a modified engine is still open):
   - Does this mean no one may change the engine at all?
   - Or that modified versions may not be redistributed as an engine or tool?

   Recommendation: studios may modify the engine for their own games, and may send
   patches upstream. They may not publish or distribute a modified engine or editor.
   Otherwise developers cannot fix a bug that blocks their release.
2. **Individual developers without a publisher:**
   - Which tier applies to free games with ads or with transactions?
   - Is there a revenue threshold above which an individual moves to the publisher
     tier? As written, a single developer earning millions pays $10.
3. **A company that publishes its own game** (a studio without a separate publisher):
   individual tier or publisher tier?
4. **The $10:** one-time per game, per year, or per engine version?
5. **Percentages:**
   - of gross revenue or net (after store fees, refunds, VAT)?
   - per product or per company?
   - with which reporting period and payment terms?
6. **"Transactions"** covers in-game purchases. Does it also cover subscriptions and
   DLC sold through a store?
7. **Licensor and jurisdiction:** an individual or a company, which country's law, and
   the languages of the text (English and Russian?).

## Next steps

1. The team answers the remaining questions: redistribution of a modified engine,
   2–6, jurisdiction and languages.
2. A lawyer drafts the license (and the CLA) from this ADR.
3. CLAUDE.md §0.2 and README are updated. A `LICENSE` file and a contributor guide are
   added.
