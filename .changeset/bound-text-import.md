---
"loro": patch
"loro-internal": patch
---

Add an experimental JSON import boundary that verifies every text insertion
against original immutable ledger extents and preserves their backing in live
native history/state while reusing ordinary structural/style validation and
rollback. Full schema authentication remains the application's responsibility.
