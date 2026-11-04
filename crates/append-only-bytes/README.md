<div align="center">
  <h1><code>append-only-bytes</code></h2>
  <h3><a href="https://docs.rs/append-only-bytes">Documentation</a></h3>
  <p></p>
</div>

If an array is append-only and guarantees that all the existing data is immutable, we can safely share slices of this array across threads, while the owner can still safely append new data to it.

This is safe because no mutable byte has more than one owner. If there isn't enough capacity for a new append, `AppendOnlyBytes` will not deallocate the old memory if a `ByteSlice` is referring to it. The old memory will be deallocated only when all the `ByteSlice`s referring to it are dropped.

# Example

```rust
use append_only_bytes::{AppendOnlyBytes, BytesSlice};
let mut bytes = AppendOnlyBytes::new();
bytes.push_slice(&[1, 2, 3]);
let slice: BytesSlice = bytes.slice(1..);
bytes.push_slice(&[4, 5, 6]);
assert_eq!(&*slice, &[2, 3]);
assert_eq!(bytes.as_bytes(), &[1, 2, 3, 4, 5, 6])
```

# Features

- `serde`: support serde serialization and deserialization
- `u32_range`: support `u32` range for `ByteSlices` method

## Qualification fork: external immutable owners

This directory vendors append-only-bytes 0.1.12 from crates.io (upstream
https://github.com/zxch3n/append-only-bytes, MIT license), with the additive
`ImmutableBytes` unsafe trait and `BytesSlice::from_immutable` constructor.
The constructor adopts the backing allocation without copying. Slice cloning,
subranges and same-owner adjacent RLE merges retain the same owner. The native
append-only allocator and its existing copy behavior are unchanged.

Implementing the trait requires an independently owned, immutable, initialized,
address-stable byte range for the entire owner lifetime. Keeping a database
object alive is not enough. Borrowed transactions, pointers to movable inline
values and pages that may be reclaimed are not suitable implementations.

The Sanakirja qualification wrapper in `experiments/immutable-page-lease` is one
narrow implementation with exclusive-file ownership and no payload reclamation.
This is not a generic claim that any database reservation can escape its guard.

Downstream Cargo workspaces must patch `append-only-bytes` to this fork explicitly;
Cargo does not inherit patches declared by dependencies.
