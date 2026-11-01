# `traverse-gql` ISO/IEC 39075 Conformance

Per-feature status against the two tables Neo4j publishes for its GQL
support: **mandatory** (ISO 39075 section 24.2 minimum conformance, listed in
subclause order) and **optional** (GQL feature IDs `Gxxx`).

Status describes the product: the server's GQL route and the embedded
bindings, which share `traverse_gql::execute`. Each row cites the parser
entry point, AST variant, executor function or test that backs it; when
one of those changes, update the row. `tests/conformance_audit.rs` checks
that the tables parse, that the summary matches the rows, and that every
test file a row cites exists and has tests.

## How it's tested

* The openGQL TCK (`tests/opengql_tck`, a git submodule) at three levels:
  `tests/tck.rs` (the grammar accepts it), `tests/tck_execute.rs` (it runs
  on the engine) and `tests/tck_correctness.rs` (it returns the rows the
  spec wants); `tests/tck_gaps.rs` lists what fails at each level.
* Per-feature tests, cited in the rows.

The TCK harness keeps an in-memory catalog of schemas and graph types to
run the catalog scenarios. The product's catalog is the server's database
registry, which handles `CREATE GRAPH` and `DROP GRAPH` (see GG01 and the
limitations below).

## Status legend

| Status | Meaning |
|---|---|
| Yes | Supported: the parser accepts it, lowering routes it, the executor produces the right result. |
| Partial | Accepted, but lowering or execution is incomplete, *or* supported only under a Cypher spelling. |
| No | Not supported: a query using it fails at parse or execute. |
| Deferred | Out of scope for now; listed for completeness. |

---

## Mandatory features (ISO 39075 section 24.2)

Order matches Neo4j's published table, by subclause.

