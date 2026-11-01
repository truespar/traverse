using System.Runtime.InteropServices;
using Traverse.Interop;

namespace Traverse;

/// <summary>
/// Represents an open Traverse database.
/// </summary>
public sealed class Database : IDisposable
{
    /// <summary>
    /// Maximum database file size that <see cref="Open"/>
    /// will attempt to load.  Traverse is fully in-memory, so loading a file
    /// larger than available RAM will abort the process (Rust OOM is not recoverable).
    /// Defaults to 80% of total physical memory.  Set to 0 to disable the check.
    /// </summary>
    public static long MaxFileSizeBytes { get; set; }

    private readonly DatabaseSafeHandle _handle;
    private TimeSpan _busyTimeout = TimeSpan.FromSeconds(5);
    private TimeSpan _statementTimeout = Timeout.InfiniteTimeSpan;
    private bool _disposed;

    static Database()
    {
        try
        {
            var totalMemory = GC.GetGCMemoryInfo().TotalAvailableMemoryBytes;
            MaxFileSizeBytes = (long)(totalMemory * 0.8);
        }
        catch
        {
            MaxFileSizeBytes = 0; // disable check if we can't determine memory
        }
    }

    private Database(DatabaseSafeHandle handle)
    {
        _handle = handle;
    }

    /// <summary>
    /// Open (or create) a database at the given path.
    /// </summary>
    /// <param name="path">File path for the <c>.tvdb</c> database.</param>
    /// <returns>An open <see cref="Database"/> instance.</returns>
    /// <exception cref="TraverseException">Thrown when the native library reports an error.</exception>
    /// <exception cref="TraverseException">
    /// Thrown with <see cref="TvErrorCode.ResourceExhausted"/> if the file exceeds
    /// <see cref="MaxFileSizeBytes"/>.
    /// </exception>
    public static Database Open(string path)
    {
        ArgumentNullException.ThrowIfNull(path);
        CheckFileSize(path);

        var raw = Interop.NativeLibrary.traverse_open(path);
        if (raw == IntPtr.Zero)
            Interop.NativeLibrary.ThrowLastError();

        var handle = new DatabaseSafeHandle();
        Marshal.InitHandle(handle, raw);
        return new Database(handle);
    }

    private static void CheckFileSize(string path)
    {
        if (MaxFileSizeBytes <= 0) return;

        try
        {
            var info = new FileInfo(path);
            if (!info.Exists) return; // new database, nothing to load

            if (info.Length > MaxFileSizeBytes)
            {
                throw new TraverseException(
                    TvErrorCode.ResourceExhausted,
                    $"Database file is {info.Length / (1024.0 * 1024.0):F1} MB, which exceeds the " +
                    $"safety limit of {MaxFileSizeBytes / (1024.0 * 1024.0):F1} MB (80% of available memory). " +
                    $"Traverse loads the entire database into memory. Adjust Database.MaxFileSizeBytes to override.");
            }
        }
        catch (TraverseException)
        {
            throw; // re-throw our own size check exception
        }
        catch
        {
            // Invalid path, empty string, etc. - let the native library handle it.
        }
    }

    /// <summary>
    /// How long <see cref="BeginTransaction"/>, <see cref="BeginBulkWrite"/> and
    /// <see cref="Flush"/> wait for an active write transaction before throwing with
    /// <see cref="TvErrorCode.Busy"/>. <see cref="Timeout.InfiniteTimeSpan"/> waits as long as it
    /// takes. Default 5 seconds.
    /// </summary>
    public TimeSpan BusyTimeout
    {
        get => _busyTimeout;
        set
        {
            if (value < TimeSpan.Zero && value != Timeout.InfiniteTimeSpan)
                throw new ArgumentOutOfRangeException(nameof(value), "must be non-negative or Timeout.InfiniteTimeSpan");
            ObjectDisposedException.ThrowIf(_disposed, this);
            var ms = value == Timeout.InfiniteTimeSpan ? -1L : (long)value.TotalMilliseconds;
            WithHandle(h => Interop.NativeLibrary.traverse_set_busy_timeout(h, ms));
            _busyTimeout = value;
        }
    }

    /// <summary>
    /// How long a statement may run before it stops with <see cref="TvErrorCode.Interrupted"/>.
    /// <see cref="Timeout.InfiniteTimeSpan"/> (the default): no limit. Applies to transactions
    /// begun afterwards.
    /// </summary>
    public TimeSpan StatementTimeout
    {
        get => _statementTimeout;
        set
        {
            if (value <= TimeSpan.Zero && value != Timeout.InfiniteTimeSpan)
                throw new ArgumentOutOfRangeException(nameof(value), "must be positive or Timeout.InfiniteTimeSpan");
            ObjectDisposedException.ThrowIf(_disposed, this);
            var ms = value == Timeout.InfiniteTimeSpan ? 0L : Math.Max(1L, (long)value.TotalMilliseconds);
            WithHandle(h => Interop.NativeLibrary.traverse_set_statement_timeout(h, ms));
            _statementTimeout = value;
        }
    }

    /// <summary>
    /// Runs a Cypher query as a transaction of its own (auto-commit): a read on a snapshot, so it
    /// never waits; a write in a write transaction that commits when the query succeeds and rolls
    /// back when it fails (waiting at most <see cref="BusyTimeout"/> for the writer lock), in
    /// which <c>CALL { ... } IN TRANSACTIONS</c> commits each batch.
    /// </summary>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult Query(string cypher)
    {
        return Execute(cypher, gql: false, CancellationToken.None);
    }

