// -- Health ----------------------------------------------------------

/**
 * @typedef {Object} HealthDatabase
 * @property {string} name
 * @property {boolean} default
 * @property {number} nodes
 * @property {number} edges
 * @property {number} memoryBytes
 */

/**
 * @typedef {Object} HealthResponse
 * @property {string} status
 * @property {string} version
 * @property {number} uptimeSecs
 * @property {number} memoryBytes
 * @property {HealthDatabase[]} databases
 * @property {boolean} allowDropDatabase
 * @property {boolean} authRequired
 * @property {Object<string, boolean>} features Compile-time server
 *   capabilities (e.g. `{ gql: true, ml: false }`) - check
 *   `features.gql` before passing dialect `'gql'` to `query()`.
 */

// -- Databases -------------------------------------------------------

/**
 * @typedef {Object} DatabaseInfo
 * @property {string} name
 * @property {boolean} default
 * @property {boolean} readOnly
 * @property {string[]} aliases
 */

// -- Files -----------------------------------------------------------

/**
 * @typedef {Object} TvdbFileInfo
 * @property {string} name
 * @property {string} file
 * @property {number} bytes
 * @property {boolean} loaded
 * @property {string} [kind] 'database' for .tvdb files, 'model' for
 *   .tvmodel trained models.
 */

/**
 * @typedef {Object} UploadResult
 * @property {boolean} ok
 * @property {string} name
 * @property {string} file
 * @property {number} bytes
 */

// -- Aliases ---------------------------------------------------------

/**
 * @typedef {Object} AliasInfo
 * @property {string} alias
 * @property {string} database
 */

// -- Schema ----------------------------------------------------------

/**
 * @typedef {Object} SchemaProperty
 * @property {string} name
 * @property {string} type
 */

/**
 * @typedef {Object} IndexInfo
 * @property {string} name
 * @property {string} label
 * @property {string} property
 * @property {number} entries
 */

/**
 * @typedef {Object} ConstraintInfo
 * @property {string} name
 * @property {string} type
 * @property {string} label
 * @property {string} property
 */

/**
 * @typedef {Object} LabelDetail
 * @property {string} name
 * @property {SchemaProperty[]} properties
 */

/**
 * @typedef {Object} SchemaResponse
 * @property {string[]} labels
 * @property {string[]} relationshipTypes
 * @property {SchemaProperty[]} propertyKeys
 * @property {IndexInfo[]} indexes
 * @property {ConstraintInfo[]} constraints
 * @property {LabelDetail[]} labelsDetail
 * @property {LabelDetail[]} relationshipTypesDetail
 */

// -- Query -----------------------------------------------------------

/**
 * @typedef {Object} QueryStats
 * @property {number} nodesCreated
 * @property {number} nodesDeleted
 * @property {number} relationshipsCreated
 * @property {number} relationshipsDeleted
 * @property {number} propertiesSet
 * @property {number} labelsAdded
 * @property {number} labelsRemoved
 */

/**
 * @typedef {Object} QueryResponse
 * @property {string[]} columns
 * @property {Array<any[]>} rows
 * @property {number} totalRows
 * @property {boolean} truncated
 * @property {string} queryType
 * @property {QueryStats} stats
 * @property {number} timeMs
 * @property {number} parsingTimeMs
 * @property {number} planningTimeMs
 * @property {number} executionTimeMs
 */

// -- Import ----------------------------------------------------------

/**
 * @typedef {Object} ImportError
 * @property {string} statement
 * @property {string} error
 */

/**
 * @typedef {Object} ImportResult
 * @property {number} totalStatements
 * @property {number} executed
 * @property {ImportError[]} errors
 * @property {QueryStats} stats
 * @property {number} timeMs
 */

/**
 * @typedef {Object} CsvImportError
 * @property {number} row
 * @property {string} error
 */

/**
 * @typedef {Object} CsvImportResult
 * @property {number} nodesCreated
 * @property {number} propertiesSet
 * @property {number} labelsAdded
 * @property {CsvImportError[]} errors
 * @property {number} timeMs
 */

/**
 * @typedef {Object} CsvEdgeImportResult
 * @property {number} edgesCreated
 * @property {number} propertiesSet
 * @property {CsvImportError[]} errors
 * @property {number} timeMs
 */

// -- Import helpers --------------------------------------------------

/**
 * Column mapping for CSV import.
 */
export class ColumnMapping {
  /**
   * @param {string} name - CSV column name.
   * @param {string|null} [property=null] - Target property name.
   * @param {string|null} [type=null] - Property type (string, integer, float, boolean).
   */
  constructor(name, property = null, type = null) {
    this.name = name;
    this.property = property;
    this.type = type;
  }

  toJSON() {
    const d = { name: this.name };
    if (this.property != null) d.property = this.property;
    if (this.type != null) d.type = this.type;
    return d;
  }
}

/**
 * Edge endpoint for CSV edge import.
 */
export class EdgeEndpoint {
  /**
   * @param {string} label - Node label.
   * @param {string} column - CSV column name.
   * @param {string} property - Node property to match.
   */
  constructor(label, column, property) {
    this.label = label;
    this.column = column;
    this.property = property;
  }

  toJSON() {
    return { label: this.label, column: this.column, property: this.property };
  }
}

// -- Tasks -----------------------------------------------------------

/**
 * @typedef {Object} TaskInfo
 * @property {string} id
 * @property {string} operation
 * @property {string} status
 * @property {any} result
 * @property {string|null} error
 * @property {string} createdAt
 * @property {string|null} finishedAt
 */

// -- Users -----------------------------------------------------------

/**
 * @typedef {Object} UserInfo
 * @property {string} username
 * @property {string} role
 */

// -- Parsers ---------------------------------------------------------