| Subclause | Description | Status | Evidence / Comment |
|---|---|---|---|
| 4.9.2 | GQL-status objects (status codes for errors & notifications) | Yes | HTTP error responses for the GQL dialect carry a `gql_status_object` envelope (section 4.9.2): `gql_status` (5-character code), `condition` (class name from `iso_condition_for`), `statement_type`, `message`, `statement_text`, `position { offset, line, column }` for syntax errors, and `nested_status_objects`. Every error carries the code of its condition: the ISO code where the standard has one (`22012` division by zero, `22003` numeric value out of range, `22007` invalid datetime format, `22G03` invalid value type, `25G03` write in a `READ ONLY` block, `42001` invalid syntax, `G1001` edges still exist) and Neo4j's published extension otherwise (`42N62` variable not defined, `42N48` unknown function, `22N79` uniqueness violated, ...), with the class as fallback. The same code reaches Bolt, gRPC and the embedded bindings. The flat `error` field stays alongside; openCypher clients get only the flat shape. |
| 4.11  | Graph pattern matching | Yes | `MATCH ... RETURN ...` end to end via `parse_match_clause`; the TCK's matching scenarios run in `tests/tck_execute.rs` and `tests/tck_correctness.rs`. |
| 4.13  | GQL object types (`NODE` / `ANY NODE` / `VERTEX` / `ANY VERTEX` / `RELATIONSHIP` / `EDGE` ...) | Yes | All eight spellings are type names in value-type predicates (`n IS NODE`, `r IS ANY EDGE`, ...; see 19.6), `parse_type_kind_keyword`. Inside graph-type bodies (`CREATE GRAPH TYPE t { NODE ... }`, section 17.2) `parse_inline_graph_type_body` reads them into `GraphTypeElement::{Node, Edge}` (`EdgeKind` keeps the spelling), with an optional `ANY` and the parenthesized node-type form (`(p:Person&Student {name STRING NOT NULL})`); element lists are validated (duplicate property names 42000, label-set cardinality outside 1-64 22G0N/22G0P, `AS COPY OF` with a non-equivalent closed type G2000). Graph-type bodies parse and are checked in the TCK harness (`tests/graph_type_bodies.rs`); the server doesn't create typed graphs (see the limitations). |
| 4.16  | Predefined value types (`BOOLEAN` / `BOOL` / `FLOAT` / `INTEGER` / `SIGNED INTEGER` / `INT` / `STRING` / `VARCHAR`) | Yes | Value-type predicates (`x IS INTEGER`, `x IS NOT STRING`; see 19.6) take every canonical spelling. In type bodies, `try_parse_value_type_name` resolves them to `ValueTypeName`: `BOOLEAN`/`BOOL`, `INTEGER`/`INT`/`SIGNED INTEGER`, `STRING`/`VARCHAR`, plus `FLOAT`, `DATE`, `LOCAL TIME`, `LOCAL DATETIME`, `ZONED TIME`/`TIME`, `ZONED DATETIME`/`DATETIME`, `DURATION`, `ANY`. Property declarations take `propName :: TYPE [NOT NULL]` (ISO) and `propName : TYPE` (Neo4j), as `PropertyDecl { name, value_type, not_null, span }`. |
| 9.1   | Nested procedure specification (combining `UNION` / `UNION ALL`) | Yes | `parse_union_part`, `Statement::Query.unions`, lowered by `lower_union_part` to `cy::Union { all }`. |
| 13.2  | `INSERT` statement | Yes | `parse_insert_clause`, `Clause::Insert(InsertClause)`, lowered to Cypher `CREATE`. `CREATE` followed by a pattern is accepted as an alias (Cypher compatibility). |
| 13.3  | `SET` statement | Yes | `parse_set_clause`, three `SetItem` variants: `Property` (`=`), `Merge` (`+=`), `Labels` (`:Label`). The order-of-evaluation note Neo4j makes applies here too (same executor). |
| 13.4  | `REMOVE` statement | Yes | `parse_remove_clause`, `RemoveItem::Property` / `Labels`. |
| 13.5  | `DELETE` statement | Yes | `parse_delete_clause`, `DeleteClause { detach, targets }`; `DETACH DELETE` via `is_detach_delete`. |
| 14.4  | `MATCH` / `OPTIONAL MATCH` | Yes | `MatchClause { optional, patterns, where_clause }`; `optional` when the clause leads with `OPTIONAL MATCH`. |
| 14.9  | `ORDER BY` / `SKIP` / `OFFSET` / `LIMIT` | Yes | `ReturnClause { order_by, skip, limit }`; `parse_return_clause` (parser/clause.rs) takes `SKIP` and `OFFSET` as synonyms. |
| 14.10 | `FINISH` (primitive write-only result) | Yes | `Clause::Finish(Span)`; `FINISH` is a reserved word. |
| 14.11 | `RETURN` statement | Yes | `parse_return_clause`, `ReturnClause { distinct, items, order_by, skip, limit }`. |
| 15.4  | Conditional statement (`WHEN` at statement level) | Yes | `WHEN <cond> THEN <stmt> [ELSE <stmt>] [END WHEN]` parses to `Statement::When` (`parse_when_statement`). `traverse_gql::execute` evaluates the condition once on the engine with no bindings (literals, parameters, `SESSION_USER` and functions of those; a variable is an error, a non-boolean a type error), then runs THEN on true and ELSE on false or null; false without ELSE runs nothing. Either branch can read or write. The procedural sublanguage (`IF`/`WHILE`/`BEGIN`...`END`, local variables) is out of scope (`DESIGN.md` section 9). Tests: `tests/exec_api.rs` (product path), `tests/when_conditional.rs`. |
| 16.2  | `LIMIT` clause | Yes | See 14.9. |
| 16.4  | Graph pattern (composite) | Yes | Comma-separated `PathPattern` list in MATCH / INSERT. |
| 16.5  | Insert graph pattern | Yes | See 13.2: INSERT reuses the pattern grammar. |
| 16.6  | `ORDER BY` clause | Yes | See 14.9. |
| 16.7  | Path pattern expression | Yes | `PathPattern { start, chain }` over `NodePattern` / `EdgePattern`; variable length via `EdgeQuantifier`. |
| 16.8  | Label expression | Yes | `LabelExpression`: `Label`, `Wildcard` (`%`), `Conjunction` (`&`), `Disjunction` (`|`), `Negation` (`!`). |
| 16.9  | Path variable reference | Yes | `MATCH p = (a)-[r]->(b)` binds `p` to the path: `PathPattern.variable`, lowered to `cy::PatternPart.variable`. Path values work with `nodes(p)`, `relationships(p)`, `size(p)` and the other path functions. |
| 16.11 | Graph pattern quantifier | Yes | `((sub)){m,n}`, `+`, `*`, `?` and `{n}` parse (recursive `PathElement::{Step, Group}` + `PathQuantifier`). A single-edge inner pattern lowers to a variable-length edge (`*m..n`). A multi-step inner pattern unrolls: an exact count into an `n x K`-hop chain, a range into a UNION ALL of the exact counts (`expand_range_quantifiers`), and unbounded forms (`+`, `*`, `{m,}`) the same way up to `UNBOUNDED_ITERATION_CAP` = 10 iterations (Neo4j's practical depth for variable-length edges); cascaded ranges expand to a fixed point within a 1024-arm budget. Group variables bind per iteration as section 16.11 lists: for a single-step group `((x)-[r]->(y)){m,n}`, `LogicalPlan::BindQppGroups` rebuilds the `x` / `y` node lists and `r` the edge list, with inner label and property constraints held at every iteration; unrolled multi-step groups bind theirs through `LogicalPlan::BindVarLists`. Limitations below. Tests: `tests/quantified_path_groups.rs`. |
| 16.17 | Sort specification list (`ASC` / `DESC`) | Yes | `OrderItem.direction: SortDirection::{Asc, Desc}`. |
| 16.19 | Offset clause | Yes | See 14.9. |
| 19.3  | Comparison predicate (`=`, `<>`, `<`, `<=`, `>`, `>=`) | Yes | `BinaryOp::{Eq, Neq, Lt, Lte, Gt, Gte}`; `<>` and `!=` both lex to `Neq`. |
| 19.4  | `EXISTS` predicate (subquery) | Yes | `EXISTS { MATCH ... RETURN ... }` and the pattern form `EXISTS { (a)-[r]->(b) }` via `parse_subquery_body`, lowered to `cy::Expr::FullExistentialSubquery(SingleQuery)`; the `exists(value)` function call still works. Tests: `tests/exists_subquery.rs`. |
| 19.5  | `IS [NOT] NULL` predicate | Yes | `Expression::IsNull { negated }`, binding at additive precedence so `a IS NULL = b IS NULL` parses right (parser/expr.rs `parse_additive_with_is_null`). |
| 19.6  | Value type predicate (`IS BOOLEAN`, `IS STRING` ...) | Yes | `Expression::TypePredicate` for BOOLEAN/BOOL, STRING/VARCHAR, INTEGER/INT/SIGNED INTEGER, FLOAT, NUMBER, NODE/VERTEX/ANY NODE/ANY VERTEX, RELATIONSHIP/EDGE/ANY RELATIONSHIP/ANY EDGE, PATH, LIST/ARRAY, MAP, DATE, LOCAL TIME, ZONED TIME/TIME, LOCAL DATETIME, ZONED DATETIME/DATETIME, DURATION, ANY, NOTHING; lowered to `typename(expr) = '<TYPE_NAME>'`. Always BOOLEAN, never null. Tests: `tests/type_predicates.rs`. |
| 19.7  | `IS [NOT] NORMALIZED` | Yes | Optional NFC/NFD/NFKC/NFKD form, backed by the `unicode-normalization` crate (`gql_is_normalized`); null input gives false. Tests: `tests/type_predicates.rs`. |
| 20.2  | Value expression primary | Yes | Literals, variables, parameters, parentheses, lists, maps, property access, indexing, function calls. |
| 20.3  | `SESSION_USER` value expression | Yes | Parses as a parameterless noun (or `SESSION_USER()`). The HTTP API sets it per request: `api-user` when an API key is configured, `anonymous` otherwise; outside a request (embedded use) it is null. Tests: `tests/path_vars_and_keywords.rs`. |
| 20.7  | `CASE` expression / `nullIf` / `coalesce` | Yes | Simple and searched `CASE ... WHEN ... THEN ... END`, `coalesce(...)`, and `nullif(a, b)` (null when `a = b`, else `a`, with three-valued logic on null operands). Tests: `tests/case_expression.rs`, `tests/path_vars_and_keywords.rs`. |
| 20.9  | Aggregate functions (`avg`, `count`, `max`, `min`, `sum`) | Yes | All five, in the executor's aggregation (`traverse-cypher` `executor/aggregate.rs`); `count(DISTINCT x)` too. As in Neo4j, `sum()` over no rows is 0, not null. |
| 20.11 | Property reference | Yes | `Expression::Property { base, name }`. |
| 20.21 | Numeric value expression (`+`, `-`, `*`, `/`, `%`) | Yes | `BinaryOp::{Add, Sub, Mul, Div, Mod}` + `UnaryOp::Neg`. |
| 20.22 | Numeric value function (`char_length`, `character_length`) | Yes | Code-point counts, distinct from `size()`, which keeps Cypher's semantics. |
| 20.23 | String concatenation operator (`\|\|`) | Yes | `TokenKind::PipePipe` at additive precedence, lowered to `cy::BinOp::Concat`: strings only, null propagates, no coercion (unlike Cypher's `+`). Tests: `tests/string_concat.rs`. |
| 20.24 | Character string functions (`left`, `lower`, `normalize`, `right`, `trim`, `upper`) | Yes | `left`, `right`, `upper`/`toupper`/`touppercase`, `lower`/`tolower`/`tolowercase`, `normalize(string [, form])` (NFC by default). `trim` follows section 20.24 in the GQL dialect: U+0020 only, including the `TRIM(... FROM ...)` form (see GF06); Cypher's `trim` keeps stripping any whitespace. Tests: `tests/exec_api.rs`. |
| 21.1  | Names and variables (lexical) | Yes | Plain identifiers, backtick-delimited (`` `weird name` ``) and double-quoted delimited identifiers (section 21.1's primary form, `""` escaping a quote) lex to `TokenKind::Ident` / `TokenKind::DelimitedIdent` (`lex_double_quoted_ident`); single quotes stay string literals (section 20.2). The GQL lexer is its own, so this doesn't touch the Cypher dialect. Tests: `tests/double_quoted_idents.rs`. |
| 22.15 | Grouping operations / counting with and without duplicates | Yes | `count(DISTINCT x)`; `ReturnClause.distinct` for `RETURN DISTINCT`. |

**Mandatory score: 40 yes / 0 partial / 0 no** (of 40 rows).

---

## Optional features

Sorted by GQL feature ID. Rows match Neo4j's published support list.

| ID | Description | Status | Evidence / Comment |
|---|---|---|---|
| G002 | Different-edges match mode (`DIFFERENT RELATIONSHIPS`) | Yes | `MATCH DIFFERENT RELATIONSHIPS ...` is `PathMode::Trail`: Cypher's default and section 14.5's TRAIL. Tests: `tests/path_vars_and_keywords.rs`. |
| G003 | Explicit `REPEATABLE ELEMENTS` keyword | Yes | `MATCH REPEATABLE ELEMENTS ...` lowers as `PathMode::Walk`: `find_variable_length_paths` drops within-path edge uniqueness. Tests: `tests/path_vars_and_keywords.rs`. |
| G004 | Path variables (`p = (a)-[]-(b)`) | Yes | See mandatory 16.9. |
| G005 | Path search prefix (`SHORTEST` etc. in a path pattern) | Yes | `parse_match_path_prefixes` takes the full prefix grammar between `MATCH` and the pattern; `path_mode` and `path_selector` sit on every `MatchClause`. Tests: `tests/path_search_prefixes.rs`. |
| G010 | Explicit `WALK` keyword | Yes | `MATCH WALK ...` lowers as `PathMode::Walk`: the same edge may repeat within a path. The path mode applies to the whole statement (see the limitations). Tests: `tests/path_modes.rs`. |
| G011 | Advanced path modes: `TRAIL` | Yes | `TRAIL` is the executor's default. Tests: `tests/path_modes.rs`. |
| G013 | Advanced path modes: `ACYCLIC` | Yes | `MATCH ACYCLIC ...` lowers as `PathMode::Acyclic`: node uniqueness per path on top of edge uniqueness. Tests: `tests/path_modes.rs`. |
| G016 | `ANY` path search | Yes | `MATCH ANY ...` adds `LIMIT 1` to the outermost RETURN (`apply_path_selector_limit`), `ANY k` adds `LIMIT k`; a `LIMIT` the query states wins. Tests: `tests/path_modes.rs`. |
| G017 | `ALL SHORTEST` path search | Yes | Lowers to `cy::ShortestPathMode::All` on each pattern part. Tests: `tests/path_search_prefixes.rs`. |
| G018 | `ANY SHORTEST` path search | Yes | A synonym for `SHORTEST`; both lower to `cy::ShortestPathMode::Single`. Tests: `tests/path_search_prefixes.rs`. |
| G019 | Counted shortest path search (`SHORTEST k`) | Yes | `ShortestPathMode::Counted(k)` -> `LogicalPlan::ShortestPath { k: Some(_), .. }` -> `bfs_k_shortest`, Yen's algorithm with BFS inside: up to `k` loopless paths per `(source, target)` pair, shortest first, filling from longer lengths when fewer than `k` share the minimum. Per pair, not a global LIMIT. Tests: `tests/shortest_k.rs`. |
| G020 | Counted shortest group search (`SHORTEST k GROUPS`) | Yes | `ShortestPathMode::CountedGroups(k)` -> `bfs_k_groups`: enumerates loopless paths up to the maximum depth, groups them by length and returns every path in the first `k` lengths, per pair. Enumeration stops at 10,000 paths (`ENUMERATION_CAP`) on dense graphs. Tests: `tests/shortest_k_groups.rs`. |
| G035 | Quantified paths (`(pattern){m,n}`) | Yes | See mandatory 16.11. |
| G036 | Quantified edges | Yes | `EdgeQuantifier { min, max }` (`*1..2` and the other forms). |
| G043 | Complete full edge patterns (`-[r]->`, `<-[r]-`, `-[r]-`) | Yes | `EdgeDirection::{Outgoing, Incoming, Undirected}`. The tilde forms (`~[...]~>`) aren't recognised, as in Neo4j. |
| G050 | Parenthesised path pattern with WHERE clause | Yes | `parse_path_pattern` reads `((pattern) WHERE pred)` (two-token `(` `(` lookahead) into `PathPattern.inner_where`; `lower_match_body` ANDs it with any outer `MATCH ... WHERE`. Tests: `tests/parenthesized_paths.rs`. |
| G051 | Parenthesised path pattern: non-local predicate | Yes | The inner WHERE can reference both endpoints (`WHERE b.age > a.age`): it's evaluated once the path's bindings exist. Tests: `tests/parenthesized_paths.rs`. |
| G060 | Bounded graph pattern quantifier | Yes | `*n..m`. |
| G061 | Unbounded graph pattern quantifier | Yes | `*n..`, `*..m`, `*`: both bounds optional in `EdgeQuantifier`. |
| G074 | Label expressions: wildcard label (`%`) | Yes | `LabelExpression::Wildcard`. |
| G111 | `IS [NOT] LABELED` predicate | Yes | `TypeKind::Labeled(LabelExpression)` with the full section 14.7 label-expression grammar (single labels, `A&B`, `A|B`, `!A`, `%`), lowered to a test on `labels(n)`; `IS NOT LABELED` negates the whole expression, as in Neo4j. Tests: `tests/type_predicates.rs`. |
| GA06 | Value type predicates | Yes | Same implementation as mandatory 19.6. |
| G115 | Property exists predicate (`PROPERTY_EXISTS`) | Yes | `property_exists(entity, 'propName')`: true when the property is present and not null, like `entity.prop IS NOT NULL`. |
| GA07 | Ordering by discarded binding variables | Yes | `MATCH (n:P)-[:R]->(m:Q) RETURN n.name ORDER BY m.age`: the sort sees the frame before projection, so bindings the RETURN drops can still order it (section 16.6). Tests: `tests/discarded_ordering_and_next.rs`. |
| GB01 | Long identifiers | Yes | No length limit on `Ident` tokens beyond the lexer's UTF-8 scan. |
| GB03 | Double-solidus comments (`// ...`) | Yes | `//` to end of line (lexer.rs `skip_trivia`). |
| GD01 | Updatable graphs | Yes | INSERT / SET / REMOVE / DELETE. Transaction commands run as a block in one call: `START TRANSACTION [READ ONLY \| READ WRITE] <statements> COMMIT` runs its statements in order on the caller's transaction (one HTTP request, one embedded call) and returns the last statement's rows with every statement's counts; over HTTP a failing statement rolls back the whole block. A READ ONLY block refuses statements that write (25G03) before any runs; a block ending in `ROLLBACK` can't contain writes, which couldn't be discarded within one call, so it runs only if it reads. An explicit transaction over Bolt, gRPC or the embedded API spans calls and can roll back. Tests: `tests/exec_api.rs`. |
| GD02 | Graph label set changes (`SET n:Label`) | Yes | `SetItem::Labels`. |
| GD04 | `DELETE` with simple expression | Yes | `DeleteClause.targets` is a `Vec<Expression>`. |
| GE07 | Boolean `XOR` | Yes | `BinaryOp::Xor` (parser/expr.rs `parse_xor`); Neo4j's precedence note applies. |
| GF01 | Enhanced numeric functions (`abs`, `floor`, `sqrt`, `MOD`, `CEILING`) | Yes | All of them, plus `ceiling` and `mod()` beside `ceil` and `%`, and `power()`. |
| GF02 | Trigonometric functions (`acos`, `asin`, `atan`, `cos`, `cosh`, `cot`, `degrees`, `radians`, `sin`, `sinh`, `tan`, `tanh`) | Yes | All twelve, plus `atan2`. |
| GF03 | Logarithmic functions (`exp`, `log10`, `ln`) | Yes | `exp`, `log10`, and `ln` as a synonym of `log` (natural logarithm). |
| GF05 | Multi-character trim (`btrim`, `ltrim`, `rtrim`) | Yes | `btrim` / `ltrim` / `rtrim(source [, characters])` strip, from both ends / the start / the end, every character that occurs in `characters`; without it, U+0020 in the GQL dialect (Cypher strips any whitespace). Null in, null out. Tests: `tests/exec_api.rs`. |
| GF06 | Explicit `TRIM` function | Yes | `TRIM([LEADING \| TRAILING \| BOTH] [<char>] FROM <source>)` (`parse_trim_args`), lowered to `gql_trim(source, char, end)`: one trim character, a space by default, both ends by default; a trim character that isn't exactly one character is a data exception (22027). `LEADING` / `TRAILING` / `BOTH` stay usable as names outside this syntax. Tests: `tests/exec_api.rs`. |
| GG01 | Graph with open graph type | Yes | `CREATE GRAPH name [IF NOT EXISTS]`, bare or `ANY`, creates a database in the server's registry; `DROP GRAPH name [IF EXISTS]` drops one. Tests: `tests/graph_type_bodies.rs` (parsing). |
| GL01 | Hexadecimal literals (`0x...`) | Yes | `lex_radix_int` with radix 16. |
| GL02 | Octal literals (`0o...`) | Yes | `lex_radix_int` with radix 8. |
| GP01 | Inline procedure (`CALL { ... }` subqueries) | Yes | `cy::Clause::CallSubquery` -> `LogicalPlan::CallSubquery { input, imports, body, return_aliases }`: the body is planned on its own (`plan_single_query_with_hints`); `CallSubqueryOp` runs it per outer row, seeded with that row, and joins each inner row onto it (inner names win, section 9.4). The body's RETURN aliases stay in scope after the call. Tests: `tests/call_subquery_scope.rs`. |
| GP03 | Inline procedure with explicit nested variable scope | Yes | `CALL (imports) { body }` sets `LogicalPlan::CallSubquery.imports`: the body sees only the imported names, and the rest of the outer row is back in scope after the call. Tests: `tests/call_subquery_scope.rs`, `tests/call_subquery_and_let.rs`. |
| GP04 | Named procedure calls | Yes | `CallClause { name: Vec<String>, args, yields }`. |
| GQ01 | `USE graph` clause | Yes | `parse_use_statement` takes all three section 7.2 reference shapes: simple (`USE social`), composite (`USE a.b.c`) and dynamic (`USE graph.byName('social')` or `graph.byName($param)`), as `Statement::Use { reference: UseReference }`, prefixing the statements it applies to (`USE g MATCH ... RETURN ...`, inside transaction blocks too). `traverse_gql::use_target` gives the graph a request names; the server runs the request on that database (over its `database` field; an unknown one is `42NA7`), and an embedded database runs it when `USE` names it and refuses any other graph (`execute_gql_on`). One request uses one graph: two different targets are `25G04`. Tests: `tests/use_refs_and_value_types.rs` (parsing), `tests/exec_api.rs` (routing and execution). |
| GQ03 | Composite query: `UNION` | Yes | See mandatory 9.1. |
| GQ08 | `FILTER` statement | Yes | `FilterClause { predicate }`; `FILTER WHERE pred` and `FILTER pred`. |
| GQ09 | `LET` statement | Yes | `LET name = expr [, name = expr ...]` lowers to one `WITH <scope>, expr AS name` per binding, so each binding sees the ones before it (section 9.3). The scope is tracked through MATCH / WITH / CALL / FOR / UNWIND / LET (`collect_match_variables`, `collect_with_aliases`) so each projection carries every prior binding. Tests: `tests/call_subquery_and_let.rs`. |
| GQ10 | `FOR` statement with list value | Yes | `ForClause { variable, list }`, lowered to `UNWIND list AS variable`. |
| GQ12 | `OFFSET` clause | Yes | A synonym for `SKIP` (parser/clause.rs). |
| GQ13 | `ORDER BY` and page statement: `LIMIT` | Yes | See 14.9. |
| GQ14 | Complex expressions in sort keys | Yes | `OrderItem.expr` is any `Expression`. |
| GQ16 | Pre-projection aliases in sort keys | Yes | `ORDER BY` resolves RETURN aliases the way Cypher does. |
| GQ20 | Advanced linear composition with `NEXT` | Yes | `NEXT [YIELD a [AS b], ...]` separators land in `Program.separators`; `try_fuse_next_chain` splices a NEXT-separated query chain into one Cypher query, turning each non-final `RETURN` into a `WITH` (keeping DISTINCT / ORDER BY / SKIP / LIMIT) filtered by the YIELD list. Without YIELD every column passes; `YIELD a AS b` renames; a YIELD name the previous RETURN lacks is an error. `;`-separated programs don't pipe rows. Tests: `tests/next_yield.rs`, `tests/discarded_ordering_and_next.rs`. |
| GQ22 | `EXISTS` predicate: multiple `MATCH` | Yes | See mandatory 19.4: the subquery body is a full linear query, so several `MATCH` / `WITH` / `WHERE` work inside it. |
| GV39 | Temporal types: `date`, `local datetime`, `local time` | Yes | Typed literals (`DATE '2024-01-15'`, `LOCAL_DATETIME '...'`, `LOCAL_TIME '...'`, `DURATION 'P1D'`, ...) parse in `parse_primary` to calls of the matching constructor; the function forms (`date('...')`) and the parameterless keywords (`CURRENT_DATE`, ...) work too. Constructors: `date`, `time`, `datetime`, `local_time`, `local_datetime`, `zoned_time`, `zoned_datetime`, `duration`. Tests: `tests/temporal_literals.rs`. |
| GV40 | Temporal types: zoned datetime / zoned time | Yes | `time()`/`zoned_time()`, `datetime()`/`zoned_datetime()`, and the `CURRENT_TIME` / `CURRENT_TIMESTAMP` keywords. Tests: `tests/temporal_literals.rs`. |
| GV50 | List value types | Yes | `Expression::List` -> `cy::Expr::ListLiteral`. |
| GV55 | Path value types | Yes | `IS PATH` (`TypeKind::Path`) is true for path variables bound by `MATCH p = ...`; the path functions (`nodes()`, `relationships()`, `head()`, `tail()`, `last()`, `size()`) take them. Tests: `tests/use_refs_and_value_types.rs`. |
| GV66 | Open dynamic unions | Yes | `IS T1 \| T2 \| ...` (after a type other than `LABELED`) lowers to an OR of per-type `typename()` checks; `IS NOT T1 \| T2` negates the OR once, keeping three-valued logic on null (section 19.6). In type bodies, `propName :: T1 \| T2` parses to `ValueTypeName::Union`. Members can be scalar, structural, temporal, `ANY` or `NOTHING`. Tests: `tests/union_value_types.rs`, `tests/graph_type_bodies.rs`. |
| GV67 | Closed dynamic unions | Yes | The same union forms, on predicates and in type bodies; one `Union` AST node covers open and closed unions (the two aren't told apart; see the limitations). Tests: `tests/union_value_types.rs`. |
| GV70 | `null` type support | Yes | `Expression::NullLit`, `CypherValue::Null`, three-valued logic in the executor. |
| GV71 | `NOTHING` empty type support | Yes | `IS NOTHING` / `IS NOT NOTHING` (`TypeKind::Nothing`) lower to `IS NULL` / its negation: null is the empty type's only value. Tests: `tests/use_refs_and_value_types.rs`. |

**Optional score: 60 yes / 0 partial / 0 no / 0 deferred** (of 60 rows).

---

## Summary

| Bucket | Yes | Partial | No | Deferred | Total |
|---|---|---|---|---|---|
| Mandatory (section 24.2) | 40 | 0 | 0 | 0 | 40 |
| Optional | 60 | 0 | 0 | 0 | 60 |
| **Total** | **100** | **0** | **0** | **0** | **100** |

## Known limitations

* **`USE`** (GQ01) picks one graph per request: a script can't switch
  graphs between statements (`25G04`).
* **Graph types and schemas**: `CREATE GRAPH TYPE`, typed `CREATE GRAPH`
  (inline type, `LIKE`, `AS COPY OF`) and `CREATE` / `DROP SCHEMA` parse,
  and the TCK harness validates them, but the server can't create them:
  every database is an open-typed property graph.
* **Transaction blocks** (GD01) live within one call; a block ending in
  `ROLLBACK` can't write. Use an explicit transaction to span calls or
  discard writes. The WebAssembly build runs each call as a transaction
  of its own, so a block there commits or rolls back as a whole.
* **Error status codes** name the condition for the errors the engine can
  identify (variables, functions, procedures, parameters, aggregation,
  arithmetic, temporal formats, constraints, transaction states, limits);
  others carry their class code (`42000`, `22G03`, `50N42`).
* **`WHEN`** branches are single statements, and the condition can't use
  bindings (section 15.4's compile-time intent).
* **Quantified path patterns** (16.11): unbounded multi-step forms stop
  at 10 iterations (state `{m,n}` for deeper paths); zero-iteration arms
  (`{0,n}`, `?`, `*`) leave group variables unbound instead of binding
  empty lists; constraints on a multi-step group's inner start node and
  an inner `WHERE` in such a group are errors; groups don't nest.
* **Path modes** (G003, G010, G013) apply per statement: the first
  non-TRAIL mode found governs every MATCH in it.
* **`SHORTEST k GROUPS`** (G020) enumerates at most 10,000 paths per call.
* **Open vs closed unions** (GV66, GV67) aren't distinguished in the
  type system.
* **The procedural sublanguage** (`IF`, `WHILE`, `BEGIN` ... `END`, local
  variables, procedural `RETURN`) is out of scope (`DESIGN.md` section 9).
