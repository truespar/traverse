using System.Diagnostics;
using Traverse.Bolt.Internal;

namespace Traverse.Bolt;

/// <summary>Telemetry API type identifiers for the TELEMETRY message.</summary>
public enum QueryApiType
{
    TransactionFunction = 0,
    UnmanagedTransaction = 1,
    AutoCommit = 2,
    DriverLevel = 3
}

/// <summary>
/// High-performance native Bolt client for Traverse and other Bolt servers.
/// Zero external dependencies - uses only .NET 10 framework APIs.
/// </summary>
public sealed class BoltClient : IAsyncDisposable
{
    private const string UserAgent = "Traverse.Bolt/1.0";

    private readonly BoltConnection _conn;
    private readonly string? _database;
    private readonly NotificationsConfig? _notifications;
    private readonly long _fetchSize;
    private int _inTransaction;

    /// <summary>Timestamp when this client was created, for pool lifetime tracking.</summary>
    internal long CreatedTicks { get; } = Stopwatch.GetTimestamp();

    private BoltClient(BoltConnection conn, string? database, NotificationsConfig? notifications, long fetchSize)
    {
        _conn = conn;
        _database = database;
        _notifications = notifications;
        _fetchSize = fetchSize;
    }

    /// <summary>
    /// Connect to a Bolt server, perform handshake, send HELLO, and optionally authenticate.
    /// </summary>
    public static async ValueTask<BoltClient> ConnectAsync(
        string host, int port,
        BoltClientOptions? options = null,
        CancellationToken ct = default)
    {
        var conn = await BoltConnection.ConnectAsync(
            host, port,
            options?.UseTls ?? false,
            options?.AllowUntrustedCertificate ?? false,
            options?.ConnectionTimeout,
            ct);

        try
        {
            // HELLO
            await conn.SendHelloAsync(UserAgent, options?.Notifications, options?.RoutingContext, ct);
            var (tag, bodyOffset, _) = await conn.ReadMessageAsync(ct);
            if (tag == MsgTag.FAILURE)
                ThrowFailure(conn, bodyOffset);
            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after HELLO, got 0x{tag:X2}");

            // LOGON - always required to transition from Authentication to Ready state
            if (options?.AuthToken is { } authToken)
                await conn.SendLogonAsync(authToken, ct);
            else if (options?.Username is not null)
                await conn.SendLogonAsync(options.Username, options.Password ?? "", ct);
            else
                await conn.SendLogonNoneAsync(ct);

            (tag, bodyOffset, _) = await conn.ReadMessageAsync(ct);
            if (tag == MsgTag.FAILURE)
                ThrowFailure(conn, bodyOffset);
            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after LOGON, got 0x{tag:X2}");

            return new BoltClient(conn, options?.Database, options?.Notifications, options?.FetchSize ?? -1);
        }
        catch
        {
            await conn.DisposeAsync();
            throw;
        }
    }

    /// <summary>
    /// Connect using a Bolt URI: bolt://, bolt+s:// (TLS), bolt+ssc:// (TLS, self-signed).
    /// </summary>
    public static ValueTask<BoltClient> ConnectAsync(
        string uri,
        BoltClientOptions? options = null,
        CancellationToken ct = default)
    {
        var (host, port, useTls, allowUntrusted) = BoltUri.Parse(uri);
        options = MergeUriTls(options, useTls, allowUntrusted);
        return ConnectAsync(host, port, options, ct);
    }

    /// <summary>
    /// Execute a Cypher query and return the full result (columns + records + summary).
    /// Uses pipelined RUN+PULL (single TCP write) for minimum latency.
    /// </summary>
    public async ValueTask<BoltResult> RunAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        if (_inTransaction != 0)
            throw new InvalidOperationException("Cannot execute auto-commit queries while a transaction is active.");

