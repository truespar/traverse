import { TraverseHttpError } from './errors.js';
import {
  parseHealth,
  parseDatabaseInfo,
  parseSchema,
  parseQueryResponse,
  parseImportResult,
  parseCsvImportResult,
  parseCsvEdgeImportResult,
  parseTaskInfo,
} from './models.js';

/**
 * Query-string suffix for /api/files kind routing. The default
 * 'database' is omitted so requests stay byte-identical to
 * pre-`kind` clients.
 * @param {string} kind
 * @returns {string}
 */
function kindQuery(kind) {
  return kind === 'database' ? '' : `?kind=${encodeURIComponent(kind)}`;
}

/**
 * HTTP client for the Traverse server management API.
 */
export class TraverseHttpClient {
  /**
   * @param {string} baseUrl - Base URL of the Traverse server (e.g. "http://localhost:7691").
   * @param {Object} [options]
   * @param {string|null} [options.apiKey=null] - API key for authentication.
   * @param {number} [options.timeout=100000] - Default request timeout in ms.
   * @param {number} [options.transferTimeout=3600000] - Timeout for file transfers in ms.
   * @param {boolean} [options.allowUntrustedCertificate=false] - Skip TLS verification.
   */
  constructor(baseUrl, options = {}) {
    const normalized = baseUrl.endsWith('/') ? baseUrl : baseUrl + '/';
    this._baseUrl = normalized;
    this._apiKey = options.apiKey ?? null;
    this._timeout = options.timeout ?? 100_000;
    this._transferTimeout = options.transferTimeout ?? 3_600_000;

    if (options.allowUntrustedCertificate) {
      process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';
    }
  }

  // -- Health ----------------------------------------------------------

  /**
   * Get server health information.
   * @returns {Promise<import('./models.js').HealthResponse>}
   */
  async health() {
    const data = await this._get('api/health');
    return parseHealth(data);
  }

  /**
   * Check if the server is ready.
   * @returns {Promise<boolean>}
   */
  async ready() {
    try {
      const resp = await fetch(this._url('api/ready'), {
        signal: AbortSignal.timeout(this._timeout),
      });
      return resp.status >= 200 && resp.status < 300;
    } catch {
      return false;
    }
  }

  // -- Databases -------------------------------------------------------

  /**
   * List all databases.
   * @returns {Promise<import('./models.js').DatabaseInfo[]>}
   */
  async listDatabases() {
    const data = await this._get('api/databases');
    return data.map(parseDatabaseInfo);
  }

  /**
   * Create a new database.
   * @param {string} name
   */
  async createDatabase(name) {
    await this._post('api/databases', { name });
  }

  /**
   * Drop a database (async task, polls until complete).
   * @param {string} name
   */
  async dropDatabase(name) {
    await this._pollTask(
      await this._delete(`api/databases/${encodeURIComponent(name)}?async=true`)
    );
  }

  /**
   * Rename a database.
   * @param {string} oldName
   * @param {string} newName
   */
  async renameDatabase(oldName, newName) {
    await this._post(`api/databases/${encodeURIComponent(oldName)}/rename`, { name: newName });
  }

  /**
   * Unload a database to disk (async task, polls until complete).
   * @param {string} name
   */
  async unloadDatabase(name) {
    await this._pollTask(
      await this._postRaw(
        `api/databases/${encodeURIComponent(name)}/unload?async=true`,
        undefined,
        this._transferTimeout
      )
    );
  }

  /**
   * Load a database from a file (async task, polls until complete).
   * @param {string} name
   * @param {string} file
   */
  async loadDatabase(name, file) {
    await this._pollTask(
      await this._postRaw(
        'api/databases/load?async=true',
        { name, file },
        this._transferTimeout
      )
    );
  }

  // -- Files -----------------------------------------------------------

  /**
   * List all tvdb files.
   * @returns {Promise<import('./models.js').TvdbFileInfo[]>}
   */
  async listFiles() {
    return await this._get('api/files');
  }

