# Contributing to Traverse

Thanks for helping. Bug reports, fixes, tests, docs and features are all welcome.

## Before you start

- **Bugs**: open an issue with the version, what you ran (the query, the API call, the binding)
  and what happened. A small `.tvdb` or a script that builds the graph helps a lot.
- **Bigger changes** (a new feature, a change to a file format or a public API): open an issue
  first so we can agree on the approach before you write the code.
- **Security problems**: don't open an issue; see [SECURITY.md](SECURITY.md).

## Building and testing

The [README](README.md) covers building. While working on a change, test the crates it touches
rather than the whole workspace, which is large:

```bash
cargo test -p traverse-cypher            # one crate
cargo test -p traverse-cypher --test tck # the openCypher TCK
cargo clippy -p traverse-cypher --all-targets -- -D warnings
cargo fmt --all
```

- **Server features**: build the server with `--features grpc,tls,mcp,ml` so nothing is left out.
- **Language bindings**: `./run_binding_tests.sh [python|java|node|go|dotnet]` runs that
  binding's tests; for Java, Node.js, Go and .NET it builds the native library first and tests
  against it (Python tests the installed module: `maturin develop` in `crates/traverse-python`).
- **HTTP clients**: `./run_http_tests.sh` runs them against a server on port 7691.
- **Studio**: `cd studio && npm ci && npx vue-tsc --noEmit`, and `npm run build:server` to rebuild
  the bundle the server embeds.

## What we look for

- **Tests** for the change: a regression test for a bug, coverage for a feature.
- **No warnings**: `cargo clippy` and `cargo fmt` clean; the workspace lint table applies.
- **Comments** that say why, briefly, for a reader who wasn't there.
- **Release notes**: user-visible changes get an entry in [RELEASE_NOTES.md](RELEASE_NOTES.md),
  written for users.

## Pull requests

Keep a pull request to one change and describe what it does and why. Maintainers review it
here; an accepted change is applied to the development tree and published with the next update
of this repository, and the release notes credit you.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
Traverse by you, as defined in the Apache-2.0 license, shall be dual licensed as MIT OR
Apache-2.0, without any additional terms or conditions.