        await _conn.SendRunPullAsync(cypher, parameters, _database, bookmarks, options, _notifications, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

        if (tag == MsgTag.FAILURE)
            throw await _conn.HandleRunFailureAsync(bodyOffset, ct);

        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

        var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
        var columns = BoltResult.ExtractColumns(runMeta);

        var (rawRecords, summary) = await _conn.ReadPullResponsesAsync(ct);

        var records = new BoltRecord[rawRecords.Count];
        for (int i = 0; i < rawRecords.Count; i++)
            records[i] = new BoltRecord(rawRecords[i], columns);

        return new BoltResult(columns, records, summary);
    }

    /// <summary>
    /// Execute a Cypher query and drain all result records without materializing them.
    /// Uses pipelined RUN+PULL (single TCP write) for minimum latency.
    /// RECORD messages are skipped at the chunk level - zero copy, zero parse.
    /// Returns summary metadata (stats, bookmark) with empty records.
    /// </summary>
    public async ValueTask<BoltResult> RunAndConsumeAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        if (_inTransaction != 0)
            throw new InvalidOperationException("Cannot execute auto-commit queries while a transaction is active.");

        await _conn.SendRunPullAsync(cypher, parameters, _database, bookmarks, options, _notifications, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

        if (tag == MsgTag.FAILURE)
            throw await _conn.HandleRunFailureAsync(bodyOffset, ct);

        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

        var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
        var columns = BoltResult.ExtractColumns(runMeta);

        var summary = await _conn.DrainPullResponsesAsync(ct);

        return new BoltResult(columns, Array.Empty<BoltRecord>(), summary);
    }

    /// <summary>
    /// Execute a Cypher query, discarding all result records.
    /// Uses pipelined RUN+DISCARD (single TCP write) - the server skips RECORD messages.
    /// Returns columns and summary metadata but no records.
    /// </summary>
    public async ValueTask<BoltResult> RunAndDiscardAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        if (_inTransaction != 0)
            throw new InvalidOperationException("Cannot execute auto-commit queries while a transaction is active.");

        await _conn.SendRunDiscardAsync(cypher, parameters, _database, bookmarks, options, _notifications, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

        if (tag == MsgTag.FAILURE)
            throw await _conn.HandleRunFailureAsync(bodyOffset, ct);

        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

        var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
        var columns = BoltResult.ExtractColumns(runMeta);

        var summary = await _conn.ReadDiscardResponseAsync(ct);

        return new BoltResult(columns, Array.Empty<BoltRecord>(), summary);
    }

    /// <summary>
    /// Execute a Cypher query and return a streaming cursor that lazily pulls batches.
    /// Use for large result sets to avoid materializing all records in memory.
    /// </summary>
    public async ValueTask<BoltResultCursor> RunStreamAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        if (_inTransaction != 0)
            throw new InvalidOperationException("Cannot execute auto-commit queries while a transaction is active.");

        long fetchSize = _fetchSize == -1 ? 1000 : _fetchSize;

        await _conn.SendRunOnlyAsync(cypher, parameters, _database, bookmarks, options, _notifications, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

        if (tag == MsgTag.FAILURE)
            ThrowFailure(_conn, bodyOffset);

        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

        var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
        var columns = BoltResult.ExtractColumns(runMeta);

        // Send first PULL batch
        await _conn.SendPullAsync(fetchSize, ct: ct);

        return new BoltResultCursor(_conn, columns, fetchSize);
    }

    /// <summary>
    /// Begin an explicit transaction. The returned transaction must be committed, rolled back,
    /// or disposed. Auto-commit queries are blocked while a transaction is active.
    /// </summary>
    public async ValueTask<BoltTransaction> BeginTransactionAsync(
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        if (Interlocked.CompareExchange(ref _inTransaction, 1, 0) != 0)
            throw new InvalidOperationException("A transaction is already active on this client.");

        try
        {
            await _conn.SendBeginAsync(_database, bookmarks, options, _notifications, ct);

            var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

            if (tag == MsgTag.FAILURE)
                ThrowFailure(_conn, bodyOffset);

            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after BEGIN, got 0x{tag:X2}");

            return new BoltTransaction(_conn, () => Interlocked.Exchange(ref _inTransaction, 0));
        }
        catch
        {
            Interlocked.Exchange(ref _inTransaction, 0);
            throw;
        }
    }

