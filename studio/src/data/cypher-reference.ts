/**
 * Traverse Cypher Language Reference
 *
 * Structured data for the Studio's searchable Cypher reference panel.
 * Each entry is categorized and includes syntax, description, and examples
 * so the UI can render a filterable, searchable reference guide.
 */

export interface CypherRefEntry {
  /** Short identifier used for anchoring / deep-linking */
  id: string
  /** Display name */
  name: string
  /** Category for filtering */
  category: CypherCategory
  /** Optional subcategory within the category */
  subcategory?: string
  /** One-line description */
  summary: string
  /** Syntax template (uses placeholders like <expr>, [optional]) */
  syntax: string
  /** Longer explanation (markdown-safe plain text) */
  description: string
  /** Runnable example queries */
  examples: CypherExample[]
  /** Alternative names / aliases (also searched) */
  aliases?: string[]
  /** Related entry IDs */
  related?: string[]
}

export interface CypherExample {
  /** Short label for the example */
  label: string
  /** Cypher query text */
  query: string
}

export type CypherCategory =
  | 'clause'
  | 'function'
  | 'operator'
  | 'expression'
  | 'pattern'
  | 'type'
  | 'schema'
  | 'admin'

export const CATEGORY_LABELS: Record<CypherCategory, string> = {
  clause: 'Clauses',
  function: 'Functions',
  operator: 'Operators',
  expression: 'Expressions',
  pattern: 'Patterns',
  type: 'Types',
  schema: 'Schema & Indexes',
  admin: 'Administration',
}

// ---------------------------------------------------------------------------
// Reference data
// ---------------------------------------------------------------------------

