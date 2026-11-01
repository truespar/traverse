import type * as Monaco from 'monaco-editor'

// Reserved words from ISO/IEC 39075:2024 section 6.2, plus the Cypher-style
// keywords the engine accepts for catalog operations (INDEX, CONSTRAINT,
// REQUIRE, UNIQUE, FOR, ON) - those land in the same `keyword` token
// class so the colour matches the rest of the highlighting.
const keywords = [
  // Reading
  'MATCH', 'OPTIONAL', 'WHERE',
  // Projection
  'RETURN', 'WITH', 'UNWIND', 'ORDER', 'BY', 'ASC', 'ASCENDING', 'DESC', 'DESCENDING',
  'SKIP', 'LIMIT', 'OFFSET', 'AS', 'DISTINCT', 'FINISH',
  // Writing
  'INSERT', 'DELETE', 'DETACH', 'SET', 'REMOVE',
  // Composition / control
  'CALL', 'YIELD', 'UNION', 'ALL',
  // Path / pattern
  'ANY', 'SHORTEST', 'ANY_SHORTEST', 'TRAIL', 'WALK',
  'PATH', 'PATHS', 'NODE', 'NODES', 'EDGE', 'EDGES', 'RELATIONSHIP',
  // Catalog
  'CREATE', 'DROP', 'GRAPH', 'INDEX', 'CONSTRAINT', 'USE', 'SCHEMA',
  'FOR', 'ON', 'REQUIRE', 'UNIQUE', 'KEY', 'REQUIRED', 'IF', 'NOT', 'EXISTS',
  // Types
  'BOOLEAN', 'INT', 'INTEGER', 'FLOAT', 'STRING', 'LIST', 'RECORD',
  'DATE', 'TIME', 'DATETIME', 'ZONED', 'TYPE', 'TYPED', 'CAST', 'OF',
  // Conditional / quantifier helpers
  'CASE', 'WHEN', 'THEN', 'ELSE', 'END',
  // Misc
  'FROM', 'TO', 'IN', 'IS', 'BETWEEN',
]

// Operators / value-keywords highlighted as keyword.operator so they
// pick up the operator tint in the theme.
const operators = [
  'AND', 'OR', 'XOR', 'NOT', 'IS', 'IN',
  'TRUE', 'FALSE', 'NULL', 'UNKNOWN',
]

// Function names - shared with Cypher. Highlight as `predefined`.
const functions = [
  // Aggregation
  'count', 'sum', 'avg', 'min', 'max', 'collect',
  'stDev', 'stDevP', 'percentileDisc', 'percentileCont',
  // Entity introspection
  'id', 'type', 'labels', 'keys', 'properties',
  'startNode', 'endNode', 'nodes', 'relationships',
  // List
  'length', 'size', 'head', 'last', 'tail', 'range', 'reverse',
  'reduce',
  // Type conversion
  'toInteger', 'toFloat', 'toString', 'toBoolean',
  // String
  'trim', 'btrim', 'ltrim', 'rtrim', 'replace', 'substring',
  'left', 'right', 'split',
  'toLower', 'toUpper', 'char_length', 'octet_length',
  // Math
  'abs', 'ceil', 'floor', 'round', 'sign', 'sqrt',
  'log', 'log10', 'exp', 'power',
  // Temporal
  'date', 'datetime', 'time', 'duration', 'current_date', 'current_time',
  // Utility
  'coalesce',
]

/**
 * Register the `gql` Monaco language. Same shape as
 * `registerCypherLanguage` so the editor can hot-swap the language on
 * a model with `monaco.editor.setModelLanguage(model, 'gql' | 'cypher')`
 * when the user toggles the dialect dropdown.
 */
