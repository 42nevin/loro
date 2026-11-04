---
"loro-crdt": patch
---

Add the opt-in native `shared_arena_experiment` generated-text reservation hook.
The default insertion API and Update format are unchanged. This qualification
hook adopts native immutable slices and explicitly requires backends to account
for any reservation-to-native ownership copy; it does not claim zero-copy
storage-owned live operations. See `context/shared-arena-experiment.md`.
