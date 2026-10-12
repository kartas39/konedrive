# fuser 0.18.0, with one change

A copy of the `fuser` crate (MIT, see `LICENSE.md`), version 0.18.0 from crates.io, without its
examples and tests. The one change: `FUSE_INTERRUPT` is handed to the filesystem
(`Filesystem::interrupt`) and no longer answered with `ENOSYS`. Upstream marks this as a TODO
in `src/request.rs`. With `ENOSYS` the kernel stops sending interrupts, and a program waiting in
an `open` for a download cannot be killed until the download ends.

The workspace uses it through `[patch.crates-io]` in the root `Cargo.toml`.
