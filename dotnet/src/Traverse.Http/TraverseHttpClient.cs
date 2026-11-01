using System.Net.Http.Json;
using System.Net.Security;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Traverse.Http;

/// <summary>
/// HTTP client for the Traverse server management API.
/// Covers databases, files, aliases, schema, users, and server operations.
/// </summary>
public sealed class TraverseHttpClient : IDisposable
{
    private readonly HttpClient _http;
    private readonly bool _ownsClient;
    private readonly TimeSpan _apiTimeout;
    private readonly TimeSpan _transferTimeout;

    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    /// <summary>
    /// Creates a new client for the given server base URL.
    /// </summary>
    /// <param name="baseUrl">Server URL, e.g. "http://localhost:7691" or "https://host:7691".</param>
    /// <param name="options">Optional client configuration.</param>
    public TraverseHttpClient(string baseUrl, TraverseHttpOptions? options = null)
    {
        options ??= new TraverseHttpOptions();

        HttpMessageHandler handler;
        if (options.AllowUntrustedCertificate)
        {
            handler = new SocketsHttpHandler
            {
                SslOptions = new SslClientAuthenticationOptions
                {
#pragma warning disable CA5359 // AllowUntrustedCertificate opts into self-signed servers
                    RemoteCertificateValidationCallback = (_, _, _, _) => true,
#pragma warning restore CA5359
                },
            };
        }
        else
        {
            handler = new SocketsHttpHandler();
        }

        _http = new HttpClient(handler) { BaseAddress = new Uri(baseUrl.TrimEnd('/') + "/") };
        _ownsClient = true;

        // Use infinite timeout on HttpClient; we manage per-request timeouts via CTS.
        _http.Timeout = Timeout.InfiniteTimeSpan;
        _apiTimeout = options.Timeout ?? TimeSpan.FromSeconds(100);
        _transferTimeout = options.FileTransferTimeout ?? Timeout.InfiniteTimeSpan;

        if (options.ApiKey is not null)
            _http.DefaultRequestHeaders.Authorization =
                new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", options.ApiKey);
    }

    /// <summary>
    /// Creates a client wrapping an existing <see cref="HttpClient"/>.
    /// The caller retains ownership of the HttpClient.
    /// </summary>
    public TraverseHttpClient(HttpClient httpClient)
    {
        _http = httpClient;
        _ownsClient = false;
        _apiTimeout = httpClient.Timeout;
        _transferTimeout = Timeout.InfiniteTimeSpan;
    }

    public void Dispose()
    {
        if (_ownsClient) _http.Dispose();
    }

    // -- Timeout helpers ---------------------------------------------

    private static CancellationTokenSource? CreateTimeoutCts(TimeSpan timeout, CancellationToken ct)
    {
        if (timeout == Timeout.InfiniteTimeSpan) return null;
        var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        cts.CancelAfter(timeout);
        return cts;
    }

    private CancellationToken ApiToken(out CancellationTokenSource? cts, CancellationToken ct)
    {
        cts = CreateTimeoutCts(_apiTimeout, ct);
        return cts?.Token ?? ct;
    }

    private CancellationToken TransferToken(out CancellationTokenSource? cts, CancellationToken ct)
    {
        cts = CreateTimeoutCts(_transferTimeout, ct);
        return cts?.Token ?? ct;
    }

    // -- Health ------------------------------------------------------

