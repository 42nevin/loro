---
"loro": patch
"loro-internal": patch
---

Add an opt-in experimental trusted local current-state/history backing API with
CoW overlays and Written/Retained capture. Ordinary external snapshot import
validation and default behavior are unchanged. Storage faults require an isolated,
discardable candidate; this is not a production recovery or remote import format.
