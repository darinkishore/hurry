# Design

`hurry` is a command line tool that provides drop-in integrations with your native build tools to provide really, really fast builds. It does this by stacking a variety of tool-specific tricks.

## Rust

For Rust, `hurry` integrates with `cargo` via `hurry cargo`. Here's how it works.

### Running `cargo build`

<!--

TODO: Implement this.

#### Avoiding extraneous recompilation

Cargo caches compiled artifacts, including incrementally compiled workspace members, with a "fingerprint". This fingerprint tells Cargo when a cached artifact is stale and needs to be recompiled.

By default, this fingerprint includes the `mtime` of the file. This means that you can trigger a recompile through changes in comments, changes in formatting, and `touch`es.

-->

#### Branch-specific `target` caches

By default, code from within your workspace (as opposed to dependency code) is built _incrementally_. These incremental artifacts are cached in your `target/` directory.

Let's say you have two branches in your git repository, `A` and `B`. When you switch from `A` to `B`, do a little work (possibly running a `cargo build`), and then switch back to `A`, Cargo will not be able to reuse your incremental build cache for `A`!

Why is this? Two reasons:

1. If you did an incremental build while working in `B`, then your incrementally cached builds of the files while they were in branch `A` have been overwritten by the new build.
2. Even if you didn't do an incremental build, your build cache has been invalidated, because switching branches changed the mtime of your source files, and Cargo uses mtime to determine whether a file has been changed!

`hurry` works around this for you. When it finds that your workspace is in a previously compiled state, it restores your previous local incremental cache and the mtimes of your source files, so Cargo will reuse the cached artifacts.

> :bulb: The longer term solution for this is to invalidate the fingerprint of compiled artifacts when the _contents_ of the compiled source files have changed. This progress is tracked in [rust-lang/cargo#14136](https://github.com/rust-lang/cargo/issues/14136).

### Other `cargo` subcommands

By default, `hurry cargo foo` will execute `cargo foo` and pass through all arguments provided on the command line.

Other subcommands currently do not have any acceleration support.

> :construction: We're working on adding acceleration for other `cargo` subcommands. If you have a specific use case, please open an issue and let us know so we can prioritize it.