  /**
   * Upload a .tvdb database (kind 'database', the default) or a
   * .tvmodel trained model (kind 'model' - requires a server built
   * with the ml feature).
   * @param {string} name
   * @param {Buffer|Uint8Array} data
   * @param {string} [kind='database']
   * @returns {Promise<import('./models.js').UploadResult>}
   */
  async uploadFile(name, data, kind = 'database') {
    const resp = await fetch(this._url(`api/files/${encodeURIComponent(name)}${kindQuery(kind)}`), {
      method: 'PUT',
      headers: this._headers({ 'Content-Type': 'application/octet-stream' }),
      body: data,
      signal: AbortSignal.timeout(this._transferTimeout),
    });
    await this._ensureSuccess(resp);
    return await resp.json();
  }

  /**
   * Download a .tvdb database or (kind 'model') a .tvmodel file.
   * @param {string} name
   * @param {string} [kind='database']
   * @returns {Promise<Buffer>}
   */
  async downloadFile(name, kind = 'database') {
    const resp = await fetch(this._url(`api/files/${encodeURIComponent(name)}${kindQuery(kind)}`), {
      method: 'GET',
      headers: this._headers(),
      signal: AbortSignal.timeout(this._transferTimeout),
    });
    await this._ensureSuccess(resp);
    const arrayBuf = await resp.arrayBuffer();
    return Buffer.from(arrayBuf);
  }

  /**
   * Delete a .tvdb database or (kind 'model') a .tvmodel file.
   * @param {string} name
   * @param {string} [kind='database']
   */
  async deleteFile(name, kind = 'database') {
    await this._delete(`api/files/${encodeURIComponent(name)}${kindQuery(kind)}`);
  }

  // -- Aliases ---------------------------------------------------------

  /**
   * List all aliases.
   * @returns {Promise<import('./models.js').AliasInfo[]>}
   */
  async listAliases() {
    return await this._get('api/aliases');
  }

  /**
   * Create an alias.
   * @param {string} alias
   * @param {string} database
   */
  async createAlias(alias, database) {
    await this._post('api/aliases', { alias, database });
  }

  /**
   * Alter an alias to point to a different database.
   * @param {string} alias
   * @param {string} database
   */
  async alterAlias(alias, database) {
    await this._put(`api/aliases/${encodeURIComponent(alias)}`, { database });
  }

  /**
   * Drop an alias.
   * @param {string} alias
   */
  async dropAlias(alias) {
    await this._delete(`api/aliases/${encodeURIComponent(alias)}`);
  }

  // -- Schema ----------------------------------------------------------

  /**
   * Get database schema.
   * @param {string|null} [database=null]
   * @returns {Promise<import('./models.js').SchemaResponse>}
   */
  async getSchema(database = null) {
    let path = 'api/schema';
    if (database != null) {
      path += `?database=${encodeURIComponent(database)}`;
    }
    const data = await this._get(path);
    return parseSchema(data);
  }

  // -- Query -----------------------------------------------------------

  /**
   * Execute a query.
   * @param {string} cypher
   * @param {Object|null} [parameters=null]
   * @param {string|null} [database=null]
   * @param {string|null} [dialect=null] `null` (openCypher, the
   *   default) or `'gql'` for ISO/IEC 39075 GQL - requires a server
   *   built with the gql feature (see `health()` `features`).
   * @returns {Promise<import('./models.js').QueryResponse>}
   */
  async query(cypher, parameters = null, database = null, dialect = null) {
    const body = { query: cypher };
    if (parameters != null) body.parameters = parameters;
    if (database != null) body.database = database;
    // dialect: null (openCypher, the default) or 'gql' for ISO/IEC
    // 39075 GQL - requires a server built with the gql feature; check
    // (await health()).features.gql.
    if (dialect != null) body.dialect = dialect;
    const data = await this._post('api/query', body);
    return parseQueryResponse(data);
  }

  // -- Import ----------------------------------------------------------