export const cypherReference: CypherRefEntry[] = [
  // -- Reading clauses -----------------------------------------------------
  {
    id: 'match',
    name: 'MATCH',
    category: 'clause',
    subcategory: 'Reading',
    summary: 'Find nodes and relationships matching a pattern.',
    syntax: 'MATCH <pattern> [WHERE <predicate>]',
    description:
      'Searches the graph for subgraphs that match the given pattern. Bind variables to nodes and relationships for use in later clauses. Combine with WHERE for filtering.',
    examples: [
      { label: 'All nodes', query: 'MATCH (n) RETURN n LIMIT 25' },
      { label: 'By label', query: 'MATCH (p:Person) RETURN p.name' },
      {
        label: 'With relationship',
        query: 'MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name',
      },
      {
        label: 'With properties',
        query: "MATCH (n:Person {name: 'Alice'}) RETURN n",
      },
    ],
    related: ['optional-match', 'where', 'return'],
  },
  {
    id: 'optional-match',
    name: 'OPTIONAL MATCH',
    category: 'clause',
    subcategory: 'Reading',
    summary: 'Match a pattern, returning NULL for missing parts.',
    syntax: 'OPTIONAL MATCH <pattern>',
    description:
      'Like MATCH, but if the pattern has no match the bound variables are set to NULL rather than removing the row. Equivalent to a left outer join.',
    examples: [
      {
        label: 'Optional relationship',
        query:
          "MATCH (p:Person {name: 'Alice'})\nOPTIONAL MATCH (p)-[:LIVES_IN]->(c:City)\nRETURN p.name, c.name",
      },
    ],
    related: ['match'],
  },
  {
    id: 'where',
    name: 'WHERE',
    category: 'clause',
    subcategory: 'Reading',
    summary: 'Filter results with a predicate.',
    syntax: 'WHERE <predicate>',
    description:
      'Constrains the results of a MATCH, OPTIONAL MATCH, or WITH clause. Supports comparison, boolean logic, string predicates, NULL checks, list membership, and pattern predicates.',
    examples: [
      {
        label: 'Comparison',
        query: 'MATCH (n:Person) WHERE n.age > 30 RETURN n.name',
      },
      {
        label: 'String predicate',
        query:
          "MATCH (n:Person) WHERE n.name STARTS WITH 'A' RETURN n.name",
      },
      {
        label: 'Pattern predicate',
        query:
          'MATCH (n:Person) WHERE (n)-[:KNOWS]->(:Person) RETURN n.name',
      },
      {
        label: 'Regex',
        query: "MATCH (n:Person) WHERE n.name =~ 'Al.*' RETURN n.name",
      },
    ],
    related: ['match', 'with'],
  },

  // -- Writing clauses -----------------------------------------------------
  {
    id: 'create',
    name: 'CREATE',
    category: 'clause',
    subcategory: 'Writing',
    summary: 'Create nodes and relationships.',
    syntax: 'CREATE <pattern>',
    description:
      'Creates new nodes and relationships. Does not check for duplicates - use MERGE for match-or-create semantics.',
    examples: [
      {
        label: 'Create node',
        query: "CREATE (n:Person {name: 'Alice', age: 30})",
      },
      {
        label: 'Create relationship',
        query:
          "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})\nCREATE (a)-[:KNOWS {since: 2020}]->(b)",
      },
      {
        label: 'Create and return',
        query: "CREATE (n:Person {name: 'Eve'}) RETURN n",
      },
    ],
    related: ['merge', 'delete'],
  },
  {
    id: 'merge',
    name: 'MERGE',
    category: 'clause',
    subcategory: 'Writing',
    summary: 'Match or create a pattern atomically.',
    syntax:
      'MERGE <pattern> [ON CREATE SET ...] [ON MATCH SET ...]',
    description:
      'Tries to MATCH the pattern first. If no match is found, CREATEs it. ON CREATE SET and ON MATCH SET allow setting different properties depending on whether the pattern was matched or created.',
    examples: [
      {
        label: 'Merge node',
        query: "MERGE (n:Person {name: 'Alice'})",
      },
      {
        label: 'With ON CREATE',
        query:
          "MERGE (n:Person {name: 'Alice'})\nON CREATE SET n.created = datetime()\nON MATCH SET n.lastSeen = datetime()",
      },
      {
        label: 'Merge relationship',
        query:
          "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})\nMERGE (a)-[:KNOWS]->(b)",
      },
    ],
    related: ['create', 'set'],
  },
  {
    id: 'set',
    name: 'SET',
    category: 'clause',
    subcategory: 'Writing',
    summary: 'Update properties or labels on nodes and relationships.',
    syntax: 'SET <assignment> [, <assignment>]*',
    description:
      'Modifies properties or adds labels. Supports single property assignment (n.prop = val), replacing all properties (n = {...}), merging properties (n += {...}), and adding labels (n:Label).',
    examples: [
      {
        label: 'Set property',
        query:
          "MATCH (n:Person {name: 'Alice'}) SET n.age = 31",
      },
      {
        label: 'Merge properties',
        query:
          "MATCH (n:Person {name: 'Alice'}) SET n += {city: 'Stockholm', active: true}",
      },
      {
        label: 'Add label',
        query: "MATCH (n:Person {name: 'Alice'}) SET n:Employee",
      },
    ],
    related: ['remove', 'merge'],
  },
  {
    id: 'remove',
    name: 'REMOVE',
    category: 'clause',
    subcategory: 'Writing',
    summary: 'Remove properties or labels.',
    syntax: 'REMOVE <property_or_label> [, ...]',
    description:
      'Removes a property from a node or relationship, or removes a label from a node.',
    examples: [
      {
        label: 'Remove property',
        query: "MATCH (n:Person {name: 'Alice'}) REMOVE n.age",
      },
      {
        label: 'Remove label',
        query: "MATCH (n:Person {name: 'Alice'}) REMOVE n:Employee",
      },
    ],
    related: ['set'],
  },
  {
    id: 'delete',
    name: 'DELETE',
    category: 'clause',
    subcategory: 'Writing',
    summary: 'Delete nodes and relationships.',
    syntax: 'DELETE <variable> [, ...]',
    description:
      'Deletes nodes or relationships. Nodes can only be deleted if they have no remaining relationships - use DETACH DELETE to remove a node and all its relationships.',
    examples: [
      {
        label: 'Delete relationship',
        query:
          "MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b)\nDELETE r",
      },
      {
        label: 'Detach delete node',
        query: "MATCH (n:Person {name: 'Alice'}) DETACH DELETE n",
      },
    ],
    related: ['create', 'match'],
  },

  // -- Control flow clauses ------------------------------------------------
  {
    id: 'return',
    name: 'RETURN',
    category: 'clause',
    subcategory: 'Projection',
    summary: 'Define the output columns of a query.',
    syntax: 'RETURN <expr> [AS <alias>] [, ...] [ORDER BY ...] [SKIP n] [LIMIT n]',
    description:
      'Projects the final result set. Supports aliasing with AS, aggregation, DISTINCT, and ordering/pagination modifiers.',
    examples: [
      { label: 'Simple', query: 'MATCH (n:Person) RETURN n.name, n.age' },
      {
        label: 'With alias',
        query: 'MATCH (n:Person) RETURN n.name AS name, n.age AS age',
      },
      {
        label: 'All variables',
        query: 'MATCH (a)-[r]->(b) RETURN *',
      },
      {
        label: 'Distinct',
        query: 'MATCH (n:Person)-[:KNOWS]->(m) RETURN DISTINCT m.city',
      },
    ],
    related: ['with', 'order-by', 'limit', 'skip'],
  },
  {
    id: 'with',
    name: 'WITH',
    category: 'clause',
    subcategory: 'Projection',
    summary: 'Pipe results between query parts.',
    syntax: 'WITH <expr> [AS <alias>] [, ...] [WHERE <predicate>]',
    description:
      'Acts as a projection barrier - only variables listed in WITH are visible to subsequent clauses. Supports aggregation, DISTINCT, ORDER BY, SKIP, LIMIT, and an inline WHERE for post-projection filtering.',
    examples: [
      {
        label: 'Aggregation pipeline',
        query:
          'MATCH (n:Person)-[:KNOWS]->(m)\nWITH n, count(m) AS friends\nWHERE friends > 5\nRETURN n.name, friends',
      },
      {
        label: 'Top-N pattern',
        query:
          'MATCH (n:Person)\nWITH n ORDER BY n.age DESC LIMIT 10\nRETURN n.name, n.age',
      },
    ],
    related: ['return', 'match'],
  },
  {
    id: 'order-by',
    name: 'ORDER BY',
    category: 'clause',
    subcategory: 'Projection',
    summary: 'Sort results.',
    syntax: 'ORDER BY <expr> [ASC | DESC] [, ...]',
    description:
      'Sorts the output of RETURN or WITH. Defaults to ascending. Multiple sort keys are applied left-to-right.',
    examples: [
      {
        label: 'Ascending',
        query: 'MATCH (n:Person) RETURN n.name ORDER BY n.name',
      },
      {
        label: 'Descending',
        query: 'MATCH (n:Person) RETURN n.name, n.age ORDER BY n.age DESC',
      },
      {
        label: 'Multiple keys',
        query:
          'MATCH (n:Person) RETURN n.city, n.name ORDER BY n.city ASC, n.name ASC',
      },
    ],
    related: ['return', 'with', 'limit', 'skip'],
  },
  {
    id: 'limit',
    name: 'LIMIT',
    category: 'clause',
    subcategory: 'Projection',
    summary: 'Restrict the number of returned rows.',
    syntax: 'LIMIT <count>',
    description: 'Returns at most the specified number of rows.',
    examples: [
      { label: 'First 10', query: 'MATCH (n) RETURN n LIMIT 10' },
    ],
    related: ['skip', 'order-by'],
  },
  {
    id: 'skip',
    name: 'SKIP',
    category: 'clause',
    subcategory: 'Projection',
    summary: 'Skip a number of rows.',
    syntax: 'SKIP <count>',
    description: 'Skips the first N rows from the result. Typically used with ORDER BY for pagination.',
    examples: [
      {
        label: 'Pagination',
        query: 'MATCH (n:Person) RETURN n.name ORDER BY n.name SKIP 20 LIMIT 10',
      },
    ],
    related: ['limit', 'order-by'],
  },
  {
    id: 'unwind',
    name: 'UNWIND',
    category: 'clause',
    subcategory: 'Reading',
    summary: 'Expand a list into individual rows.',
    syntax: 'UNWIND <list> AS <variable>',
    description:
      'Takes a list expression and produces one row per element, binding each element to the given variable.',
    examples: [
      {
        label: 'Literal list',
        query: "UNWIND ['Alice', 'Bob', 'Charlie'] AS name RETURN name",
      },
      {
        label: 'Batch create',
        query:
          "UNWIND [{name: 'A'}, {name: 'B'}] AS props\nCREATE (n:Person) SET n = props",
      },
    ],
    related: ['with'],
  },
  {
    id: 'union',
    name: 'UNION',
    category: 'clause',
    subcategory: 'Reading',
    summary: 'Combine results from multiple queries.',
    syntax: '<query1> UNION [ALL] <query2>',
    description:
      'Combines the results of two or more queries. UNION removes duplicates; UNION ALL keeps all rows. All queries must return the same column names.',
    examples: [
      {
        label: 'Union with dedup',
        query:
          "MATCH (n:Person) RETURN n.name AS name\nUNION\nMATCH (n:Company) RETURN n.name AS name",
      },
      {
        label: 'Union all',
        query:
          "MATCH (n:Person) RETURN n.name AS name\nUNION ALL\nMATCH (n:Company) RETURN n.name AS name",
      },
    ],
  },
  {
    id: 'call',
    name: 'CALL',
    category: 'clause',
    subcategory: 'Procedures',
    summary: 'Invoke a stored procedure.',
    syntax: 'CALL <procedure>(<args>) [YIELD <columns>]',
    description:
      'Calls a built-in or user-defined procedure. Use YIELD to select which output columns to bind. YIELD * binds all output columns.',
    examples: [
      {
        label: 'List procedures',
        query: 'CALL dbms.procedures() YIELD name, signature',
      },
      {
        label: 'Schema info',
        query: 'CALL db.labels() YIELD label RETURN label',
      },
    ],
  },
  {
    id: 'load-csv',
    name: 'LOAD CSV',
    category: 'clause',
    subcategory: 'Reading',
    summary: 'Read rows from a CSV file.',
    syntax: "LOAD CSV [WITH HEADERS] FROM '<path>' AS <row> [FIELDTERMINATOR '<char>']",
    description:
      'Reads a CSV file and makes each row available as a list (or map if WITH HEADERS is used). Useful for bulk data import.',
    examples: [
      {
        label: 'With headers',
        query:
          "LOAD CSV WITH HEADERS FROM 'file:///import/people.csv' AS row\nCREATE (:Person {name: row.name, age: toInteger(row.age)})",
      },
      {
        label: 'Custom delimiter',
        query:
          "LOAD CSV FROM 'file:///data.tsv' AS row FIELDTERMINATOR '\\t'\nRETURN row",
      },
    ],
  },
  {
    id: 'explain',
    name: 'EXPLAIN',
    category: 'clause',
    subcategory: 'Query Analysis',
    summary: 'Show the query execution plan without running it.',
    syntax: 'EXPLAIN <query>',
    description:
      'Returns the planned execution steps without actually executing the query. Useful for understanding how the planner will process a query.',
    examples: [
      {
        label: 'Explain match',
        query: 'EXPLAIN MATCH (n:Person)-[:KNOWS]->(m) RETURN n, m',
      },
    ],
    related: ['profile'],
  },
  {
    id: 'profile',
    name: 'PROFILE',
    category: 'clause',
    subcategory: 'Query Analysis',
    summary: 'Execute the query and return the plan with timing.',
    syntax: 'PROFILE <query>',
    description:
      'Executes the query and returns the execution plan annotated with actual row counts and timing data. Useful for performance analysis.',
    examples: [
      {
        label: 'Profile match',
        query: 'PROFILE MATCH (n:Person)-[:KNOWS]->(m) RETURN count(m)',
      },
    ],
    related: ['explain'],
  },

  // -- Operators -----------------------------------------------------------
  {
    id: 'op-comparison',
    name: 'Comparison',
    category: 'operator',
    subcategory: 'Comparison',
    summary: 'Compare values: =, <>, <, <=, >, >=',
    syntax: '<expr> = | <> | < | <= | > | >= <expr>',
    description:
      'Standard comparison operators. Supports chaining: a < b <= c. Returns NULL if either operand is NULL.',
    examples: [
      { label: 'Equality', query: 'MATCH (n) WHERE n.age = 30 RETURN n' },
      { label: 'Range', query: 'MATCH (n) WHERE 18 <= n.age < 65 RETURN n' },
    ],
  },
  {
    id: 'op-boolean',
    name: 'Boolean',
    category: 'operator',
    subcategory: 'Logical',
    summary: 'Combine predicates: AND, OR, XOR, NOT',
    syntax: '<pred> AND | OR | XOR <pred>  /  NOT <pred>',
    description:
      'Boolean logic operators for combining predicates. Uses three-valued logic (TRUE, FALSE, NULL).',
    examples: [
      {
        label: 'AND / OR',
        query: 'MATCH (n) WHERE n.age > 18 AND (n.active = true OR n.verified = true) RETURN n',
      },
    ],
  },
  {
    id: 'op-arithmetic',
    name: 'Arithmetic',
    category: 'operator',
    subcategory: 'Math',
    summary: 'Numeric operations: +, -, *, /, %, ^',
    syntax: '<expr> + | - | * | / | % | ^ <expr>',
    description:
      'Standard arithmetic. + also works for string concatenation and list concatenation. ^ is exponentiation.',
    examples: [
      { label: 'Calculation', query: 'RETURN 2 ^ 10 AS result' },
      {
        label: 'String concat',
        query: "MATCH (n:Person) RETURN n.firstName + ' ' + n.lastName AS fullName",
      },
    ],
  },
  {
    id: 'op-string',
    name: 'String predicates',
    category: 'operator',
    subcategory: 'String',
    summary: 'STARTS WITH, ENDS WITH, CONTAINS, =~ (regex)',
    syntax: "<expr> STARTS WITH | ENDS WITH | CONTAINS <string>\n<expr> =~ '<regex>'",
    description:
      'String matching predicates. STARTS WITH, ENDS WITH, and CONTAINS are case-sensitive. =~ performs a full regex match.',
    examples: [
      {
        label: 'Starts with',
        query: "MATCH (n:Person) WHERE n.name STARTS WITH 'Al' RETURN n",
      },
      {
        label: 'Regex',
        query: "MATCH (n) WHERE n.email =~ '.*@example\\.com' RETURN n",
      },
    ],
  },
  {
    id: 'op-null',
    name: 'NULL checks',
    category: 'operator',
    subcategory: 'Null',
    summary: 'IS NULL, IS NOT NULL',
    syntax: '<expr> IS [NOT] NULL',
    description: 'Tests whether a value is NULL or not. Properties that do not exist evaluate to NULL.',
    examples: [
      {
        label: 'Filter missing',
        query: 'MATCH (n:Person) WHERE n.email IS NOT NULL RETURN n',
      },
    ],
  },
  {
    id: 'op-in',
    name: 'IN',
    category: 'operator',
    subcategory: 'Collection',
    summary: 'Test list membership.',
    syntax: '<expr> IN <list>',
    description: 'Returns true if the left-hand value is found in the right-hand list.',
    examples: [
      {
        label: 'List membership',
        query: "MATCH (n:Person) WHERE n.city IN ['Stockholm', 'Berlin'] RETURN n",
      },
    ],
  },

  // -- Expressions ---------------------------------------------------------
  {
    id: 'expr-case',
    name: 'CASE',
    category: 'expression',
    summary: 'Conditional expression (if/else logic).',
    syntax:
      'CASE [<expr>] WHEN <val> THEN <result> [...] [ELSE <default>] END',
    description:
      'Two forms: simple CASE (compare one expression against values) and generic CASE (independent boolean predicates per WHEN). Returns the THEN value of the first matching WHEN, or ELSE if none match.',
    examples: [
      {
        label: 'Simple case',
        query:
          "MATCH (n:Person)\nRETURN n.name,\n  CASE n.status\n    WHEN 'A' THEN 'Active'\n    WHEN 'I' THEN 'Inactive'\n    ELSE 'Unknown'\n  END AS status",
      },
      {
        label: 'Generic case',
        query:
          "MATCH (n:Person)\nRETURN n.name,\n  CASE\n    WHEN n.age < 18 THEN 'Minor'\n    WHEN n.age < 65 THEN 'Adult'\n    ELSE 'Senior'\n  END AS category",
      },
    ],
  },
  {
    id: 'expr-list-comprehension',
    name: 'List comprehension',
    category: 'expression',
    summary: 'Transform and filter lists inline.',
    syntax: '[<var> IN <list> [WHERE <pred>] [| <expr>]]',
    description:
      'Iterates over a list, optionally filtering with WHERE and transforming with a mapping expression. Returns a new list.',
    examples: [
      {
        label: 'Filter and map',
        query: 'RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x] AS squares',
      },
    ],
  },
  {
    id: 'expr-pattern-comprehension',
    name: 'Pattern comprehension',
    category: 'expression',
    summary: 'Collect results from pattern matching inline.',
    syntax: '[<pattern> [WHERE <pred>] | <expr>]',
    description:
      'Matches a pattern and collects the mapped expression into a list. Works like a subquery that returns a list.',
    examples: [
      {
        label: 'Collect friend names',
        query:
          'MATCH (p:Person)\nRETURN p.name, [(p)-[:KNOWS]->(f) | f.name] AS friends',
      },
    ],
  },
  {
    id: 'expr-exists',
    name: 'EXISTS subquery',
    category: 'expression',
    summary: 'Test whether a subquery returns any results.',
    syntax: 'EXISTS { <subquery> }',
    description:
      'Returns true if the enclosed subquery matches at least one row. Can contain full MATCH/WHERE/WITH patterns.',
    examples: [
      {
        label: 'Existential check',
        query:
          'MATCH (n:Person)\nWHERE EXISTS { MATCH (n)-[:KNOWS]->(:Person {city: n.city}) }\nRETURN n.name',
      },
    ],
    related: ['expr-count'],
  },
  {
    id: 'expr-count',
    name: 'COUNT subquery',
    category: 'expression',
    summary: 'Count matches of a subquery.',
    syntax: 'COUNT { <subquery> }',
    description:
      'Returns the number of rows matched by the enclosed subquery.',
    examples: [
      {
        label: 'Count subquery',
        query:
          'MATCH (n:Person)\nRETURN n.name, COUNT { (n)-[:KNOWS]->() } AS friendCount',
      },
    ],
    related: ['expr-exists'],
  },
  {
    id: 'expr-quantifiers',
    name: 'Quantifier predicates',
    category: 'expression',
    summary: 'ALL, ANY, NONE, SINGLE - test list elements.',
    syntax:
      'ALL | ANY | NONE | SINGLE (<var> IN <list> WHERE <pred>)',
    description:
      'List quantifier predicates. ALL: true if predicate holds for every element. ANY: true if at least one matches. NONE: true if no elements match. SINGLE: true if exactly one matches.',
    examples: [
      {
        label: 'All positive',
        query: 'WITH [1, 2, 3] AS nums RETURN ALL(x IN nums WHERE x > 0) AS allPositive',
      },
      {
        label: 'Any match',
        query:
          "MATCH p = (a)-[*]->(b)\nWHERE ANY(r IN relationships(p) WHERE r.weight > 0.5)\nRETURN p",
      },
    ],
  },
  {
    id: 'expr-list-index',
    name: 'List indexing & slicing',
    category: 'expression',
    summary: 'Access elements or sub-ranges of a list.',
    syntax: '<list>[<index>]  /  <list>[<start>..<end>]',
    description:
      'Access individual elements by zero-based index, or extract sub-lists using range syntax. Negative indexes count from the end.',
    examples: [
      {
        label: 'Index and slice',
        query: "WITH ['a','b','c','d','e'] AS l RETURN l[0], l[-1], l[1..3]",
      },
    ],
  },
  {
    id: 'expr-map-literal',
    name: 'Map literal',
    category: 'expression',
    summary: 'Construct a map (key-value pairs).',
    syntax: '{<key>: <value>, ...}',
    description: 'Creates an inline map. Keys are strings, values can be any expression.',
    examples: [
      {
        label: 'Map literal',
        query: "RETURN {name: 'Alice', age: 30, active: true} AS person",
      },
    ],
  },
  {
    id: 'expr-reduce',
    name: 'reduce()',
    category: 'expression',
    summary: 'Fold a list into a single value with an accumulator.',
    syntax: 'reduce(<acc> = <initial>, <var> IN <list> | <expr>)',
    description:
      'Iterates over a list, threading an accumulator through each step. The expression can reference both the current element and the accumulator. Returns the final accumulator value.',
    examples: [
      {
        label: 'Sum a list',
        query: 'RETURN reduce(total = 0, x IN [1, 2, 3, 4] | total + x) AS sum',
      },
      {
        label: 'Concatenate strings',
        query:
          "RETURN reduce(s = '', name IN ['Alice', 'Bob', 'Eve'] | s + name + ' ') AS names",
      },
      {
        label: 'Sum path weights',
        query:
          "MATCH p = (a:City {name: 'Stockholm'})-[:ROAD*]->(b:City {name: 'Berlin'})\nRETURN reduce(dist = 0, r IN relationships(p) | dist + r.distance) AS totalDist",
      },
    ],
    related: ['expr-list-comprehension', 'func-list-head', 'func-list-tail'],
  },
  {
    id: 'expr-map-projection',
    name: 'Map projection',
    category: 'expression',
    summary: 'Project selected properties from a node, relationship, or map.',
    syntax: '<expr> {.<prop1>, .<prop2>, ...}',
    description:
      'Creates a map containing only the listed properties from the source value. Useful for returning a subset of properties without listing each one explicitly.',
    examples: [
      {
        label: 'Project node properties',
        query: 'MATCH (p:Person) RETURN p {.name, .age}',
      },
      {
        label: 'Project with alias',
        query:
          "MATCH (p:Person)-[:LIVES_IN]->(c:City)\nRETURN p {.name, .age, city: c.name}",
      },
    ],
    related: ['expr-map-literal', 'func-scalar-properties'],
  },
  {
    id: 'expr-parameters',
    name: 'Parameters',
    category: 'expression',
    summary: 'Use query parameters with $name.',
    syntax: '$<parameterName>',
    description:
      'References a parameter passed to the query. Preferred over string interpolation for security and performance (plan caching).',
    examples: [
      {
        label: 'Parameter usage',
        query: 'MATCH (n:Person {name: $name}) RETURN n',
      },
    ],
  },

  // -- Patterns ------------------------------------------------------------
  {
    id: 'pattern-node',
    name: 'Node pattern',
    category: 'pattern',
    summary: 'Match a node by label and/or properties.',
    syntax: '(<var>[:<Label>] [{<props>}])',
    description:
      'Matches a single node. Optionally filtered by one or more labels and/or property values.',
    examples: [
      { label: 'Any node', query: 'MATCH (n) RETURN n LIMIT 5' },
      { label: 'With label', query: 'MATCH (p:Person) RETURN p' },
      {
        label: 'Multiple labels',
        query: 'MATCH (n:Person:Employee) RETURN n',
      },
      {
        label: 'With properties',
        query: "MATCH (n:Person {name: 'Alice', age: 30}) RETURN n",
      },
    ],
    related: ['pattern-rel'],
  },
  {
    id: 'pattern-rel',
    name: 'Relationship pattern',
    category: 'pattern',
    summary: 'Match a relationship by type and direction.',
    syntax:
      '-[<var>[:<TYPE>] [{<props>}]]->\n<-[<var>[:<TYPE>]]-\n-[<var>[:<TYPE>]]-',
    description:
      'Matches a relationship between two nodes. Specify direction with arrow heads, type with :TYPE, and optional properties. Use | for multiple types.',
    examples: [
      {
        label: 'Outgoing typed',
        query: 'MATCH (a)-[r:KNOWS]->(b) RETURN a, r, b',
      },
      {
        label: 'Undirected',
        query: 'MATCH (a)-[:KNOWS]-(b) RETURN a, b',
      },
      {
        label: 'Multiple types',
        query: 'MATCH (a)-[r:KNOWS|LIKES]->(b) RETURN type(r), b',
      },
      {
        label: 'With properties',
        query: 'MATCH (a)-[r:KNOWS {since: 2020}]->(b) RETURN a, b',
      },
    ],
    related: ['pattern-node', 'pattern-varlength'],
  },
  {
    id: 'pattern-varlength',
    name: 'Variable-length path',
    category: 'pattern',
    summary: 'Match paths of varying length with *min..max.',
    syntax: '-[<var>[:<TYPE>]*[<min>..<max>]]->',
    description:
      'Matches paths that traverse a variable number of relationships. Specify minimum and maximum hops. Omitting bounds means unbounded.',
    examples: [
      {
        label: '1 to 3 hops',
        query: 'MATCH (a)-[:KNOWS*1..3]->(b) RETURN a, b',
      },
      {
        label: 'Exactly 2 hops',
        query: 'MATCH (a)-[:KNOWS*2]->(b) RETURN a, b',
      },
      {
        label: 'Up to 5 hops',
        query: 'MATCH (a)-[:KNOWS*..5]->(b) RETURN a, b',
      },
      {
        label: 'Any length',
        query: 'MATCH (a)-[:KNOWS*]->(b) RETURN a, b',
      },
    ],
    related: ['pattern-rel', 'pattern-shortest-path'],
  },
  {
    id: 'pattern-shortest-path',
    name: 'Shortest path',
    category: 'pattern',
    summary: 'Find shortest paths between nodes.',
    syntax:
      'shortestPath((<a>)-[*..N]->(<b>))\nallShortestPaths((<a>)-[*..N]->(<b>))',
    description:
      'shortestPath returns one shortest path. allShortestPaths returns all paths tied for shortest. Both require a maximum depth bound.',
    examples: [
      {
        label: 'Single shortest',
        query:
          "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})\nMATCH p = shortestPath((a)-[*..15]->(b))\nRETURN p",
      },
      {
        label: 'All shortest',
        query:
          "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})\nMATCH p = allShortestPaths((a)-[*..10]->(b))\nRETURN p",
      },
    ],
    related: ['pattern-weighted-shortest', 'pattern-varlength'],
  },
  {
    id: 'pattern-weighted-shortest',
    name: 'Weighted shortest path',
    category: 'pattern',
    summary: 'Find the shortest path by edge weight (Dijkstra).',
    syntax: '(a)-[r *wShortest (r, node | r.cost) totalCost]->(b)',
    description:
      'Finds the path with the lowest total weight using Dijkstra\'s algorithm. The cost expression is evaluated per edge, and the total is bound to a variable.',
    examples: [
      {
        label: 'Weighted shortest',
        query:
          "MATCH (a:City {name: 'Stockholm'}), (b:City {name: 'Berlin'})\nMATCH p = (a)-[r *wShortest (r, n | r.distance) totalDist]->(b)\nRETURN p, totalDist",
      },
    ],
    related: ['pattern-shortest-path'],
  },
  {
    id: 'pattern-path-variable',
    name: 'Path variable',
    category: 'pattern',
    summary: 'Bind an entire path to a variable.',
    syntax: 'p = <pattern>',
    description:
      'Assigns the full path (sequence of alternating nodes and relationships) to a variable. Use nodes(p) and relationships(p) to decompose.',
    examples: [
      {
        label: 'Bind path',
        query:
          'MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person)\nRETURN nodes(p), relationships(p)',
      },
    ],
    related: ['fn-nodes', 'fn-relationships'],
  },

  // -- Aggregation functions -----------------------------------------------
  {
    id: 'fn-count',
    name: 'count()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Count rows or non-null values.',
    syntax: 'count(*) | count(<expr>) | count(DISTINCT <expr>)',
    description:
      'count(*) counts all rows. count(expr) counts non-null values. DISTINCT eliminates duplicates before counting.',
    examples: [
      { label: 'Count all', query: 'MATCH (n:Person) RETURN count(*)' },
      {
        label: 'Count distinct',
        query: 'MATCH (n:Person) RETURN count(DISTINCT n.city)',
      },
    ],
  },
  {
    id: 'fn-sum',
    name: 'sum()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Sum numeric values.',
    syntax: 'sum(<numeric_expr>)',
    description: 'Returns the sum of all non-null numeric values.',
    examples: [
      {
        label: 'Total age',
        query: 'MATCH (n:Person) RETURN sum(n.age) AS totalAge',
      },
    ],
  },
  {
    id: 'fn-avg',
    name: 'avg()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Average numeric values.',
    syntax: 'avg(<numeric_expr>)',
    description: 'Returns the arithmetic mean of all non-null numeric values.',
    examples: [
      {
        label: 'Average age',
        query: 'MATCH (n:Person) RETURN avg(n.age) AS avgAge',
      },
    ],
  },
  {
    id: 'fn-min',
    name: 'min()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Minimum value.',
    syntax: 'min(<expr>)',
    description: 'Returns the smallest value among all non-null values. Works with numbers, strings, and other comparable types.',
    examples: [
      {
        label: 'Youngest',
        query: 'MATCH (n:Person) RETURN min(n.age) AS youngest',
      },
    ],
  },
  {
    id: 'fn-max',
    name: 'max()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Maximum value.',
    syntax: 'max(<expr>)',
    description: 'Returns the largest value among all non-null values.',
    examples: [
      {
        label: 'Oldest',
        query: 'MATCH (n:Person) RETURN max(n.age) AS oldest',
      },
    ],
  },
  {
    id: 'fn-collect',
    name: 'collect()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Collect values into a list.',
    syntax: 'collect(<expr>) | collect(DISTINCT <expr>)',
    description: 'Aggregates non-null values into a list. Order is not guaranteed.',
    examples: [
      {
        label: 'Collect names',
        query:
          'MATCH (p:Person)-[:LIVES_IN]->(c:City)\nRETURN c.name, collect(p.name) AS residents',
      },
    ],
  },
  {
    id: 'fn-percentile',
    name: 'percentileDisc() / percentileCont()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Calculate percentiles (discrete or continuous).',
    syntax: 'percentileDisc(<expr>, <percentile>)\npercentileCont(<expr>, <percentile>)',
    description:
      'percentileDisc returns the nearest value at the given percentile (0.0-1.0). percentileCont interpolates between values.',
    examples: [
      {
        label: 'Median',
        query: 'MATCH (n:Person) RETURN percentileCont(n.age, 0.5) AS median',
      },
      {
        label: 'P95',
        query: 'MATCH (n:Person) RETURN percentileDisc(n.age, 0.95) AS p95',
      },
    ],
  },
  {
    id: 'fn-stdev',
    name: 'stDev() / stDevP()',
    category: 'function',
    subcategory: 'Aggregation',
    summary: 'Standard deviation (sample or population).',
    syntax: 'stDev(<numeric_expr>)\nstDevP(<numeric_expr>)',
    description:
      'stDev computes the sample standard deviation (n-1). stDevP computes the population standard deviation (n).',
    examples: [
      {
        label: 'Sample stdev',
        query: 'MATCH (n:Person) RETURN stDev(n.age) AS sd',
      },
    ],
  },

  // -- Scalar functions ----------------------------------------------------
  {
    id: 'fn-id',
    name: 'id()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Get the internal ID of a node or relationship.',
    syntax: 'id(<node_or_rel>)',
    description: 'Returns the internal integer ID. IDs are stable within a session but may be reused after deletion.',
    examples: [
      { label: 'Node ID', query: 'MATCH (n) RETURN id(n), n LIMIT 5' },
    ],
    related: ['fn-type', 'fn-labels'],
  },
  {
    id: 'fn-type',
    name: 'type()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Get the type name of a relationship.',
    syntax: 'type(<relationship>)',
    description: 'Returns the type string of a relationship.',
    examples: [
      {
        label: 'Relationship type',
        query: 'MATCH ()-[r]->() RETURN DISTINCT type(r)',
      },
    ],
    related: ['fn-labels'],
  },
  {
    id: 'fn-labels',
    name: 'labels()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Get the labels of a node as a list.',
    syntax: 'labels(<node>)',
    description: 'Returns a list of label strings attached to the node.',
    examples: [
      {
        label: 'Node labels',
        query: 'MATCH (n) RETURN DISTINCT labels(n)',
      },
    ],
    related: ['fn-type'],
  },
  {
    id: 'fn-keys',
    name: 'keys()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Get property keys of a node, relationship, or map.',
    syntax: 'keys(<node_or_rel_or_map>)',
    description: 'Returns a list of property key strings.',
    examples: [
      {
        label: 'Property keys',
        query: 'MATCH (n:Person) RETURN keys(n) LIMIT 1',
      },
    ],
    related: ['fn-properties'],
  },
  {
    id: 'fn-properties',
    name: 'properties()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Get all properties as a map.',
    syntax: 'properties(<node_or_rel_or_map>)',
    description: 'Returns all properties of a node or relationship as a map.',
    examples: [
      {
        label: 'As map',
        query: 'MATCH (n:Person) RETURN properties(n) LIMIT 5',
      },
    ],
    related: ['fn-keys'],
  },
  {
    id: 'fn-coalesce',
    name: 'coalesce()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Return the first non-null value.',
    syntax: 'coalesce(<val1>, <val2>, ...)',
    description: 'Evaluates arguments left to right and returns the first non-null value.',
    examples: [
      {
        label: 'Default value',
        query:
          "MATCH (n:Person) RETURN coalesce(n.nickname, n.name, 'Unknown') AS displayName",
      },
    ],
  },
  {
    id: 'fn-exists',
    name: 'exists()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Test if a property exists (is not null).',
    syntax: 'exists(<expr>)',
    description: 'Returns true if the expression evaluates to a non-null value.',
    examples: [
      {
        label: 'Property exists',
        query: 'MATCH (n:Person) WHERE exists(n.email) RETURN n',
      },
    ],
  },
  {
    id: 'fn-typename',
    name: 'valueType() / typeName()',
    category: 'function',
    subcategory: 'Scalar',
    summary: 'Get the type name of a value.',
    syntax: 'valueType(<expr>)',
    description: 'Returns a string describing the type of the value (e.g., "INTEGER", "STRING", "NODE").',
    aliases: ['typeName', 'valueType'],
    examples: [
      {
        label: 'Type check',
        query: "RETURN valueType(42), valueType('hello'), valueType(null)",
      },
    ],
  },

  // -- Type conversion -----------------------------------------------------
  {
    id: 'fn-tointeger',
    name: 'toInteger()',
    category: 'function',
    subcategory: 'Conversion',
    summary: 'Convert a value to an integer.',
    syntax: 'toInteger(<expr>)',
    description: 'Converts a string, float, or boolean to an integer. Returns NULL if conversion fails.',
    aliases: ['toInt'],
    examples: [
      {
        label: 'String to int',
        query: "RETURN toInteger('42') AS num",
      },
    ],
    related: ['fn-tofloat', 'fn-tostring', 'fn-toboolean'],
  },
  {
    id: 'fn-tofloat',
    name: 'toFloat()',
    category: 'function',
    subcategory: 'Conversion',
    summary: 'Convert a value to a float.',
    syntax: 'toFloat(<expr>)',
    description: 'Converts a string or integer to a 64-bit float. Returns NULL if conversion fails.',
    examples: [
      {
        label: 'String to float',
        query: "RETURN toFloat('3.14') AS num",
      },
    ],
    related: ['fn-tointeger'],
  },
  {
    id: 'fn-tostring',
    name: 'toString()',
    category: 'function',
    subcategory: 'Conversion',
    summary: 'Convert a value to a string.',
    syntax: 'toString(<expr>)',
    description: 'Converts a number, boolean, or temporal value to its string representation.',
    examples: [
      {
        label: 'Int to string',
        query: "RETURN toString(42) + ' items' AS label",
      },
    ],
    related: ['fn-tointeger'],
  },
  {
    id: 'fn-toboolean',
    name: 'toBoolean()',
    category: 'function',
    subcategory: 'Conversion',
    summary: 'Convert a value to a boolean.',
    syntax: 'toBoolean(<expr>)',
    description: "Converts a string ('true'/'false') or integer (0/non-zero) to a boolean.",
    examples: [
      {
        label: 'String to bool',
        query: "RETURN toBoolean('true') AS flag",
      },
    ],
    related: ['fn-tointeger'],
  },

  // -- String functions ----------------------------------------------------
  {
    id: 'fn-toupper',
    name: 'toUpper()',
    category: 'function',
    subcategory: 'String',
    summary: 'Convert a string to uppercase.',
    syntax: 'toUpper(<string>)',
    description: 'Returns the string with all characters converted to uppercase.',
    aliases: ['toUpperCase'],
    examples: [
      { label: 'Uppercase', query: "RETURN toUpper('hello') AS result" },
    ],
    related: ['fn-tolower'],
  },
  {
    id: 'fn-tolower',
    name: 'toLower()',
    category: 'function',
    subcategory: 'String',
    summary: 'Convert a string to lowercase.',
    syntax: 'toLower(<string>)',
    description: 'Returns the string with all characters converted to lowercase.',
    aliases: ['toLowerCase'],
    examples: [
      { label: 'Lowercase', query: "RETURN toLower('HELLO') AS result" },
    ],
    related: ['fn-toupper'],
  },
  {
    id: 'fn-trim',
    name: 'trim() / lTrim() / rTrim()',
    category: 'function',
    subcategory: 'String',
    summary: 'Remove whitespace from a string.',
    syntax: 'trim(<string>)\nlTrim(<string>)\nrTrim(<string>)',
    description: 'trim removes leading and trailing whitespace. lTrim removes leading only. rTrim removes trailing only.',
    aliases: ['btrim'],
    examples: [
      { label: 'Trim', query: "RETURN trim('  hello  ') AS result" },
    ],
  },
  {
    id: 'fn-replace',
    name: 'replace()',
    category: 'function',
    subcategory: 'String',
    summary: 'Replace occurrences of a substring.',
    syntax: 'replace(<string>, <search>, <replacement>)',
    description: 'Replaces all occurrences of search with replacement.',
    examples: [
      {
        label: 'Replace',
        query: "RETURN replace('hello world', 'world', 'Cypher') AS result",
      },
    ],
  },
  {
    id: 'fn-substring',
    name: 'substring()',
    category: 'function',
    subcategory: 'String',
    summary: 'Extract part of a string.',
    syntax: 'substring(<string>, <start> [, <length>])',
    description: 'Returns a substring starting at the given zero-based index. If length is omitted, returns to the end.',
    examples: [
      {
        label: 'Substring',
        query: "RETURN substring('hello world', 6) AS result",
      },
    ],
    related: ['fn-left', 'fn-right'],
  },
  {
    id: 'fn-left',
    name: 'left()',
    category: 'function',
    subcategory: 'String',
    summary: 'Get the first N characters.',
    syntax: 'left(<string>, <n>)',
    description: 'Returns the leftmost n characters of the string.',
    examples: [
      { label: 'Left', query: "RETURN left('hello', 3) AS result" },
    ],
    related: ['fn-right', 'fn-substring'],
  },
  {
    id: 'fn-right',
    name: 'right()',
    category: 'function',
    subcategory: 'String',
    summary: 'Get the last N characters.',
    syntax: 'right(<string>, <n>)',
    description: 'Returns the rightmost n characters of the string.',
    examples: [
      { label: 'Right', query: "RETURN right('hello', 3) AS result" },
    ],
    related: ['fn-left', 'fn-substring'],
  },
  {
    id: 'fn-split',
    name: 'split()',
    category: 'function',
    subcategory: 'String',
    summary: 'Split a string into a list.',
    syntax: 'split(<string>, <delimiter>)',
    description: 'Splits the string on every occurrence of the delimiter and returns a list of parts.',
    examples: [
      {
        label: 'Split',
        query: "RETURN split('a,b,c', ',') AS parts",
      },
    ],
  },
  {
    id: 'fn-reverse',
    name: 'reverse()',
    category: 'function',
    subcategory: 'String',
    summary: 'Reverse a string or list.',
    syntax: 'reverse(<string_or_list>)',
    description: 'Reverses the characters in a string or the elements in a list.',
    examples: [
      { label: 'Reverse', query: "RETURN reverse('hello') AS result" },
    ],
  },

  // -- Numeric functions ---------------------------------------------------
  {
    id: 'fn-abs',
    name: 'abs()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Absolute value.',
    syntax: 'abs(<number>)',
    description: 'Returns the absolute (non-negative) value of a number.',
    examples: [{ label: 'Abs', query: 'RETURN abs(-42) AS result' }],
  },
  {
    id: 'fn-ceil',
    name: 'ceil()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Round up to the nearest integer.',
    syntax: 'ceil(<number>)',
    description: 'Returns the smallest integer greater than or equal to the number.',
    examples: [{ label: 'Ceil', query: 'RETURN ceil(3.14) AS result' }],
    related: ['fn-floor', 'fn-round'],
  },
  {
    id: 'fn-floor',
    name: 'floor()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Round down to the nearest integer.',
    syntax: 'floor(<number>)',
    description: 'Returns the largest integer less than or equal to the number.',
    examples: [{ label: 'Floor', query: 'RETURN floor(3.99) AS result' }],
    related: ['fn-ceil', 'fn-round'],
  },
  {
    id: 'fn-round',
    name: 'round()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Round to the nearest integer.',
    syntax: 'round(<number>)',
    description: 'Rounds to the nearest integer using half-up rounding.',
    examples: [{ label: 'Round', query: 'RETURN round(3.5) AS result' }],
    related: ['fn-ceil', 'fn-floor'],
  },
  {
    id: 'fn-sign',
    name: 'sign()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Sign of a number (-1, 0, or 1).',
    syntax: 'sign(<number>)',
    description: 'Returns -1 for negative, 0 for zero, 1 for positive.',
    examples: [
      { label: 'Sign', query: 'RETURN sign(-5), sign(0), sign(42)' },
    ],
  },
  {
    id: 'fn-sqrt',
    name: 'sqrt()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Square root.',
    syntax: 'sqrt(<number>)',
    description: 'Returns the square root of a non-negative number.',
    examples: [{ label: 'Sqrt', query: 'RETURN sqrt(144) AS result' }],
  },
  {
    id: 'fn-log',
    name: 'log() / log10()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Natural or base-10 logarithm.',
    syntax: 'log(<number>)\nlog10(<number>)',
    description: 'log returns the natural logarithm (base e). log10 returns the base-10 logarithm.',
    examples: [
      { label: 'Logarithms', query: 'RETURN log(e()), log10(1000)' },
    ],
    related: ['fn-exp'],
  },
  {
    id: 'fn-exp',
    name: 'exp()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Exponential function (e^x).',
    syntax: 'exp(<number>)',
    description: 'Returns e raised to the power of the given number.',
    examples: [{ label: 'Exp', query: 'RETURN exp(1) AS result' }],
    related: ['fn-log'],
  },
  {
    id: 'fn-rand',
    name: 'rand()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Random float between 0 and 1.',
    syntax: 'rand()',
    description: 'Returns a pseudo-random float in the range [0, 1).',
    examples: [
      { label: 'Random', query: 'RETURN rand() AS r, toInteger(rand() * 100) AS r100' },
    ],
  },
  {
    id: 'fn-pi',
    name: 'pi() / e()',
    category: 'function',
    subcategory: 'Numeric',
    summary: 'Mathematical constants.',
    syntax: 'pi()\ne()',
    description: 'pi() returns 3.14159... e() returns 2.71828...',
    examples: [
      { label: 'Constants', query: 'RETURN pi(), e()' },
    ],
  },

  // -- List functions ------------------------------------------------------
  {
    id: 'fn-size',
    name: 'size() / length()',
    category: 'function',
    subcategory: 'List',
    summary: 'Get the length of a list, string, or path.',
    syntax: 'size(<list_or_string_or_path>)',
    description: 'Returns the number of elements in a list, characters in a string, or relationships in a path.',
    aliases: ['length'],
    examples: [
      {
        label: 'List size',
        query: "RETURN size([1, 2, 3]) AS listLen, size('hello') AS strLen",
      },
    ],
    related: ['fn-head', 'fn-tail', 'fn-last'],
  },
  {
    id: 'fn-head',
    name: 'head()',
    category: 'function',
    subcategory: 'List',
    summary: 'Get the first element of a list.',
    syntax: 'head(<list>)',
    description: 'Returns the first element. Returns NULL for an empty list.',
    examples: [
      { label: 'Head', query: 'RETURN head([1, 2, 3]) AS first' },
    ],
    related: ['fn-tail', 'fn-last'],
  },
  {
    id: 'fn-tail',
    name: 'tail()',
    category: 'function',
    subcategory: 'List',
    summary: 'Get all elements except the first.',
    syntax: 'tail(<list>)',
    description: 'Returns a new list with all elements except the first.',
    examples: [
      { label: 'Tail', query: 'RETURN tail([1, 2, 3]) AS rest' },
    ],
    related: ['fn-head', 'fn-last'],
  },
  {
    id: 'fn-last',
    name: 'last()',
    category: 'function',
    subcategory: 'List',
    summary: 'Get the last element of a list.',
    syntax: 'last(<list>)',
    description: 'Returns the last element. Returns NULL for an empty list.',
    examples: [
      { label: 'Last', query: 'RETURN last([1, 2, 3]) AS end' },
    ],
    related: ['fn-head', 'fn-tail'],
  },
  {
    id: 'fn-range',
    name: 'range()',
    category: 'function',
    subcategory: 'List',
    summary: 'Generate a list of integers.',
    syntax: 'range(<start>, <end> [, <step>])',
    description: 'Returns a list of integers from start to end (inclusive). Optional step defaults to 1.',
    examples: [
      {
        label: 'Range',
        query: 'RETURN range(1, 5) AS r, range(0, 10, 2) AS evens',
      },
    ],
  },

  // -- Path functions ------------------------------------------------------
  {
    id: 'fn-nodes',
    name: 'nodes()',
    category: 'function',
    subcategory: 'Path',
    summary: 'Get all nodes in a path.',
    syntax: 'nodes(<path>)',
    description: 'Returns a list of all nodes traversed in the path, in order.',
    examples: [
      {
        label: 'Path nodes',
        query:
          'MATCH p = (a)-[*1..3]->(b) RETURN nodes(p) LIMIT 1',
      },
    ],
    related: ['fn-relationships'],
  },
  {
    id: 'fn-relationships',
    name: 'relationships() / rels()',
    category: 'function',
    subcategory: 'Path',
    summary: 'Get all relationships in a path.',
    syntax: 'relationships(<path>)',
    description: 'Returns a list of all relationships traversed in the path, in order.',
    aliases: ['rels'],
    examples: [
      {
        label: 'Path rels',
        query:
          'MATCH p = (a)-[*1..3]->(b) RETURN relationships(p) LIMIT 1',
      },
    ],
    related: ['fn-nodes'],
  },
  {
    id: 'fn-startnode',
    name: 'startNode() / endNode()',
    category: 'function',
    subcategory: 'Path',
    summary: 'Get the source or target node of a relationship.',
    syntax: 'startNode(<relationship>)\nendNode(<relationship>)',
    description: 'startNode returns the source node; endNode returns the target node of a relationship.',
    examples: [
      {
        label: 'Endpoints',
        query:
          'MATCH ()-[r:KNOWS]->() RETURN startNode(r).name, endNode(r).name LIMIT 5',
      },
    ],
  },

  // -- Temporal functions --------------------------------------------------
  {
    id: 'fn-datetime',
    name: 'datetime()',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Create or get the current date-time.',
    syntax: "datetime() | datetime(<map>) | datetime('<iso_string>')",
    description:
      'With no arguments, returns the current transaction date-time. Can also construct from a map ({year, month, day, hour, minute, second}) or ISO 8601 string.',
    examples: [
      { label: 'Current', query: 'RETURN datetime() AS now' },
      {
        label: 'From map',
        query: 'RETURN datetime({year: 2026, month: 2, day: 25, hour: 14}) AS dt',
      },
      {
        label: 'From epoch',
        query: 'RETURN datetime.fromepochmillis(1740480000000) AS dt',
      },
    ],
    related: ['fn-date', 'fn-duration'],
  },
  {
    id: 'fn-date',
    name: 'date()',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Create or get the current date.',
    syntax: "date() | date(<map>) | date('<iso_string>')",
    description:
      'With no arguments, returns the current transaction date. Can construct from {year, month, day} or ISO string.',
    examples: [
      { label: 'Current', query: 'RETURN date() AS today' },
      {
        label: 'From map',
        query: 'RETURN date({year: 2026, month: 1, day: 1}) AS d',
      },
    ],
    related: ['fn-datetime', 'fn-duration'],
  },
  {
    id: 'fn-localtime',
    name: 'localtime() / time()',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Create or get the current time.',
    syntax: 'localtime() | time()',
    description:
      'localtime() returns time without timezone. time() returns time with timezone offset.',
    examples: [
      {
        label: 'Current times',
        query: 'RETURN localtime() AS local, time() AS withTz',
      },
    ],
    related: ['fn-datetime'],
  },
  {
    id: 'fn-localdatetime',
    name: 'localdatetime()',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Create or get a date-time without timezone.',
    syntax: 'localdatetime() | localdatetime(<map>)',
    description: 'Like datetime() but without timezone information.',
    examples: [
      {
        label: 'Current',
        query: 'RETURN localdatetime() AS ldt',
      },
    ],
    related: ['fn-datetime'],
  },
  {
    id: 'fn-duration',
    name: 'duration()',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Create a temporal duration.',
    syntax: "duration(<map>) | duration('<iso_string>')",
    description:
      'Creates a duration from {years, months, weeks, days, hours, minutes, seconds} or ISO 8601 duration string. Durations can be added/subtracted from temporal values.',
    examples: [
      {
        label: 'Duration arithmetic',
        query: "RETURN date() + duration({months: 3}) AS threeMonthsLater",
      },
      {
        label: 'Between dates',
        query:
          "RETURN duration.between(date('2025-01-01'), date('2026-02-25')) AS diff",
      },
    ],
    related: ['fn-datetime', 'fn-date'],
  },
  {
    id: 'fn-temporal-truncate',
    name: 'Temporal truncate',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Truncate a temporal value to a given precision.',
    syntax:
      'date.truncate(<unit>, <temporal>)\ndatetime.truncate(<unit>, <temporal>)\nlocaldatetime.truncate(<unit>, <temporal>)\ntime.truncate(<unit>, <temporal>)\nlocaltime.truncate(<unit>, <temporal>)',
    description:
      "Truncates a temporal value to the specified unit of precision. Units: 'millennium', 'century', 'decade', 'year', 'weekYear', 'quarter', 'month', 'week', 'day', 'hour', 'minute', 'second', 'millisecond', 'microsecond'.",
    examples: [
      {
        label: 'Truncate to month',
        query: "RETURN date.truncate('month', date()) AS firstOfMonth",
      },
      {
        label: 'Truncate datetime to hour',
        query: "RETURN datetime.truncate('hour', datetime()) AS onTheHour",
      },
    ],
    related: ['fn-datetime', 'fn-date'],
  },
  {
    id: 'fn-temporal-epoch',
    name: 'datetime.fromepoch()',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Create a datetime from epoch seconds or milliseconds.',
    syntax: 'datetime.fromepoch(<seconds>)\ndatetime.fromepochmillis(<milliseconds>)',
    description:
      'Converts a Unix epoch timestamp (seconds or milliseconds since 1970-01-01T00:00:00Z) into a datetime value.',
    examples: [
      {
        label: 'From epoch seconds',
        query: 'RETURN datetime.fromepoch(1740480000) AS dt',
      },
      {
        label: 'From epoch millis',
        query: 'RETURN datetime.fromepochmillis(1740480000000) AS dt',
      },
    ],
    aliases: ['datetime.fromepochmillis'],
    related: ['fn-datetime'],
  },
  {
    id: 'fn-duration-utils',
    name: 'Duration utilities',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Compute durations between temporal values.',
    syntax:
      'duration.between(<temporal1>, <temporal2>)\nduration.inMonths(<temporal1>, <temporal2>)\nduration.inDays(<temporal1>, <temporal2>)\nduration.inSeconds(<temporal1>, <temporal2>)',
    description:
      'duration.between returns the full duration between two temporal values. duration.inMonths/inDays/inSeconds return durations normalized to the specified unit.',
    examples: [
      {
        label: 'Full duration',
        query: "RETURN duration.between(date('2025-01-01'), date('2026-02-25')) AS diff",
      },
      {
        label: 'In months',
        query: "RETURN duration.inMonths(date('2025-01-01'), date('2026-02-25')) AS months",
      },
      {
        label: 'In seconds',
        query: "RETURN duration.inSeconds(datetime('2026-02-25T00:00:00Z'), datetime()) AS elapsed",
      },
    ],
    aliases: ['duration.between', 'duration.inMonths', 'duration.inDays', 'duration.inSeconds'],
    related: ['fn-duration'],
  },
  {
    id: 'fn-temporal-clock',
    name: 'Temporal clock functions',
    category: 'function',
    subcategory: 'Temporal',
    summary: 'Get the current temporal value from different clock sources.',
    syntax:
      '<type>.transaction()\n<type>.statement()\n<type>.realtime()',
    description:
      'Each temporal type (date, localtime, time, localdatetime, datetime) provides three clock functions: .transaction() returns the same value for the entire transaction, .statement() returns the same value for the current statement, .realtime() returns the wall-clock value at the moment of evaluation. Calling a temporal constructor with no arguments is equivalent to .transaction().',
    examples: [
      {
        label: 'Transaction vs realtime',
        query: 'RETURN datetime.transaction() AS txTime, datetime.realtime() AS wallTime',
      },
      {
        label: 'Date clocks',
        query: 'RETURN date.transaction() AS txDate, date.statement() AS stmtDate',
      },
    ],
    aliases: [
      'date.transaction', 'date.statement', 'date.realtime',
      'localtime.transaction', 'localtime.statement', 'localtime.realtime',
      'time.transaction', 'time.statement', 'time.realtime',
      'localdatetime.transaction', 'localdatetime.statement', 'localdatetime.realtime',
      'datetime.transaction', 'datetime.statement', 'datetime.realtime',
    ],
    related: ['fn-datetime', 'fn-date', 'fn-localtime', 'fn-localdatetime'],
  },

  // -- Types ---------------------------------------------------------------
  {
    id: 'type-integer',
    name: 'INTEGER',
    category: 'type',
    summary: '64-bit signed integer.',
    syntax: '42, -1, 0',
    description: 'Whole numbers stored as 64-bit signed integers (i64). Range: -2^63 to 2^63-1.',
    examples: [
      { label: 'Integer', query: 'RETURN 42, -1, 2^32' },
    ],
  },
  {
    id: 'type-float',
    name: 'FLOAT',
    category: 'type',
    summary: '64-bit IEEE 754 floating point.',
    syntax: '3.14, -0.5, 1.0e10',
    description: 'Decimal numbers stored as 64-bit IEEE 754 doubles.',
    examples: [
      { label: 'Float', query: 'RETURN 3.14, 1.0e-3, pi()' },
    ],
  },
  {
    id: 'type-string',
    name: 'STRING',
    category: 'type',
    summary: 'UTF-8 text.',
    syntax: "'hello', \"world\"",
    description: 'Unicode text strings, delimited by single or double quotes.',
    examples: [
      { label: 'String', query: "RETURN 'single' + \" double\"" },
    ],
  },
  {
    id: 'type-boolean',
    name: 'BOOLEAN',
    category: 'type',
    summary: 'true or false.',
    syntax: 'true, false',
    description: 'Boolean truth values. NULL is distinct from false.',
    examples: [
      { label: 'Boolean', query: 'RETURN true, false, null' },
    ],
  },
  {
    id: 'type-null',
    name: 'NULL',
    category: 'type',
    summary: 'Absence of a value.',
    syntax: 'null',
    description:
      'Represents a missing or undefined value. Comparisons with NULL propagate NULL (three-valued logic). Use IS NULL / IS NOT NULL to test.',
    examples: [
      { label: 'Null', query: 'RETURN null = null, null IS NULL' },
    ],
  },
  {
    id: 'type-list',
    name: 'LIST',
    category: 'type',
    summary: 'Ordered collection of values.',
    syntax: '[1, 2, 3], [], [1, "two", true]',
    description: 'Ordered, zero-indexed lists. Can contain mixed types. Supports indexing, slicing, and comprehensions.',
    examples: [
      {
        label: 'List operations',
        query: "WITH [1, 2, 3, 4, 5] AS l RETURN size(l), l[0], l[1..3], l + [6]",
      },
    ],
  },
  {
    id: 'type-map',
    name: 'MAP',
    category: 'type',
    summary: 'Key-value pairs (string keys).',
    syntax: '{key: value, ...}',
    description: 'Unordered collection of key-value pairs. Keys are strings. Access values with dot notation or bracket syntax.',
    examples: [
      {
        label: 'Map access',
        query: "WITH {name: 'Alice', age: 30} AS m RETURN m.name, m['age']",
      },
    ],
  },
  {
    id: 'type-node',
    name: 'NODE',
    category: 'type',
    summary: 'A graph node with labels and properties.',
    syntax: 'Returned by MATCH (n)',
    description: 'A vertex in the graph. Has an internal ID, zero or more labels, and a property map.',
    examples: [
      { label: 'Node', query: 'MATCH (n) RETURN n, labels(n), properties(n) LIMIT 1' },
    ],
  },
  {
    id: 'type-relationship',
    name: 'RELATIONSHIP',
    category: 'type',
    summary: 'A directed edge with a type and properties.',
    syntax: 'Returned by MATCH ()-[r]->()',
    description: 'A directed edge connecting two nodes. Has an internal ID, exactly one type, and a property map.',
    examples: [
      {
        label: 'Relationship',
        query: 'MATCH ()-[r]->() RETURN r, type(r), properties(r) LIMIT 1',
      },
    ],
  },
  {
    id: 'type-path',
    name: 'PATH',
    category: 'type',
    summary: 'A sequence of alternating nodes and relationships.',
    syntax: 'Returned by MATCH p = (a)-[*]->(b)',
    description: 'An ordered sequence of nodes and relationships forming a walk through the graph. Decompose with nodes(p) and relationships(p).',
    examples: [
      {
        label: 'Path',
        query: 'MATCH p = (a)-[*1..2]->(b) RETURN p, length(p) LIMIT 1',
      },
    ],
  },
  {
    id: 'type-temporal',
    name: 'Temporal types',
    category: 'type',
    summary: 'DATE, TIME, DATETIME, DURATION.',
    syntax: 'date(), time(), datetime(), duration()',
    description:
      'Traverse supports five temporal types: DATE (calendar date), LOCAL_TIME (time without tz), TIME (time with tz offset), LOCAL_DATE_TIME (date+time without tz), DATE_TIME (date+time with tz), and DURATION (temporal span). All follow ISO 8601.',
    examples: [
      {
        label: 'Temporal values',
        query: 'RETURN date() AS d, datetime() AS dt, duration({days: 7}) AS week',
      },
    ],
  },

  // -- Schema & Indexes ----------------------------------------------------
  {
    id: 'schema-create-index',
    name: 'CREATE INDEX',
    category: 'schema',
    summary: 'Create a label+property index.',
    syntax: 'CREATE INDEX <name> [IF NOT EXISTS] FOR (n:<Label>) ON (n.<property>)',
    description: 'Creates an index on the given label and property combination. Speeds up lookups that filter on that label and property. Use IF NOT EXISTS to skip silently if the index already exists.',
    examples: [
      {
        label: 'Create index',
        query: 'CREATE INDEX idx_person_name FOR (n:Person) ON (n.name)',
      },
      {
        label: 'Idempotent',
        query: 'CREATE INDEX idx_person_name IF NOT EXISTS FOR (n:Person) ON (n.name)',
      },
    ],
    related: ['schema-create-edge-index', 'schema-drop-index', 'schema-show-indexes'],
  },
  {
    id: 'schema-create-edge-index',
    name: 'CREATE EDGE INDEX',
    category: 'schema',
    summary: 'Create an edge type or edge property index.',
    syntax: 'CREATE EDGE INDEX ON :<EdgeType>(<property>)\nCREATE EDGE INDEX ON :<EdgeType>',
    description: 'Creates an index on edges. Two forms: a property index on a specific edge type and property combination, or a type-only index that indexes all edges of a given type. Speeds up traversals that filter on edge type or edge properties. The index name is auto-generated (eidx_<type>_<property> or eidx_<type>).',
    examples: [
      {
        label: 'Property index',
        query: 'CREATE EDGE INDEX ON :KNOWS(since)',
      },
      {
        label: 'Type-only index',
        query: 'CREATE EDGE INDEX ON :WORKS_AT',
      },
    ],
    related: ['schema-create-index', 'schema-drop-edge-index', 'schema-show-indexes'],
  },
  {
    id: 'schema-drop-index',
    name: 'DROP INDEX',
    category: 'schema',
    summary: 'Remove a node index.',
    syntax: 'DROP INDEX <name> [IF EXISTS]',
    description: 'Drops the named node index. Use IF EXISTS to skip silently if the index does not exist.',
    examples: [
      { label: 'Drop index', query: 'DROP INDEX idx_person_name' },
      { label: 'Idempotent', query: 'DROP INDEX idx_person_name IF EXISTS' },
    ],
    related: ['schema-create-index', 'schema-drop-edge-index', 'schema-show-indexes'],
  },
  {
    id: 'schema-drop-edge-index',
    name: 'DROP EDGE INDEX',
    category: 'schema',
    summary: 'Remove an edge index.',
    syntax: 'DROP EDGE INDEX <name>',
    description: 'Drops the named edge index.',
    examples: [
      { label: 'Drop edge index', query: 'DROP EDGE INDEX eidx_KNOWS_since' },
    ],
    related: ['schema-create-edge-index', 'schema-drop-index', 'schema-show-indexes'],
  },
  {
    id: 'schema-create-constraint-unique',
    name: 'CREATE CONSTRAINT (UNIQUE)',
    category: 'schema',
    summary: 'Enforce uniqueness on a property.',
    syntax:
      'CREATE CONSTRAINT <name> [IF NOT EXISTS] FOR (n:<Label>) REQUIRE n.<property> IS UNIQUE',
    description:
      'Creates a uniqueness constraint. Prevents two nodes with the same label from having the same value for the constrained property. Validates existing data on creation. For best performance, create a matching index on the same label/property. Use IF NOT EXISTS for idempotent creation.',
    examples: [
      {
        label: 'Unique constraint',
        query:
          'CREATE CONSTRAINT uniq_person_email FOR (n:Person) REQUIRE n.email IS UNIQUE',
      },
    ],
    related: ['schema-create-constraint-notnull', 'schema-drop-constraint', 'schema-show-constraints'],
  },
  {
    id: 'schema-create-constraint-notnull',
    name: 'CREATE CONSTRAINT (NOT NULL)',
    category: 'schema',
    summary: 'Require a property to be present.',
    syntax:
      'CREATE CONSTRAINT <name> [IF NOT EXISTS] FOR (n:<Label>) REQUIRE n.<property> IS NOT NULL',
    description:
      'Creates a not-null constraint. Prevents nodes with the given label from having a NULL (missing) value for the constrained property. Use IF NOT EXISTS for idempotent creation.',
    examples: [
      {
        label: 'Not-null constraint',
        query:
          'CREATE CONSTRAINT nn_person_name FOR (n:Person) REQUIRE n.name IS NOT NULL',
      },
    ],
    related: ['schema-create-constraint-unique', 'schema-drop-constraint', 'schema-show-constraints'],
  },
  {
    id: 'schema-drop-constraint',
    name: 'DROP CONSTRAINT',
    category: 'schema',
    summary: 'Remove a constraint.',
    syntax: 'DROP CONSTRAINT <name> [IF EXISTS]',
    description: 'Drops the named constraint. Use IF EXISTS to skip silently if the constraint does not exist.',
    examples: [
      {
        label: 'Drop constraint',
        query: 'DROP CONSTRAINT uniq_person_email',
      },
    ],
    related: ['schema-create-constraint-unique', 'schema-show-constraints'],
  },
  {
    id: 'schema-analyze',
    name: 'ANALYZE GRAPH',
    category: 'schema',
    summary: 'Collect statistics for query optimization.',
    syntax: 'ANALYZE GRAPH [LABEL <label1>, <label2>, ...]',
    description:
      'Collects cardinality and selectivity statistics used by the cost-based query planner. Without LABEL, analyzes the entire graph. With LABEL, only the specified labels.',
    examples: [
      { label: 'Analyze all', query: 'ANALYZE GRAPH' },
      {
        label: 'Specific labels',
        query: 'ANALYZE GRAPH LABEL Person, Company',
      },
    ],
  },

  // --- Schema inspection ---------------------------------------------------

  {
    id: 'schema-show-indexes',
    name: 'SHOW INDEXES',
    category: 'schema',
    summary: 'List all node and edge indexes.',
    syntax: 'SHOW INDEXES',
    description:
      'Lists all indexes in the current database. Returns columns: name, type (BTREE/LOOKUP), entityType (NODE/RELATIONSHIP), labelsOrTypes, and properties.',
    examples: [
      { label: 'List all indexes', query: 'SHOW INDEXES' },
    ],
    aliases: ['SHOW INDEX'],
    related: ['schema-create-index', 'schema-create-edge-index', 'schema-drop-index', 'schema-drop-edge-index'],
  },
  {
    id: 'schema-show-constraints',
    name: 'SHOW CONSTRAINTS',
    category: 'schema',
    summary: 'List all constraints.',
    syntax: 'SHOW CONSTRAINTS',
    description:
      'Lists all constraints in the current database. Returns columns: name, type (UNIQUE/NOT NULL), entityType (NODE), labelsOrTypes, and properties.',
    examples: [
      { label: 'List all constraints', query: 'SHOW CONSTRAINTS' },
    ],
    aliases: ['SHOW CONSTRAINT'],
    related: ['schema-create-constraint-unique', 'schema-create-constraint-notnull', 'schema-drop-constraint'],
  },

  // --- Database management -------------------------------------------------

  {
    id: 'admin-show-databases',
    name: 'SHOW DATABASES',
    category: 'admin',
    subcategory: 'Databases',
    summary: 'List all databases.',
    syntax: 'SHOW DATABASES',
    description:
      'Lists all databases on the server with their status, default flag, read-only flag, and any aliases.',
    examples: [
      { label: 'List databases', query: 'SHOW DATABASES' },
    ],
    related: ['admin-create-database', 'admin-drop-database'],
  },
  {
    id: 'admin-create-database',
    name: 'CREATE DATABASE',
    category: 'admin',
    subcategory: 'Databases',
    summary: 'Create a new empty database.',
    syntax: 'CREATE DATABASE <name>',
    description:
      'Creates a new empty database. The name must contain only alphanumeric characters and underscores.',
    examples: [
      { label: 'Create database', query: 'CREATE DATABASE analytics' },
    ],
    related: ['admin-drop-database', 'admin-show-databases'],
  },
  {
    id: 'admin-drop-database',
    name: 'DROP DATABASE',
    category: 'admin',
    subcategory: 'Databases',
    summary: 'Delete a database.',
    syntax: 'DROP DATABASE <name>',
    description:
      'Permanently deletes a database and all its data. Only available when the server is started with TRAVERSE_ALLOW_DROP_DATABASE=true.',
    examples: [
      { label: 'Drop database', query: 'DROP DATABASE analytics' },
    ],
    related: ['admin-create-database', 'admin-show-databases'],
  },
  {
    id: 'admin-load-database',
    name: 'LOAD DATABASE',
    category: 'admin',
    subcategory: 'Databases',
    summary: 'Load a .tvdb file as a read-only database.',
    syntax: "LOAD DATABASE <name> FROM '<file>'",
    description:
      'Loads a .tvdb file from the server data directory as an immutable read-only database. Useful for mounting archived or snapshot data.',
    examples: [
      { label: 'Load database', query: "LOAD DATABASE archive FROM 'old_data.tvdb'" },
    ],
    related: ['admin-show-databases'],
  },

  // --- Alias management ----------------------------------------------------

  {
    id: 'admin-show-aliases',
    name: 'SHOW ALIASES',
    category: 'admin',
    subcategory: 'Aliases',
    summary: 'List all database aliases.',
    syntax: 'SHOW ALIASES',
    description:
      'Lists all aliases and the databases they point to.',
    examples: [
      { label: 'List aliases', query: 'SHOW ALIASES' },
    ],
    related: ['admin-create-alias', 'admin-drop-alias'],
  },
  {
    id: 'admin-create-alias',
    name: 'CREATE ALIAS',
    category: 'admin',
    subcategory: 'Aliases',
    summary: 'Create a database alias.',
    syntax: 'CREATE ALIAS <name> FOR DATABASE <target>',
    description:
      'Creates an alias that points to an existing database. Queries can use the alias name in place of the database name.',
    examples: [
      { label: 'Create alias', query: 'CREATE ALIAS prod FOR DATABASE production' },
    ],
    related: ['admin-alter-alias', 'admin-drop-alias', 'admin-show-aliases'],
  },
  {
    id: 'admin-alter-alias',
    name: 'ALTER ALIAS',
    category: 'admin',
    subcategory: 'Aliases',
    summary: 'Retarget an alias to a different database.',
    syntax: 'ALTER ALIAS <name> SET DATABASE <target>',
    description:
      'Atomically retargets an existing alias to point to a different database.',
    examples: [
      { label: 'Retarget alias', query: 'ALTER ALIAS prod SET DATABASE production_v2' },
    ],
    related: ['admin-create-alias', 'admin-drop-alias'],
  },
  {
    id: 'admin-drop-alias',
    name: 'DROP ALIAS',
    category: 'admin',
    subcategory: 'Aliases',
    summary: 'Remove a database alias.',
    syntax: 'DROP ALIAS <name>',
    description:
      'Drops the named alias. The target database is not affected.',
    examples: [
      { label: 'Drop alias', query: 'DROP ALIAS prod' },
    ],
    related: ['admin-create-alias', 'admin-show-aliases'],
  },

  // --- User management -----------------------------------------------------

  {
    id: 'admin-show-users',
    name: 'SHOW USERS',
    category: 'admin',
    subcategory: 'Users',
    summary: 'List all users.',
    syntax: 'SHOW USERS',
    description:
      'Lists all users and their roles.',
    examples: [
      { label: 'List users', query: 'SHOW USERS' },
    ],
    related: ['admin-create-user', 'admin-drop-user'],
  },
  {
    id: 'admin-create-user',
    name: 'CREATE USER',
    category: 'admin',
    subcategory: 'Users',
    summary: 'Create a new user.',
    syntax: "CREATE USER <name> SET PASSWORD '<pass>' [SET ROLE ADMIN|EDITOR|READER]",
    description:
      'Creates a new user with the given password. Role defaults to READER if not specified. Available roles: ADMIN (full access), EDITOR (read + write), READER (read-only).',
    examples: [
      { label: 'Create admin', query: "CREATE USER alice SET PASSWORD 'secret' SET ROLE ADMIN" },
      { label: 'Create reader', query: "CREATE USER bob SET PASSWORD 'pass123'" },
    ],
    related: ['admin-alter-user', 'admin-drop-user', 'admin-show-users'],
  },
  {
    id: 'admin-alter-user',
    name: 'ALTER USER',
    category: 'admin',
    subcategory: 'Users',
    summary: 'Update a user password or role.',
    syntax: "ALTER USER <name> SET PASSWORD '<pass>' | SET ROLE ADMIN|EDITOR|READER",
    description:
      'Updates a user\'s password, role, or both.',
    examples: [
      { label: 'Change password', query: "ALTER USER alice SET PASSWORD 'newpass'" },
      { label: 'Change role', query: 'ALTER USER alice SET ROLE EDITOR' },
    ],
    related: ['admin-create-user', 'admin-show-users'],
  },
  {
    id: 'admin-drop-user',
    name: 'DROP USER',
    category: 'admin',
    subcategory: 'Users',
    summary: 'Delete a user.',
    syntax: 'DROP USER <name>',
    description:
      'Removes a user account.',
    examples: [
      { label: 'Drop user', query: 'DROP USER alice' },
    ],
    related: ['admin-create-user', 'admin-show-users'],
  },
]

// ---------------------------------------------------------------------------
// Helpers for the UI
// ---------------------------------------------------------------------------

/** All unique categories in display order */
export const CATEGORIES: CypherCategory[] = [
  'clause',
  'function',
  'operator',
  'expression',
  'pattern',
  'type',
  'schema',
  'admin',
]