export function registerGqlLanguage(monaco: typeof Monaco) {
  monaco.languages.register({ id: 'gql' })

  monaco.languages.setMonarchTokensProvider('gql', {
    ignoreCase: true,
    keywords,
    operators,
    functions,
    tokenizer: {
      root: [
        // Block comment - falls into the @blockComment state so we
        // can match `*/` across newlines without a back-reference.
        [/\/\*/, 'comment', '@blockComment'],

        // GQL line comments use `--` (SQL-style). We do *not*
        // highlight `//` as a comment - that's Cypher.
        // The lexer has carve-outs for `-->` and `---` so the
        // tokenizer should too: only fire when the next char is not
        // `>` or `-`.
        [/--(?![->]).*$/, 'comment'],

        // Strings (single & double quoted).
        [/"([^"\\]|\\.)*"/, 'string'],
        [/'([^'\\]|\\.)*'/, 'string'],

        // Numeric literals - hex / octal / binary / decimal / float.
        [/0[xX][0-9a-fA-F]+/, 'number.hex'],
        [/0[oO][0-7]+/, 'number.octal'],
        [/0[bB][01]+/, 'number.binary'],
        [/\d+\.\d+([eE][+-]?\d+)?/, 'number.float'],
        [/\.\d+([eE][+-]?\d+)?/, 'number.float'],
        [/\d+[eE][+-]?\d+/, 'number.float'],
        [/\d+/, 'number'],

        // Node labels :Label - same shape as Cypher; label
        // expressions like `:A&B` and `:!A` are tokenised
        // operator-by-operator (the `&`/`|`/`!` falls through to the
        // operators rule).
        [/:([A-Za-z_]\w*)/, 'type.identifier'],

        // Edge type lead-in `[:TYPE`
        [/\[:[A-Za-z_]\w*/, 'type.identifier'],

        // Parameters `$name` and `$"quoted name"`.
        [/\$[A-Za-z_]\w*/, 'variable'],
        [/\$"([^"\\]|\\.)*"/, 'variable'],

        // Backtick-delimited identifiers.
        [/`[^`]*`/, 'identifier'],

        // Identifiers / keywords.
        [/[A-Za-z_]\w*/, {
          cases: {
            '@keywords': 'keyword',
            '@operators': 'keyword.operator',
            '@functions': 'predefined',
            '@default': 'identifier',
          },
        }],

        // Operators - GQL adds `&` (label conjunction), `|` (label
        // disjunction), `~` (path concat), `::` (type cast), `->`
        // / `<-` (edge arrows).
        [/->|<-|::/, 'operator'],
        [/[<>]=?|<>|!=|=/, 'operator'],
        [/[+\-*/%^]/, 'operator'],
        [/[&|~?!]/, 'operator'],
        [/\.\./, 'operator'],

        // Brackets and punctuation.
        [/[{}()\[\]]/, 'delimiter.bracket'],
        [/[,;.]/, 'delimiter'],

        // Whitespace.
        [/\s+/, 'white'],
      ],
      blockComment: [
        [/[^*/]+/, 'comment'],
        [/\*\//, 'comment', '@pop'],
        [/[*/]/, 'comment'],
      ],
    },
  })

  // Auto-pair / bracket matching config - matches the Cypher
  // language so users get the same closing-quote / closing-bracket
  // behaviour from Monaco.
  monaco.languages.setLanguageConfiguration('gql', {
    comments: {
      lineComment: '--',
      blockComment: ['/*', '*/'],
    },
    brackets: [['{', '}'], ['[', ']'], ['(', ')']],
    autoClosingPairs: [
      { open: '{', close: '}' },
      { open: '[', close: ']' },
      { open: '(', close: ')' },
      { open: '"', close: '"' },
      { open: "'", close: "'" },
      { open: '`', close: '`' },
    ],
  })

  monaco.languages.registerCompletionItemProvider('gql', {
    provideCompletionItems: (model, position) => {
      const word = model.getWordUntilPosition(position)
      const range = {
        startLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endLineNumber: position.lineNumber,
        endColumn: position.column,
      }
      const suggestions: Monaco.languages.CompletionItem[] = [
        ...keywords.map((kw) => ({
          label: kw,
          kind: monaco.languages.CompletionItemKind.Keyword,
          insertText: kw,
          range,
        })),
        ...functions.map((fn) => ({
          label: fn,
          kind: monaco.languages.CompletionItemKind.Function,
          insertText: fn + '($0)',
          insertTextRules: monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet,
          range,
        })),
      ]
      return { suggestions }
    },
  })
}