  /**
   * Import Cypher statements.
   * @param {string} cypher
   * @param {string|null} [database=null]
   * @returns {Promise<import('./models.js').ImportResult>}
   */
  async importCypher(cypher, database = null) {
    const body = { cypher };
    if (database != null) body.database = database;
    const data = await this._post('api/import', body);
    return parseImportResult(data);
  }

  /**
   * Import nodes from CSV data.
   * @param {string} csv
   * @param {string} label
   * @param {import('./models.js').ColumnMapping[]} columns
   * @param {Object} [options]
   * @param {string|null} [options.database=null]
   * @param {string|null} [options.delimiter=null]
   * @param {boolean|null} [options.skipHeader=null]
   * @returns {Promise<import('./models.js').CsvImportResult>}
   */
  async importCsv(csv, label, columns, options = {}) {
    const body = {
      csv,
      label,
      columns: columns.map((c) => (typeof c.toJSON === 'function' ? c.toJSON() : c)),
    };
    if (options.database != null) body.database = options.database;
    if (options.delimiter != null) body.delimiter = options.delimiter;
    if (options.skipHeader != null) body.skip_header = options.skipHeader;
    const data = await this._post('api/import/csv', body);
    return parseCsvImportResult(data);
  }

  /**
   * Import edges from CSV data.
   * @param {string} csv
   * @param {string} edgeType
   * @param {import('./models.js').EdgeEndpoint} source
   * @param {import('./models.js').EdgeEndpoint} target
   * @param {Object} [options]
   * @param {import('./models.js').ColumnMapping[]|null} [options.columns=null]
   * @param {string|null} [options.database=null]
   * @param {string|null} [options.delimiter=null]
   * @param {boolean|null} [options.skipHeader=null]
   * @returns {Promise<import('./models.js').CsvEdgeImportResult>}
   */
  async importCsvEdges(csv, edgeType, source, target, options = {}) {
    const body = {
      csv,
      edge_type: edgeType,
      source: typeof source.toJSON === 'function' ? source.toJSON() : source,
      target: typeof target.toJSON === 'function' ? target.toJSON() : target,
    };
    if (options.columns != null) {
      body.columns = options.columns.map((c) => (typeof c.toJSON === 'function' ? c.toJSON() : c));
    }
    if (options.database != null) body.database = options.database;
    if (options.delimiter != null) body.delimiter = options.delimiter;
    if (options.skipHeader != null) body.skip_header = options.skipHeader;
    const data = await this._post('api/import/csv/edges', body);
    return parseCsvEdgeImportResult(data);
  }

  // -- Users -----------------------------------------------------------

  /**
   * List all users.
   * @returns {Promise<import('./models.js').UserInfo[]>}
   */
  async listUsers() {
    return await this._get('api/users');
  }

  /**
   * Create a new user.
   * @param {string} username
   * @param {string} password
   * @param {string} role
   */
  async createUser(username, password, role) {
    await this._post('api/users', { username, password, role });
  }

  /**
   * Update a user.
   * @param {string} username
   * @param {Object} [options]
   * @param {string|null} [options.password=null]
   * @param {string|null} [options.role=null]
   */
  async updateUser(username, options = {}) {
    const body = {};
    if (options.password != null) body.password = options.password;
    if (options.role != null) body.role = options.role;
    await this._put(`api/users/${encodeURIComponent(username)}`, body);
  }

  /**
   * Delete a user.
   * @param {string} username
   */
  async deleteUser(username) {
    await this._delete(`api/users/${encodeURIComponent(username)}`);
  }

  // -- Server operations -----------------------------------------------

  /**
   * Save all databases (async task, polls until complete).
   */
  async save() {
    await this._pollTask(
      await this._postRaw('api/save?async=true', undefined, this._transferTimeout)
    );
  }

  // -- Tasks -----------------------------------------------------------

  /**
   * Get task status.
   * @param {string} taskId
   * @returns {Promise<import('./models.js').TaskInfo>}
   */
  async getTask(taskId) {
    const data = await this._get(`api/tasks/${encodeURIComponent(taskId)}`);
    return parseTaskInfo(data);
  }

  // -- Internal HTTP helpers -------------------------------------------

