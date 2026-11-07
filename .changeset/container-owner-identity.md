---
"loro": patch
"loro-internal": patch
---

Add a side-effect-free container owner identity check for validation that must
not enable auto-commit or create a transaction. Existing document access keeps
its ordinary editing behavior.

Retain incoming Update metadata in the experimental native reference oracle
before known-history trimming, so callers can validate the original wire span
without a second owned-text decode.
