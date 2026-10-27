# Local disjoint-import experiment

Verified against code 2026-10-27. **Research only; default off; not published.**

Baseline: `42nevin/loro@5f6788d7f78362e266e40044bd783d435ff14620`.
Mechanism reference:
[eg-strider transform.rs](https://github.com/jackyzha0/eg-strider/blob/e3b80d5e5f88c86121013ba72ef8d974c786f795/crates/eg-strider/src/transform.rs).

## What is integrated

The optional `strider-experiment` feature tries an OT-before-CRDT proof before
the ordinary attached-import diff calculation. If two concurrent edits touch
strictly separated parts of one text, the remote position is shifted by the
local edit's net length when necessary. The resulting native `DeltaRope` patch
uses the original inserted `IdFull` (peer, counter, lamport). The original op
log, validation, state application and future replay remain unchanged.

This is **not** a dependency on eg-strider, a whole engine transplant, or an
implementation of its general event-run transform planner. It tests one narrow
instance of that mechanism inside Loro. It does not replace Fugue ordering,
introduce a second history, change wire formats, or retain an additional cache.

## Exact eligibility boundary

* Only `DiffCalculator::new_for_import()` at the two attached import application
  sites enables the capability. Ordinary calculators, persistent checkout,
  filtered diffs and detached imports cannot take it.
* Before: one single frontier, two peers (base and local). After: exactly two
  frontiers and three peers, adding only the remote peer.
* All three proof changes must already be decoded in the in-memory change store.
  The detector only peeks: it never loads external history bytes or calls block
  parsing. If parsed proof is unavailable, it immediately uses original replay.
  This includes cold, absent, or retired blocks; it does not infer validity from
  encoded headers or current visible length.
* The base is a complete counter-zero change containing exactly one text insert
  at position zero and no dependencies. Its complete native span matches the
  base version vector. Therefore its entity and Unicode positions coincide.
* Local and remote are each a complete counter-zero change with one text
  insert/delete run, depending on precisely the base's last ID. The three runs
  belong to one ordinary root text container. Position bounds are checked;
  deletes must name the exact base target ID, including reverse deletes.
* Closed edited intervals must be strictly separated. Same-position insertions,
  endpoint contact, overlap, style operations, frontiers covering only part of
  a stored run, other graph
  shapes, additional roots, mergeable roots and shallow history fall back.

Here “complete” refers to the change currently stored in the decoded log. A
valid transport prefix can itself be stored as a complete run and qualify;
the eligibility check does not promise that every partial wire transfer falls
back. A later suffix must still retain native replay semantics.

The narrow base restriction is intentional: it proves absence of style anchors
without scanning older history or introducing a new persistent index.
The original prototype forced full base decoding to obtain this proof; the
parsed-only revision deliberately refuses that cost. Cold imports may therefore
fall back while the identical already-parsed graph qualifies. Warm-history setup
cost and retained memory must be reported separately, never hidden as reopen
benefits. Real documents
with accumulated history commonly do **not** qualify. A positive result here
would establish a mechanism, not an application-wide speedup.

## Diagnostics and oracle

Only with this feature, `loro_internal::diff_calc::strider_experiment` exposes
thread-local `set_enabled`, `reset_stats` and `stats`. `set_enabled` returns the
previous value, which test callers must restore. It is not a global production
configuration or stable API. OFF takes the untouched original replay path;
ON records attempts, hits, shape/overlap fallback counts and
`unparsed_fallbacks` (unavailable parsed proof, not necessarily a base cache miss). There are no
per-operation timing clocks in the algorithm.

Validation must compare the same histories against the original OFF path:
visible/rich text, native op identities and versions, old/new cursors, export
and re-import, historical checkout and later conflicting edits/marks. Positive
cases must assert actual hits; negative cases must assert fallbacks. Invalid
input and import atomicity checks remain required. Timing results must include
whole imports and identify this deliberately narrow workload; defaults and
fallback overhead need separate measurement. See the experiment report for
actual run results; this design note alone is not evidence of passing tests.

## Independent borrowed-base experiment

The local follow-on adds a second, default-false thread-local diagnostic switch,
`set_borrowed_base`, within the same already opt-in feature. With it disabled,
the measured parsed-only reference behavior is retained. With it enabled, local
and remote branch changes use ordinary lookup; they can still be decoded and
registered in the arena. Only the initial base proof avoids native Change/Op
and text-arena materialization. Do not call this zero decoding or zero KV/cache
activity: the existing encoded-block loader may read and cache encoded bytes.

If a decoded base exists, the existing proof is used. Otherwise, the encoded
proof accepts only one complete counter-zero change, one initial ordinary-root
text insertion, empty dependencies and no commit message. It reuses header,
container/key, position, delete-ID, timestamp and commit-length parsers, verifies
all required bounds and consumes the complete operation iterator. UTF-8 is
validated through the borrowed value reader and actual Unicode scalar length
must match both encoded length and the base tip. No persistent format, summary
cache, alternate authority or unchecked length inference is introduced.

An invalid or unsupported proof returns to the original replay/error path and
never emits an accelerated delta. Nonempty commit messages and extra operation
runs are deliberately ineligible rather than decoded by a second general
engine. `borrowed_base_attempts` counts encoded-proof requests;
`borrowed_base_proofs` counts successful proofs (a later overlap check can still
fall back), while `hits` proves the resulting delta was actually used.

The paid branch decoding and failed-attempt overhead must remain visible in
qualification. This is a separate source/cohort from the parsed-only result;
normal-looking time differences or successful final strings alone do not prove
performance or native-history equivalence.
