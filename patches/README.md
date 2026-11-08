# Patched crates

`lancedb-0.40.0/` is the crates.io package of `lancedb` 0.40.0 with one file changed. The root
`Cargo.toml` uses it through `[patch.crates-io]`.

## What is changed

Only `src/job.rs`. Two `match` arms gained a `#[cfg(not(feature = "remote"))]` twin that returns
`Error::Runtime` instead of the `remote`-only `Error::Http`. The other files are the upstream
package as published, without its `Cargo.lock` and `.cargo_vcs_info.json`, plus the upstream
`LICENSE` file, which the crates.io package does not include.

## Why

lancedb 0.38 to 0.40 do not compile without the `remote` feature
([lancedb#4142](https://github.com/lancedb/lancedb/issues/4142)). Selene Retrieval runs LanceDB
locally and does not enable `remote`.

Upstream fixed this in lancedb 0.41.0
([lancedb#4233](https://github.com/lancedb/lancedb/pull/4233)). When Selene Retrieval upgrades to
0.41.0 or newer, delete this folder and the `[patch.crates-io]` entry.

## License

The crate is licensed under the Apache License 2.0, Copyright The LanceDB Authors. See
`lancedb-0.40.0/LICENSE`. The changed file carries a notice under its license header.