/**
 * Parse raw health JSON into a HealthResponse.
 * @param {Object} d - Raw JSON object.
 * @returns {HealthResponse}
 */
export function parseHealth(d) {
  return {
    status: d.status ?? '',
    version: d.version ?? '',
    uptimeSecs: d.uptime_secs ?? 0,
    memoryBytes: d.memory_bytes ?? 0,
    databases: (d.databases ?? []).map((db) => ({
      name: db.name ?? '',
      default: db.default ?? false,
      nodes: db.nodes ?? 0,
      edges: db.edges ?? 0,
      memoryBytes: db.memory_bytes ?? 0,
    })),
    allowDropDatabase: d.allow_drop_database ?? false,
    authRequired: d.auth_required ?? false,
    // Compile-time server capabilities, e.g. { gql: true, ml: false }.
    // Check features.gql before passing dialect 'gql' to query().
    features: { ...(d.features ?? {}) },
  };
}

/**
 * Parse raw database info JSON.
 * @param {Object} d
 * @returns {DatabaseInfo}
 */
export function parseDatabaseInfo(d) {
  return {
    name: d.name ?? '',
    default: d.default ?? false,
    readOnly: d.readOnly ?? false,
    aliases: d.aliases ?? [],
  };
}

/**
 * Parse raw schema JSON.
 * @param {Object} d
 * @returns {SchemaResponse}
 */
export function parseSchema(d) {
  const parseLabelDetail = (ld) => ({
    name: ld.name ?? '',
    properties: (ld.properties ?? []).map((p) => ({ name: p.name ?? '', type: p.type ?? '' })),
  });
  return {
    labels: d.labels ?? [],
    relationshipTypes: d.relationship_types ?? [],
    propertyKeys: (d.property_keys ?? []).map((p) => ({ name: p.name ?? '', type: p.type ?? '' })),
    indexes: (d.indexes ?? []).map((i) => ({
      name: i.name ?? '',
      label: i.label ?? '',
      property: i.property ?? '',
      entries: i.entries ?? 0,
    })),
    constraints: (d.constraints ?? []).map((c) => ({
      name: c.name ?? '',
      type: c.type ?? '',
      label: c.label ?? '',
      property: c.property ?? '',
    })),
    labelsDetail: (d.labels_detail ?? []).map(parseLabelDetail),
    relationshipTypesDetail: (d.relationship_types_detail ?? []).map(parseLabelDetail),
  };
}

/**
 * Parse raw query response JSON.
 * @param {Object} d
 * @returns {QueryResponse}
 */
export function parseQueryResponse(d) {
  const s = d.stats ?? {};
  return {
    columns: d.columns ?? [],
    rows: d.rows ?? [],
    totalRows: d.total_rows ?? 0,
    truncated: d.truncated ?? false,
    queryType: d.query_type ?? '',
    stats: {
      nodesCreated: s.nodes_created ?? 0,
      nodesDeleted: s.nodes_deleted ?? 0,
      relationshipsCreated: s.relationships_created ?? 0,
      relationshipsDeleted: s.relationships_deleted ?? 0,
      propertiesSet: s.properties_set ?? 0,
      labelsAdded: s.labels_added ?? 0,
      labelsRemoved: s.labels_removed ?? 0,
    },
    timeMs: d.time_ms ?? 0,
    parsingTimeMs: d.parsing_time_ms ?? 0,
    planningTimeMs: d.planning_time_ms ?? 0,
    executionTimeMs: d.execution_time_ms ?? 0,
  };
}

/**
 * Parse raw import result JSON.
 * @param {Object} d
 * @returns {ImportResult}
 */
export function parseImportResult(d) {
  const s = d.stats ?? {};
  return {
    totalStatements: d.total_statements ?? 0,
    executed: d.executed ?? 0,
    errors: (d.errors ?? []).map((e) => ({ statement: e.statement ?? '', error: e.error ?? '' })),
    stats: {
      nodesCreated: s.nodes_created ?? 0,
      nodesDeleted: s.nodes_deleted ?? 0,
      relationshipsCreated: s.relationships_created ?? 0,
      relationshipsDeleted: s.relationships_deleted ?? 0,
      propertiesSet: s.properties_set ?? 0,
      labelsAdded: s.labels_added ?? 0,
      labelsRemoved: s.labels_removed ?? 0,
    },
    timeMs: d.time_ms ?? 0,
  };
}

/**
 * Parse raw CSV import result JSON.
 * @param {Object} d
 * @returns {CsvImportResult}
 */
export function parseCsvImportResult(d) {
  return {
    nodesCreated: d.nodes_created ?? 0,
    propertiesSet: d.properties_set ?? 0,
    labelsAdded: d.labels_added ?? 0,
    errors: (d.errors ?? []).map((e) => ({ row: e.row ?? 0, error: e.error ?? '' })),
    timeMs: d.time_ms ?? 0,
  };
}

/**
 * Parse raw CSV edge import result JSON.
 * @param {Object} d
 * @returns {CsvEdgeImportResult}
 */
export function parseCsvEdgeImportResult(d) {
  return {
    edgesCreated: d.edges_created ?? 0,
    propertiesSet: d.properties_set ?? 0,
    errors: (d.errors ?? []).map((e) => ({ row: e.row ?? 0, error: e.error ?? '' })),
    timeMs: d.time_ms ?? 0,
  };
}

/**
 * Parse raw task info JSON.
 * @param {Object} d
 * @returns {TaskInfo}
 */
export function parseTaskInfo(d) {
  return {
    id: d.id ?? '',
    operation: d.operation ?? '',
    status: d.status ?? '',
    result: d.result ?? null,
    error: d.error ?? null,
    createdAt: d.createdAt ?? '',
    finishedAt: d.finishedAt ?? null,
  };
}
