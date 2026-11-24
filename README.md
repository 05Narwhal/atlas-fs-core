# atlas-fs-core

The shared traits and types behind the `atlas-*` filesystem and disk image readers.

- `BlockSource`: positional, thread-safe reads over a device, an image file, memory or a slice of any of them.
  `MemSource`, `Slice` and `FileSource` implement it.
- `FileSystem`: a tree of nodes named by `NodeId`, with `lookup`, `read_dir` (a `DirStream` that fills
  `DirBatch`es), `metadata`, `open` (a `BlockSource` over a file's bytes) and optional extents, extended
  attributes, snapshots and a whole-volume scan.
- `Error`: the one error type every reader returns. Corrupt input is an error, never a panic.

The crate has no `unsafe` code and one dependency (`thiserror`). Format crates such as `atlas-hfsplus`
implement `FileSystem` on top of a `BlockSource`.

```rust
use atlas_fs_core::io::block::{BlockSource, MemSource};

let source = MemSource::new(vec![1u8, 2, 3, 4]);
let mut buf = [0u8; 2];
source.read_at(1, &mut buf).unwrap();
assert_eq!(buf, [2, 3]);
// Reads past the end are errors.
assert!(source.read_at(3, &mut buf).is_err());
```

## Licence

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT license
([LICENSE-MIT](LICENSE-MIT)) at your option.
