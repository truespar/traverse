# `traverse-gql` design

ISO/IEC 39075 GQL for Traverse. It lives beside `traverse-cypher`; the
two parsers don't share code, and both produce the statement
representation the one executor runs.

## 1. Why a separate crate

- GQL and Cypher overlap in keywords that mean different things (label
  syntax, `OPTIONAL`, `RETURN` forms, string quoting). Two independent
  parsers keep each grammar unambiguous and leave the Cypher parser,
  the stable path, untouched.
- The GQL dialect can follow new editions of the standard
  (`DIALECT_VERSION`) without disturbing openCypher.
- `traverse-server` builds GQL in by default (the `gql` feature); the
  embedded bindings and the WebAssembly build include it too.

## 2. Sources of truth

In order of authority:

1. **ISO/IEC 39075:2024**, the standard, and its published BNF
   (<https://standards.iso.org/iso-iec/39075/ed-1/en/ISO_IEC_39075(en).bnf.txt>).
   Production names mirror it where reasonable.
2. **`opengql/grammar`**, the ANTLR4 reference grammar (Apache-2.0), to
   cross-check tree shapes.
3. **Gheerbrant & Libkin, "GQL and SQL/PGQ: Theoretical Models and
   Expressive Power"** (VLDB 2025), for pattern-matching semantics where
   the spec text is ambiguous.
4. **Neo4j's published GQL support tables**, the row set
   `CONFORMANCE.md` tracks.

## 3. Pipeline

```
text --lexer--> tokens --parser--> GQL AST --lower--> Cypher AST --> planner / executor
                                       |
                                       +--exec--> statements with no Cypher form
```

- **Lexer** (`lexer.rs`): byte-offset spans, a reserved-word table
  (`RESERVED_WORDS`, case-insensitive, no allocation), double-quoted
  delimited identifiers (section 21.1). Trivia is dropped.
- **Parser** (`parser/`): hand-written recursive descent, a Pratt parser
  for expressions over the full precedence ladder. One module per area:
  catalog statements, graph-type bodies, queries and MATCH, clauses,
  patterns, expressions. It records the deepest nesting and the longest
  operator chain (`parse_with_stats`), which size the execution stack.
- **Lowering** (`lower.rs`): the explicit dialect bridge. GQL forms map
  onto the Cypher AST (`INSERT` -> `CREATE`, `FOR` -> `UNWIND`, `LET` ->
  `WITH`, type predicates -> `typename()` tests, quantified path groups ->
  variable-length edges or UNION ALL of unrollings, `NEXT` chains ->
  one query); where GQL and Cypher semantics differ, the GQL spelling
  goes to a GQL-strict function (`trim` -> `gql_trim`, trims default to a
  space).
- **Execution** (`exec.rs`): parse -> lower -> execute, shared by the
  server and every binding so none of them can skip a step. It installs
  the section 14.5 path-mode guard and the section 14.6 `ANY` limit, which the Cypher
  AST has no place for, and runs the statements that have no Cypher
  form: the section 15.4 `WHEN` conditional and transaction blocks.
- **Catalog statements**: the server handles `CREATE GRAPH` / `DROP
  GRAPH` against its database registry before execution; elsewhere they
  are reported as unsupported.

Keeping the parser, lowering and execution apart means the parser's job
ends at a faithful GQL AST, dialect decisions live in one place, and the
engine can change without touching the grammar.

## 4. Module layout

```
crates/traverse-gql/
+-- CONFORMANCE.md   support, row by row, and known limitations
+-- TESTING.md       test layers and where a new test goes
+-- src/
|   +-- lib.rs       public API: parse, lower, execute_gql
|   +-- lexer.rs     tokens, spans, reserved words (tests in lexer/tests.rs)
|   +-- parser/      mod.rs (statements, token helpers), catalog.rs, graph_type.rs,
|   |                query.rs, clause.rs, pattern.rs, expr.rs, tests.rs
|   +-- ast.rs       GQL AST
|   +-- lower.rs     GQL AST -> Cypher AST (tests in lower/tests.rs)
|   +-- exec.rs      the execution pipeline
|   +-- error.rs     ParseError
+-- tests/           feature tests, TCK harnesses, conformance audit
+-- benches/         criterion: parse, lower, quantified paths
+-- fuzz/            cargo-fuzz targets (outside the workspace; needs nightly)
```

## 5. AST principles

- Every node carries a `Span` (byte offsets into the source).
- Nodes derive `Debug, Clone, PartialEq`; no `Eq` where float literals
  can appear.
- Keywords are `&'static str` into the reserved-word table or enum
  variants, not heap strings.
- The tree stays GQL-shaped; anything Cypher-shaped happens in lowering.

## 6. Errors

`parse` returns `Result<Program, Vec<ParseError>>`. A `ParseError` has a
`Span`, a `ParseErrorKind` (`LexError`, `UnexpectedToken`,
`UnexpectedEof`, `NotImplemented`) and a message naming what was
expected. Parsing stops at the first error, so the list has one entry;
there's no recovery across statements. `exec::errors_to_cypher_error`
turns diagnostics into one engine syntax error carrying the first span,
so editors can mark it; the HTTP API wraps errors in the section 4.9.2
`gql_status_object` envelope.

## 7. Testing

See `TESTING.md`: unit tests beside the code, one feature-test file per
conformance row or group, the openGQL TCK at three levels (parse,
execute, correct rows) plus a gap report, a conformance audit that keeps
`CONFORMANCE.md` honest, property tests, fuzzing and benchmarks.

## 8. Non-goals

- The procedural sublanguage of section 13 (`IF`, `WHILE`, `BEGIN` ... `END`,
  local variables, procedural `RETURN`): it belongs to stored
  procedures, which Traverse doesn't host.
- Enforcing graph types: databases are open-typed property graphs, so
  `CREATE GRAPH TYPE` and typed `CREATE GRAPH` parse but aren't created.
- Several graphs in one statement or request (`USE` switching graphs).
- Formal ISO certification: the aim is that queries written to the
  standard parse and run correctly, measured by `CONFORMANCE.md` and the
  TCK.