    /// <summary>
    /// Send LOGOFF to transition back to Authentication state (Bolt 5.x).
    /// Allows re-authenticating with different credentials without reconnecting.
    /// </summary>
    public async ValueTask LogoffAsync(CancellationToken ct = default)
    {
        await _conn.SendLogoffAsync(ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);
        if (tag == MsgTag.FAILURE)
            ThrowFailure(_conn, bodyOffset);
        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after LOGOFF, got 0x{tag:X2}");
    }

    /// <summary>
    /// Re-authenticate after LOGOFF with new credentials. Transitions back to Ready state.
    /// </summary>
    public async ValueTask LogonAsync(string username, string password, CancellationToken ct = default)
    {
        await _conn.SendLogonAsync(username, password, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);
        if (tag == MsgTag.FAILURE)
            ThrowFailure(_conn, bodyOffset);
        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after LOGON, got 0x{tag:X2}");
    }

    /// <summary>
    /// Send ROUTE message for cluster routing table discovery. Returns the routing table metadata.
    /// </summary>
    public async ValueTask<Dictionary<string, object?>> RouteAsync(
        Dictionary<string, string>? routing = null,
        IReadOnlyList<string>? bookmarks = null,
        string? impersonatedUser = null,
        CancellationToken ct = default)
    {
        await _conn.SendRouteAsync(routing, bookmarks, _database, impersonatedUser, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);
        if (tag == MsgTag.FAILURE)
            ThrowFailure(_conn, bodyOffset);
        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after ROUTE, got 0x{tag:X2}");

        return _conn.ReadSuccessMetadata(bodyOffset);
    }

    /// <summary>Send TELEMETRY message with the given API type (Bolt 5.4+).</summary>
    public async ValueTask SendTelemetryAsync(QueryApiType apiType, CancellationToken ct = default)
    {
        await _conn.SendTelemetryAsync((int)apiType, ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);
        if (tag == MsgTag.FAILURE)
            ThrowFailure(_conn, bodyOffset);
        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after TELEMETRY, got 0x{tag:X2}");
    }

    /// <summary>Send RESET to verify the connection is alive and return to Ready state.</summary>
    internal async ValueTask ResetAsync(CancellationToken ct = default)
        => await _conn.ResetAsync(ct);

    private static void ThrowFailure(BoltConnection conn, int bodyOffset)
    {
        var metadata = conn.ReadSuccessMetadata(bodyOffset);
        throw BoltException.FromMetadata(metadata, conn.NegotiatedVersion);
    }

    private static BoltClientOptions MergeUriTls(BoltClientOptions? options, bool useTls, bool allowUntrusted)
    {
        if (!useTls && !allowUntrusted)
            return options ?? new BoltClientOptions();

        return new BoltClientOptions
        {
            Username = options?.Username,
            Password = options?.Password,
            Database = options?.Database,
            UseTls = useTls || (options?.UseTls ?? false),
            AllowUntrustedCertificate = allowUntrusted || (options?.AllowUntrustedCertificate ?? false),
            ConnectionTimeout = options?.ConnectionTimeout ?? TimeSpan.FromSeconds(30),
            Notifications = options?.Notifications,
            FetchSize = options?.FetchSize ?? -1,
            AuthToken = options?.AuthToken,
            TelemetryDisabled = options?.TelemetryDisabled ?? false,
            RoutingContext = options?.RoutingContext,
        };
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            await _conn.SendGoodbyeAsync();
        }
        catch
        {
            // Best-effort GOODBYE
        }

        await _conn.DisposeAsync();
    }
}
