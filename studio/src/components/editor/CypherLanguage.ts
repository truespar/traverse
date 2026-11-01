import type * as Monaco from 'monaco-editor'

const keywords = [
  // Reading
  'MATCH', 'OPTIONAL', 'WHERE',
  // Projection
  'RETURN', 'WITH', 'UNWIND', 'ORDER', 'BY', 'ASC', 'ASCENDING', 'DESC', 'DESCENDING',
  'SKIP', 'LIMIT', 'AS', 'DISTINCT',
  // Writing
  'CREATE', 'MERGE', 'DELETE', 'DETACH', 'SET', 'REMOVE', 'ON',
  // Control
  'CALL', 'YIELD', 'UNION', 'ALL',
  // Expressions
  'CASE', 'WHEN', 'THEN', 'ELSE', 'END', 'EXISTS', 'COUNT',
  // Schema
  'INDEX', 'CONSTRAINT', 'DROP', 'IF', 'NOT', 'FOR', 'UNIQUE', 'REQUIRE',
  'SHOW', 'ANALYZE', 'GRAPH', 'EDGE',
  // Query analysis
  'EXPLAIN', 'PROFILE',
  // CSV
  'LOAD', 'CSV', 'FROM', 'HEADERS', 'FIELDTERMINATOR',
]

const operators = [
  'AND', 'OR', 'XOR', 'NOT', 'IN', 'STARTS', 'ENDS', 'CONTAINS', 'IS', 'NULL',
  'TRUE', 'FALSE',
]

const functions = [
  // Aggregation
  'count', 'sum', 'avg', 'min', 'max', 'collect',
  'stDev', 'stDevP', 'percentileDisc', 'percentileCont',
  // Entity introspection
  'id', 'type', 'labels', 'keys', 'properties',
  'startNode', 'endNode', 'nodes', 'relationships', 'rels',
  'typename', 'valueType',
  // List
  'length', 'size', 'head', 'last', 'tail', 'range', 'reverse',
  'reduce', 'any', 'all', 'none', 'single', 'exists',
  // Type conversion
  'toInteger', 'toInt', 'toFloat', 'toString', 'toBoolean',
  // String
  'trim', 'btrim', 'ltrim', 'rtrim', 'replace', 'substring',
  'left', 'right', 'split',
  'toLower', 'toLowerCase', 'toUpper', 'toUpperCase',
  // Math
  'abs', 'ceil', 'floor', 'round', 'sign', 'rand',
  'log', 'log10', 'exp', 'e', 'sqrt', 'pi',
  // Temporal
  'date', 'datetime', 'time', 'localtime', 'localdatetime', 'duration',
  // Path
  'shortestPath', 'allShortestPaths',
  // Utility
  'coalesce',
]

