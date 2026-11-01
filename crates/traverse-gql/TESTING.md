# `traverse-gql` testing

The GQL front end is tested in layers; each catches a different kind of regression. Use this to
decide where a new test belongs.

## Unit tests (`src/*.rs`)

`#[test]` functions next to the lexer, parser and lowering code. Add one when you teach the
parser a new shape, asserting the AST it produces.

```
cargo test -p traverse-gql --lib
```

## Feature tests (`tests/<feature>.rs`)

One file per `CONFORMANCE.md` row or small group of related rows. Each builds an in-memory graph
with `CREATE` statements, runs the feature and asserts on the result rows: the canonical
end-to-end evidence for each conformance claim.

| Test file | Conformance row(s) | Notes |
|---|---|---|
| `string_concat.rs` | 20.23 | `\|\|` string concat |
| `case_expression.rs` | 20.7 | `CASE` expression |
| `type_predicates.rs` | 19.6, 19.7, 20.24, G111, GA06 | `IS ...` predicates |
| `exists_subquery.rs` | 19.4, GQ22 | `EXISTS { }` subquery |
| `path_search_prefixes.rs` | G005, G017, G018 (parse) | path search prefixes |
| `call_subquery_and_let.rs` | GP01, GP03, GQ09 | CALL block + LET |
| `path_vars_and_keywords.rs` | 16.9, G004, G002, G003, 20.7, GV40 | path variable, NULLIF, mode prefixes, CURRENT_TIME |
| `path_modes.rs` | G003, G010, G013, G016 | WALK/ACYCLIC/ANY |
| `call_subquery_scope.rs` | GP01, GP03 (execute) | CALL { } cross-join |
| `graph_type_bodies.rs` | 4.13, 4.16 | CREATE GRAPH TYPE body |
| `quantified_path_groups.rs` | 16.11, G035 | `((sub)){m,n}` |
| `use_refs_and_value_types.rs` | GQ01, GV55, GV71 | composite USE / IS PATH / IS NOTHING |
| `discarded_ordering_and_next.rs` | GA07, GQ20 | ORDER BY discarded vars, NEXT YIELD |
| `parenthesized_paths.rs` | G050, G051 | parenthesised path WHERE |
| `temporal_literals.rs` | GV39 | DATE '...' typed literals |
| `union_value_types.rs` | GV66, GV67 | dynamic union value types |
| `shortest_k.rs` | G019 | Yens k-shortest |
| `shortest_k_groups.rs` | G020 | k-groups DFS enumeration |
| `next_yield.rs` | GQ20 | NEXT YIELD row piping |
| `double_quoted_idents.rs` | 21.1 | `"foo"` identifiers |
| `when_conditional.rs` | 15.4 | top-level `WHEN` statement |
| `exec_api.rs` | 15.4, 20.24, GD01, GF05, GF06 | `traverse_gql::execute`, the path the server and bindings run: `WHEN`, transaction blocks, trims |

A new feature gets a file (or a section in the closest one) with at least a happy-path test, an
error-path test and a check that an adjacent feature still works.

```
cargo test -p traverse-gql --tests
```

## openGQL TCK harnesses (`tests/tck*.rs`)

These run over the [opengql/tck] feature corpus, a submodule at `tests/opengql_tck/`.

[opengql/tck]: https://github.com/opengql/tck

| Harness | Asserts |
|---|---|
| `tck.rs` | parse and lower coverage stay above their floors |
| `tck_execute.rs` | executable scenarios run without an engine error (floor) |
| `tck_correctness.rs` | result rows match the scenario's expected table |
| `tck_gaps.rs` | nothing: prints a per-scenario list of failures for triage |

Raise a floor when coverage goes up. Catalog statements (CREATE/DROP SCHEMA, GRAPH, GRAPH TYPE)
run through the in-memory catalog rather than `lower::lower()`, which is why the lower floor sits
below the parse floor.

```
cargo test -p traverse-gql --tests tck
cargo test -p traverse-gql --test tck_gaps -- --nocapture
```

## Conformance audit (`tests/conformance_audit.rs`)

Treats `CONFORMANCE.md` as executable documentation. It checks that the row table parses to the
expected number of rows, that every status is recognised, that every `Yes` row cites a test file that
exists and contains at least one `#[test]`, and that the summary counts match the rows. A renamed
test file or a forgotten summary update fails it.

```
cargo test -p traverse-gql --test conformance_audit
```

Adding a row means updating the row-count assertion in the same change.

## Property tests (`tests/properties.rs`)

proptest invariants, about 256 cases each (`PROPTEST_CASES=N` to change): the parser never panics
on random ASCII, keyword soup or deep nesting; integer and string literals round-trip; `NOT NOT x`
nests as expected; the lowerer never panics on parsable input. Failures shrink to a minimal case
and are saved under `tests/proptest-regressions/`, so they replay on the next run.

```
cargo test -p traverse-gql --test properties --release
```

## Fuzzing (`fuzz/`)

cargo-fuzz targets: `parse` and `lower` (random bytes, no panics) and `roundtrip_integer`. The
`fuzz/` crate is excluded from the workspace because libfuzzer needs nightly.

```
cd crates/traverse-gql
cargo +nightly fuzz run parse --max-total-time 600
```

A crash is minimised to `fuzz/artifacts/<target>/crash-<hash>`; rerun that file to reproduce. Any
parse error is a valid result; only a panic is a bug.

## Benchmarks (`benches/`)

Criterion benches for parse and lower throughput: `parse.rs` and `lower.rs` over representative
query shapes, and `quantified_paths.rs` for the quantified-path expansion (unrolling, UNION arms,
the unbounded-iteration cap).

```
cargo bench -p traverse-gql -- --save-baseline main
cargo bench -p traverse-gql -- --baseline main   # after a change
```

Compare baselines taken on the same quiet machine; timings shift with the OS, the allocator and
background load.