    /// <summary>
    /// <see cref="Query(string)"/>; cancelling <paramref name="cancellationToken"/> stops it,
    /// waiting for the writer lock included.
    /// </summary>
    /// <exception cref="OperationCanceledException">The token was cancelled while the query ran.</exception>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult Query(string cypher, CancellationToken cancellationToken)
    {
        return Execute(cypher, gql: false, cancellationToken);
    }

    /// <summary>
    /// <see cref="Query(string)"/> for an ISO/IEC 39075 GQL query.
    /// </summary>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult QueryGql(string query)
    {
        return Execute(query, gql: true, CancellationToken.None);
    }

    /// <summary>
    /// <see cref="QueryGql(string)"/>; cancelling <paramref name="cancellationToken"/> stops it.
    /// </summary>
    /// <exception cref="OperationCanceledException">The token was cancelled while the query ran.</exception>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult QueryGql(string query, CancellationToken cancellationToken)
    {
        return Execute(query, gql: true, cancellationToken);
    }

    private QueryResult Execute(string query, bool gql, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(query);
        ObjectDisposedException.ThrowIf(_disposed, this);
        cancellationToken.ThrowIfCancellationRequested();
        using var canceller = cancellationToken.CanBeCanceled ? CancellerSafeHandle.ForStatement() : null;
        IntPtr raw;
        bool refAdded = false;
        canceller?.DangerousAddRef(ref refAdded);
        try
        {
            var cancellerHandle = canceller?.DangerousGetHandle() ?? IntPtr.Zero;
            using (canceller is null ? default : cancellationToken.Register(canceller.Cancel))
            {
                raw = WithHandle(h => gql
                    ? Interop.NativeLibrary.traverse_execute_gql(h, query, cancellerHandle)
                    : Interop.NativeLibrary.traverse_execute(h, query, cancellerHandle));
            }
        }
        finally
        {
            if (refAdded) canceller!.DangerousRelease();
        }
        if (raw == IntPtr.Zero)
        {
            if (cancellationToken.IsCancellationRequested
                && (TvErrorCode)Interop.NativeLibrary.traverse_last_error_code() == TvErrorCode.Interrupted)
                throw new OperationCanceledException(cancellationToken);
            Interop.NativeLibrary.ThrowLastError();
        }
        var result = new ResultSafeHandle();
        Marshal.InitHandle(result, raw);
        return new QueryResult(result);
    }

    /// <summary>
    /// Flush dirty pages to disk. Waits at most <see cref="BusyTimeout"/> for an active write
    /// transaction.
    /// </summary>
    /// <exception cref="TraverseException">
    /// Thrown on I/O or internal error, or with <see cref="TvErrorCode.Busy"/>.
    /// </exception>
    public void Flush()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        Interop.NativeLibrary.CheckReturnCode(WithHandle(Interop.NativeLibrary.traverse_flush));
    }

    /// <summary>
    /// Begin a write transaction. One is active at a time: this waits at most
    /// <see cref="BusyTimeout"/> for the active one.
    /// </summary>
    /// <returns>A new <see cref="Transaction"/>.</returns>
    /// <exception cref="TraverseException">
    /// Thrown on error, or with <see cref="TvErrorCode.Busy"/> if another writer stayed active.
    /// </exception>
    public Transaction BeginTransaction()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        return Transaction.Begin(_handle, readOnly: false);
    }

    /// <summary>
    /// Begin a read transaction: a snapshot of the database as of now. Never waits and runs
    /// alongside the writer and other readers; writes throw.
    /// </summary>
    /// <returns>A new read-only <see cref="Transaction"/>.</returns>
    /// <exception cref="TraverseException">Thrown on error.</exception>
    public Transaction BeginReadTransaction()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        return Transaction.Begin(_handle, readOnly: true);
    }

    /// <summary>
    /// Begin a bulk load for high-throughput loading (see <see cref="BulkWriter"/>). Waits at
    /// most <see cref="BusyTimeout"/> for an active write transaction.
    /// </summary>
    /// <returns>A new <see cref="BulkWriter"/>.</returns>
    /// <exception cref="TraverseException">
    /// Thrown on error, or with <see cref="TvErrorCode.Busy"/> if another writer stayed active.
    /// </exception>
    public BulkWriter BeginBulkWrite()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        return BulkWriter.Begin(_handle);
    }

    /// <summary>
    /// Close the database: rolls back transactions and bulk writes still open on it (their later
    /// calls throw), saves, and releases the file. Like a <see cref="FileStream"/>,
    /// throws when that save fails: what changed since the last save is not on disk then. A call
    /// still running on another thread delays the close to its end, and its error is lost.
    /// </summary>
    /// <exception cref="TraverseException">The final save failed.</exception>
    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _handle.Dispose();
        if (_handle.CloseError is { } error)
            throw error;
    }

    private T WithHandle<T>(Func<IntPtr, T> call)
    {
        bool refAdded = false;
        _handle.DangerousAddRef(ref refAdded);
        try
        {
            return call(_handle.DangerousGetHandle());
        }
        finally
        {
            if (refAdded) _handle.DangerousRelease();
        }
    }
}