export function registerCypherLanguage(monaco: typeof Monaco) {
  // Register the language
  monaco.languages.register({ id: 'cypher' })

  // Tokenizer
  monaco.languages.setMonarchTokensProvider('cypher', {
    ignoreCase: true,
    keywords,
    operators,
    functions,
    tokenizer: {
      root: [
        // Comments
        [/\/\/.*$/, 'comment'],

        // Strings
        [/"([^"\\]|\\.)*"/, 'string'],
        [/'([^'\\]|\\.)*'/, 'string'],

        // Numbers
        [/\d+\.\d*/, 'number.float'],
        [/\d+/, 'number'],

        // Node labels :Label
        [/:([A-Za-z_]\w*)/, 'type.identifier'],

        // Relationship types in brackets [:TYPE]
        [/\[:[A-Za-z_]\w*/, 'type.identifier'],

        // Parameter $name
        [/\$[A-Za-z_]\w*/, 'variable'],

        // Identifiers / keywords
        [/[A-Za-z_]\w*/, {
          cases: {
            '@keywords': 'keyword',
            '@operators': 'keyword.operator',
            '@functions': 'predefined',
            '@default': 'identifier',
          },
        }],

        // Operators
        [/[<>]=?|<>|=~|=/, 'operator'],
        [/[+\-*/%^]/, 'operator'],
        [/\.\./, 'operator'],

        // Brackets
        [/[{}()\[\]]/, 'delimiter.bracket'],

        // Punctuation
        [/[,;.]/, 'delimiter'],

        // Whitespace
        [/\s+/, 'white'],
      ],
    },
  })

  // Completion provider
  monaco.languages.registerCompletionItemProvider('cypher', {
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

/**
 * Inject schema-aware completions (labels, types, properties).
 * When typing `var.`, resolves the variable's label from the query text
 * and only suggests properties for that label/type.
 * Disposes the previous provider on each call to avoid accumulation.
 */
let schemaDisposable: Monaco.IDisposable | null = null

export interface LabelProperties {
  name: string
  properties: { name: string }[]
}

/**
 * Scan the full query text for `(var:Label)` and `[var:TYPE]` bindings.
 * Returns a map from variable name -> label/type name.
 */
function parseVariableBindings(text: string): Map<string, string> {
  const bindings = new Map<string, string>()
  // Node patterns: (var:Label)
  const nodeRe = /\(\s*([A-Za-z_]\w*)\s*:\s*([A-Za-z_]\w*)/g
  let m: RegExpExecArray | null
  while ((m = nodeRe.exec(text)) !== null) {
    bindings.set(m[1], m[2])
  }
  // Relationship patterns: [var:TYPE]
  const relRe = /\[\s*([A-Za-z_]\w*)\s*:\s*([A-Za-z_]\w*)/g
  while ((m = relRe.exec(text)) !== null) {
    bindings.set(m[1], m[2])
  }
  return bindings
}

export function updateSchemaCompletions(
  monaco: typeof Monaco,
  labels: string[],
  relationshipTypes: string[],
  labelProps: LabelProperties[],
  edgeTypeProps: LabelProperties[],
  allPropertyKeys: string[],
) {
  schemaDisposable?.dispose()

  // Build lookup maps: label/type name -> property names
  const labelPropMap = new Map<string, string[]>()
  for (const lp of labelProps) {
    labelPropMap.set(lp.name, lp.properties.map(p => p.name))
  }
  const edgePropMap = new Map<string, string[]>()
  for (const ep of edgeTypeProps) {
    edgePropMap.set(ep.name, ep.properties.map(p => p.name))
  }

  schemaDisposable = monaco.languages.registerCompletionItemProvider('cypher', {
    triggerCharacters: [':', '.'],
    provideCompletionItems: (model, position) => {
      const word = model.getWordUntilPosition(position)
      const range = {
        startLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endLineNumber: position.lineNumber,
        endColumn: position.column,
      }

      const textUntilPosition = model.getValueInRange({
        startLineNumber: position.lineNumber,
        startColumn: 1,
        endLineNumber: position.lineNumber,
        endColumn: position.column,
      })

      const suggestions: Monaco.languages.CompletionItem[] = []

      // After colon - suggest labels and relationship types
      if (textUntilPosition.match(/:\s*$/)) {
        labels.forEach((label) => {
          suggestions.push({
            label,
            kind: monaco.languages.CompletionItemKind.Class,
            insertText: label,
            range,
          })
        })
        relationshipTypes.forEach((rt) => {
          suggestions.push({
            label: rt,
            kind: monaco.languages.CompletionItemKind.Enum,
            insertText: rt,
            range,
          })
        })
      }

      // After `variable.` - suggest properties scoped to the variable's label/type
      const dotMatch = textUntilPosition.match(/\b([A-Za-z_]\w*)\.\s*$/)
      if (dotMatch) {
        const varName = dotMatch[1]
        const fullText = model.getValue()
        const bindings = parseVariableBindings(fullText)
        const boundLabel = bindings.get(varName)

        if (boundLabel) {
          // Try label properties first, then edge type properties
          const props = labelPropMap.get(boundLabel) || edgePropMap.get(boundLabel)
          if (props) {
            props.forEach((pk) => {
              suggestions.push({
                label: pk,
                kind: monaco.languages.CompletionItemKind.Property,
                insertText: pk,
                range,
              })
            })
            return { suggestions }
          }
        }

        // Fallback: no binding found, suggest all properties
        allPropertyKeys.forEach((pk) => {
          suggestions.push({
            label: pk,
            kind: monaco.languages.CompletionItemKind.Property,
            insertText: pk,
            range,
          })
        })
      }

      return { suggestions }
    },
  })
}