  /**
   * Build the full URL for a path.
   * @param {string} path
   * @returns {string}
   */
  _url(path) {
    return this._baseUrl + path;
  }

  /**
   * Build headers with optional auth and additional headers.
   * @param {Object} [extra={}]
   * @returns {Object}
   */
  _headers(extra = {}) {
    const h = { ...extra };
    if (this._apiKey) {
      h['Authorization'] = `Bearer ${this._apiKey}`;
    }
    return h;
  }

  /**
   * Ensure the response is successful (2xx), throw TraverseHttpError otherwise.
   * @param {Response} resp
   */
  async _ensureSuccess(resp) {
    if (resp.status >= 200 && resp.status < 300) return;
    let errorMessage = null;
    try {
      const data = await resp.json();
      errorMessage = data.error;
    } catch {
      // ignore parse errors
    }
    throw new TraverseHttpError(
      errorMessage || `HTTP ${resp.status}`,
      resp.status
    );
  }

  /**
   * GET request, return parsed JSON.
   * @param {string} path
   * @returns {Promise<any>}
   */
  async _get(path) {
    const resp = await fetch(this._url(path), {
      method: 'GET',
      headers: this._headers(),
      signal: AbortSignal.timeout(this._timeout),
    });
    await this._ensureSuccess(resp);
    return await resp.json();
  }

  /**
   * POST request with JSON body, return parsed JSON (or null if empty).
   * @param {string} path
   * @param {any} [body=null]
   * @returns {Promise<any>}
   */
  async _post(path, body = null) {
    const opts = {
      method: 'POST',
      headers: this._headers({ 'Content-Type': 'application/json' }),
      signal: AbortSignal.timeout(this._timeout),
    };
    if (body != null) {
      opts.body = JSON.stringify(body);
    }
    const resp = await fetch(this._url(path), opts);
    await this._ensureSuccess(resp);
    const text = await resp.text();
    if (text) {
      return JSON.parse(text);
    }
    return null;
  }

  /**
   * POST request, return raw response text.
   * @param {string} path
   * @param {any} [body]
   * @param {number} [timeout]
   * @returns {Promise<string>}
   */
  async _postRaw(path, body, timeout) {
    const opts = {
      method: 'POST',
      headers: this._headers({ 'Content-Type': 'application/json' }),
      signal: AbortSignal.timeout(timeout ?? this._timeout),
    };
    if (body != null) {
      opts.body = JSON.stringify(body);
    }
    const resp = await fetch(this._url(path), opts);
    await this._ensureSuccess(resp);
    return await resp.text();
  }

  /**
   * PUT request with JSON body.
   * @param {string} path
   * @param {any} body
   */
  async _put(path, body) {
    const resp = await fetch(this._url(path), {
      method: 'PUT',
      headers: this._headers({ 'Content-Type': 'application/json' }),
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(this._timeout),
    });
    await this._ensureSuccess(resp);
  }

  /**
   * DELETE request, return raw response text.
   * @param {string} path
   * @returns {Promise<string>}
   */
  async _delete(path) {
    const resp = await fetch(this._url(path), {
      method: 'DELETE',
      headers: this._headers(),
      signal: AbortSignal.timeout(this._timeout),
    });
    await this._ensureSuccess(resp);
    return await resp.text();
  }

  /**
   * Poll a task until completed or failed.
   * @param {string} responseBody - Raw response body containing { taskId }.
   */
  async _pollTask(responseBody) {
    if (!responseBody || !responseBody.trim()) return;
    let data;
    try {
      data = JSON.parse(responseBody);
    } catch {
      return;
    }
    const taskId = data.taskId;
    if (!taskId) return;

    let delay = 250;
    while (true) {
      await new Promise((resolve) => setTimeout(resolve, delay));
      const task = await this.getTask(taskId);
      if (task.status === 'completed') return;
      if (task.status === 'failed') {
        throw new TraverseHttpError(task.error || 'task failed');
      }
      delay = Math.min(delay * 2, 2000);
    }
  }
}