    /// <summary>Returns server health, version, memory, and database summary.</summary>
    public async Task<HealthResponse> HealthAsync(CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync("api/health", token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<HealthResponse>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from health");
    }

    /// <summary>Returns true if the server is ready.</summary>
    public async Task<bool> ReadyAsync(CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync("api/ready", token);
        return resp.IsSuccessStatusCode;
    }

    // -- Databases (in-memory) ---------------------------------------

    /// <summary>Lists all loaded databases.</summary>
    public async Task<DatabaseInfo[]> ListDatabasesAsync(CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync("api/databases", token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<DatabaseInfo[]>(JsonOptions, token) ?? [];
    }

    /// <summary>Creates a new empty database.</summary>
    public async Task CreateDatabaseAsync(string name, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsJsonAsync("api/databases", new { name }, JsonOptions, token);
        await EnsureSuccess(resp);
    }

    /// <summary>
    /// Drops a loaded database and deletes its .tvdb file.
    /// Uses async task mode to avoid timeout on large databases.
    /// </summary>
    public async Task DropDatabaseAsync(string name, IProgress<TaskInfo>? progress = null, CancellationToken ct = default)
    {
        var token = TransferToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.DeleteAsync($"api/databases/{Uri.EscapeDataString(name)}?async=true", token);
        await EnsureSuccess(resp);
        var taskId = await ReadTaskId(resp, token);
        if (taskId != null)
        {
            var result = await PollTaskToCompletionAsync(taskId, progress, token);
            if (result.Status == "failed")
                throw new TraverseHttpException(result.Error ?? "drop failed");
        }
    }

    /// <summary>Renames a loaded database (also renames the .tvdb file on disk).</summary>
    public async Task RenameDatabaseAsync(string oldName, string newName, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsJsonAsync(
            $"api/databases/{Uri.EscapeDataString(oldName)}/rename",
            new { name = newName }, JsonOptions, token);
        await EnsureSuccess(resp);
    }

    /// <summary>
    /// Unloads a database from memory, persisting unsaved changes to disk first.
    /// The .tvdb file is preserved and can be re-loaded with <see cref="LoadDatabaseAsync"/>.
    /// Uses async task mode to avoid timeout on large databases.
    /// </summary>
    public async Task UnloadDatabaseAsync(string name, IProgress<TaskInfo>? progress = null, CancellationToken ct = default)
    {
        var token = TransferToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsync($"api/databases/{Uri.EscapeDataString(name)}/unload?async=true", null, token);
        await EnsureSuccess(resp);
        var taskId = await ReadTaskId(resp, token);
        if (taskId != null)
        {
            var result = await PollTaskToCompletionAsync(taskId, progress, token);
            if (result.Status == "failed")
                throw new TraverseHttpException(result.Error ?? "unload failed");
        }
    }

    /// <summary>
    /// Loads a .tvdb file from the data directory into memory.
    /// Uses async task mode to avoid timeout on large databases.
    /// </summary>
    public async Task LoadDatabaseAsync(string name, string file, IProgress<TaskInfo>? progress = null, CancellationToken ct = default)
    {
        var token = TransferToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsJsonAsync("api/databases/load?async=true", new { name, file }, JsonOptions, token);
        await EnsureSuccess(resp);
        var taskId = await ReadTaskId(resp, token);
        if (taskId != null)
        {
            var result = await PollTaskToCompletionAsync(taskId, progress, token);
            if (result.Status == "failed")
                throw new TraverseHttpException(result.Error ?? "load failed");
        }
    }

    // -- Files (on-disk .tvdb) ---------------------------------------

    /// <summary>Lists all .tvdb files in the data directory.</summary>
    public async Task<TvdbFileInfo[]> ListFilesAsync(CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync("api/files", token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<TvdbFileInfo[]>(JsonOptions, token) ?? [];
    }

    /// <summary>
    /// Uploads a .tvdb file to the server's data directory.
    /// The file is written to disk but NOT loaded into memory.
    /// Use <see cref="LoadDatabaseAsync"/> to load it afterwards.
    /// </summary>
    /// <param name="name">Database name (becomes {name}.tvdb on disk).</param>
    /// <param name="stream">Stream containing the .tvdb file bytes.</param>
    /// <param name="progress">Optional progress reporter receiving cumulative bytes uploaded.</param>
    /// <param name="kind">"database" (default) for a .tvdb file, or "model" for a
    /// .tvmodel trained model - requires a server built with the ml feature.</param>
    /// <param name="ct">Cancellation token.</param>
    public async Task<UploadResult> UploadFileAsync(
        string name, Stream stream, IProgress<long>? progress = null,
        string kind = "database", CancellationToken ct = default)
    {
        var token = TransferToken(out var cts, ct);
        using var _ = cts;

        Stream source = progress is not null ? new ProgressStream(stream, progress) : stream;
        using var content = new StreamContent(source, bufferSize: 81920);
        content.Headers.ContentType = new System.Net.Http.Headers.MediaTypeHeaderValue("application/octet-stream");
        var resp = await _http.PutAsync($"api/files/{Uri.EscapeDataString(name)}{KindQuery(kind)}", content, token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<UploadResult>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from upload");
    }

    /// <summary>
    /// Downloads a .tvdb file from the server to the given stream.
    /// Works for both loaded and unloaded databases.
    /// </summary>
    /// <param name="name">Database name (downloads {name}.tvdb).</param>
    /// <param name="destination">Stream to write the file contents to.</param>
    /// <param name="progress">Optional progress reporter receiving cumulative bytes downloaded.</param>
    /// <param name="ct">Cancellation token.</param>
    public async Task DownloadFileAsync(
        string name, Stream destination, IProgress<long>? progress = null,
        string kind = "database", CancellationToken ct = default)
    {
        var token = TransferToken(out var cts, ct);
        using var _ = cts;

        var resp = await _http.GetAsync($"api/files/{Uri.EscapeDataString(name)}{KindQuery(kind)}",
            HttpCompletionOption.ResponseHeadersRead, token);
        await EnsureSuccess(resp);

        if (progress is null)
        {
            await resp.Content.CopyToAsync(destination, token);
        }
        else
        {
            var buffer = new byte[81920];
            await using var source = await resp.Content.ReadAsStreamAsync(token);
            long totalRead = 0;
            int bytesRead;
            while ((bytesRead = await source.ReadAsync(buffer, token)) > 0)
            {
                await destination.WriteAsync(buffer.AsMemory(0, bytesRead), token);
                totalRead += bytesRead;
                progress.Report(totalRead);
            }
        }
    }

    /// <summary>
    /// Deletes a .tvdb file from the data directory.
    /// Rejects if the database is currently loaded (drop it first).
    /// </summary>
    public async Task DeleteFileAsync(string name, string kind = "database", CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.DeleteAsync($"api/files/{Uri.EscapeDataString(name)}{KindQuery(kind)}", token);
        await EnsureSuccess(resp);
    }

    /// <summary>
    /// Query-string suffix for /api/files kind routing. The default
    /// "database" is omitted so requests stay byte-identical to
    /// pre-kind clients.
    /// </summary>
    private static string KindQuery(string kind) =>
        kind is "" or "database" ? "" : $"?kind={Uri.EscapeDataString(kind)}";

    // -- Aliases -----------------------------------------------------

    /// <summary>Lists all database aliases.</summary>
    public async Task<AliasInfo[]> ListAliasesAsync(CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync("api/aliases", token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<AliasInfo[]>(JsonOptions, token) ?? [];
    }

    /// <summary>Creates an alias pointing to a database.</summary>
    public async Task CreateAliasAsync(string alias, string database, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsJsonAsync("api/aliases", new { alias, database }, JsonOptions, token);
        await EnsureSuccess(resp);
    }

    /// <summary>Changes the target database for an existing alias.</summary>
    public async Task AlterAliasAsync(string alias, string database, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PutAsJsonAsync(
            $"api/aliases/{Uri.EscapeDataString(alias)}",
            new { database }, JsonOptions, token);
        await EnsureSuccess(resp);
    }

    /// <summary>Deletes an alias.</summary>
    public async Task DropAliasAsync(string alias, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.DeleteAsync($"api/aliases/{Uri.EscapeDataString(alias)}", token);
        await EnsureSuccess(resp);
    }

    // -- Schema ------------------------------------------------------

    /// <summary>Returns the schema (labels, types, indexes, constraints) for the current database.</summary>
    public async Task<SchemaResponse> GetSchemaAsync(string? database = null, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var url = database is not null ? $"api/schema?database={Uri.EscapeDataString(database)}" : "api/schema";
        var resp = await _http.GetAsync(url, token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<SchemaResponse>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from schema");
    }

    // -- Query -------------------------------------------------------

    /// <summary>Executes a Cypher query and returns the result.</summary>
    /// <param name="cypher">Cypher query string.</param>
    /// <param name="parameters">Optional query parameters (values must be JSON-serializable).</param>
    /// <param name="database">Optional database name (uses server default if null).</param>
    /// <param name="ct">Cancellation token.</param>
    /// <summary>
    /// Runs a query. <paramref name="dialect"/> selects the query
    /// language: null (openCypher, the default) or "gql" for ISO/IEC
    /// 39075 GQL - requires a server built with the gql feature; check
    /// <see cref="HealthResponse.Features"/>.
    /// </summary>
    public async Task<QueryResponse> QueryAsync(
        string cypher,
        Dictionary<string, object?>? parameters = null,
        string? database = null,
        string? dialect = null,
        CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var body = new Dictionary<string, object?> { ["query"] = cypher };
        if (parameters is not null) body["parameters"] = parameters;
        if (database is not null) body["database"] = database;
        if (dialect is not null) body["dialect"] = dialect;
        var resp = await _http.PostAsJsonAsync("api/query", body, JsonOptions, token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<QueryResponse>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from query");
    }

    // -- Import ------------------------------------------------------

    /// <summary>Imports Cypher statements (one per line).</summary>
    public async Task<ImportResult> ImportCypherAsync(string cypher, string? database = null, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var body = new Dictionary<string, string> { ["cypher"] = cypher };
        if (database is not null) body["database"] = database;
        var resp = await _http.PostAsJsonAsync("api/import", body, JsonOptions, token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<ImportResult>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from import");
    }

    /// <summary>Imports nodes from CSV data.</summary>
    /// <param name="csv">CSV data as a string.</param>
    /// <param name="label">Node label to apply.</param>
    /// <param name="columns">Column-to-property mappings.</param>
    /// <param name="database">Optional database name.</param>
    /// <param name="delimiter">CSV delimiter: "," (default), "\t", "tab", ";", "|".</param>
    /// <param name="skipHeader">Whether to skip the first row as headers (default true).</param>
    /// <param name="ct">Cancellation token.</param>
    public async Task<CsvImportResult> ImportCsvAsync(
        string csv,
        string label,
        CsvColumnMapping[] columns,
        string? database = null,
        string? delimiter = null,
        bool? skipHeader = null,
        CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var body = new Dictionary<string, object?> { ["csv"] = csv, ["label"] = label, ["columns"] = columns };
        if (database is not null) body["database"] = database;
        if (delimiter is not null) body["delimiter"] = delimiter;
        if (skipHeader is not null) body["skip_header"] = skipHeader;
        var resp = await _http.PostAsJsonAsync("api/import/csv", body, JsonOptions, token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<CsvImportResult>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from CSV import");
    }

    /// <summary>Imports edges from CSV data.</summary>
    /// <param name="csv">CSV data as a string.</param>
    /// <param name="edgeType">Relationship type for created edges.</param>
    /// <param name="source">Source node endpoint mapping.</param>
    /// <param name="target">Target node endpoint mapping.</param>
    /// <param name="columns">Optional additional edge property mappings.</param>
    /// <param name="database">Optional database name.</param>
    /// <param name="delimiter">CSV delimiter: "," (default), "\t", "tab", ";", "|".</param>
    /// <param name="skipHeader">Whether to skip the first row as headers (default true).</param>
    /// <param name="ct">Cancellation token.</param>
    public async Task<CsvEdgeImportResult> ImportCsvEdgesAsync(
        string csv,
        string edgeType,
        CsvEdgeEndpoint source,
        CsvEdgeEndpoint target,
        CsvColumnMapping[]? columns = null,
        string? database = null,
        string? delimiter = null,
        bool? skipHeader = null,
        CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var body = new Dictionary<string, object?>
        {
            ["csv"] = csv,
            ["edge_type"] = edgeType,
            ["source"] = source,
            ["target"] = target,
        };
        if (columns is not null) body["columns"] = columns;
        if (database is not null) body["database"] = database;
        if (delimiter is not null) body["delimiter"] = delimiter;
        if (skipHeader is not null) body["skip_header"] = skipHeader;
        var resp = await _http.PostAsJsonAsync("api/import/csv/edges", body, JsonOptions, token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<CsvEdgeImportResult>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from CSV edge import");
    }

    // -- Users -------------------------------------------------------

    /// <summary>Lists all users.</summary>
    public async Task<UserInfo[]> ListUsersAsync(CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync("api/users", token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<UserInfo[]>(JsonOptions, token) ?? [];
    }

    /// <summary>Creates a new user.</summary>
    public async Task CreateUserAsync(string username, string password, string role = "reader", CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsJsonAsync("api/users", new { username, password, role }, JsonOptions, token);
        await EnsureSuccess(resp);
    }

    /// <summary>Updates user password and/or role.</summary>
    public async Task UpdateUserAsync(string username, string? password = null, string? role = null, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var body = new Dictionary<string, string?>();
        if (password is not null) body["password"] = password;
        if (role is not null) body["role"] = role;
        var resp = await _http.PutAsJsonAsync($"api/users/{Uri.EscapeDataString(username)}", body, JsonOptions, token);
        await EnsureSuccess(resp);
    }

    /// <summary>Deletes a user.</summary>
    public async Task DeleteUserAsync(string username, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.DeleteAsync($"api/users/{Uri.EscapeDataString(username)}", token);
        await EnsureSuccess(resp);
    }

    // -- Server operations -------------------------------------------

    /// <summary>
    /// Persists all databases to disk immediately.
    /// Uses async task mode to avoid timeout on large databases.
    /// </summary>
    public async Task SaveAsync(IProgress<TaskInfo>? progress = null, CancellationToken ct = default)
    {
        var token = TransferToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.PostAsync("api/save?async=true", null, token);
        await EnsureSuccess(resp);
        var taskId = await ReadTaskId(resp, token);
        if (taskId != null)
        {
            var result = await PollTaskToCompletionAsync(taskId, progress, token);
            if (result.Status == "failed")
                throw new TraverseHttpException(result.Error ?? "save failed");
        }
    }

    // -- Tasks (async operations) ----------------------------------

    /// <summary>Gets the status of an async server task.</summary>
    public async Task<TaskInfo> GetTaskAsync(string taskId, CancellationToken ct = default)
    {
        var token = ApiToken(out var cts, ct);
        using var _ = cts;
        var resp = await _http.GetAsync($"api/tasks/{Uri.EscapeDataString(taskId)}", token);
        await EnsureSuccess(resp);
        return await resp.Content.ReadFromJsonAsync<TaskInfo>(JsonOptions, token)
            ?? throw new TraverseHttpException("Empty response from task status");
    }

    /// <summary>
    /// Polls a task until it reaches "completed" or "failed" status.
    /// Uses exponential backoff: 250ms -> 500ms -> 1s -> 2s (cap).
    /// </summary>
    private async Task<TaskInfo> PollTaskToCompletionAsync(string taskId, IProgress<TaskInfo>? progress, CancellationToken ct)
    {
        int delayMs = 250;
        while (true)
        {
            ct.ThrowIfCancellationRequested();
            await Task.Delay(delayMs, ct);

            var task = await GetTaskAsync(taskId, ct);
            progress?.Report(task);
            if (task.Status is "completed" or "failed")
                return task;

            delayMs = Math.Min(delayMs * 2, 2000);
        }
    }

    /// <summary>Reads the taskId field from a 202 Accepted response body. Returns null if not present (synchronous completion).</summary>
    private static async Task<string?> ReadTaskId(HttpResponseMessage resp, CancellationToken ct)
    {
        var doc = await resp.Content.ReadFromJsonAsync<JsonDocument>(ct);
        if (doc != null && doc.RootElement.TryGetProperty("taskId", out var prop))
            return prop.GetString();
        return null;
    }

    // -- Error handling ----------------------------------------------

    private static async Task EnsureSuccess(HttpResponseMessage resp)
    {
        if (resp.IsSuccessStatusCode) return;

        string? errorMessage = null;
        try
        {
            var json = await resp.Content.ReadFromJsonAsync<JsonDocument>();
            errorMessage = json?.RootElement.TryGetProperty("error", out var err) == true
                ? err.GetString()
                : null;
        }
        catch { /* ignore deserialization failures */ }

        throw new TraverseHttpException(
            errorMessage ?? $"HTTP {(int)resp.StatusCode} {resp.ReasonPhrase}",
            resp.StatusCode);
    }

    // -- ProgressStream (upload progress tracking) -------------------

    private sealed class ProgressStream(Stream inner, IProgress<long> progress) : Stream
    {
        private long _totalRead;

        public override bool CanRead => inner.CanRead;
        public override bool CanSeek => inner.CanSeek;
        public override bool CanWrite => inner.CanWrite;
        public override long Length => inner.Length;
        public override long Position
        {
            get => inner.Position;
            set => inner.Position = value;
        }

        public override void Flush() => inner.Flush();
        public override int Read(byte[] buffer, int offset, int count)
        {
            var n = inner.Read(buffer, offset, count);
            if (n > 0) { _totalRead += n; progress.Report(_totalRead); }
            return n;
        }

        public override async Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken ct)
        {
            var n = await inner.ReadAsync(buffer.AsMemory(offset, count), ct);
            if (n > 0) { _totalRead += n; progress.Report(_totalRead); }
            return n;
        }

        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken ct = default)
        {
            var n = await inner.ReadAsync(buffer, ct);
            if (n > 0) { _totalRead += n; progress.Report(_totalRead); }
            return n;
        }

        public override long Seek(long offset, SeekOrigin origin) => inner.Seek(offset, origin);
        public override void SetLength(long value) => inner.SetLength(value);
        public override void Write(byte[] buffer, int offset, int count) => inner.Write(buffer, offset, count);
    }
}
