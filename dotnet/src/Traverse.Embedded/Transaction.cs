using System.Runtime.InteropServices;
using Traverse.Interop;

namespace Traverse;

/// <summary>
/// A transaction on a Traverse database: the writer (<see cref="Database.BeginTransaction"/>)
/// or a snapshot reader (<see cref="Database.BeginReadTransaction"/>).
/// Disposing an uncommitted write transaction rolls it back. Any thread may use it, one call at
/// a time; disposing the database rolls it back and later calls throw.
/// A statement that fails after changing data fails the transaction: later queries and
/// <see cref="Commit"/> throw <see cref="TvErrorCode.Transaction"/>, and only a rollback ends it.
/// </summary>
public sealed class Transaction : IDisposable
{
    private readonly TransactionSafeHandle _handle;
    // stops the running statement from any thread; outlives the native transaction
    private readonly CancellerSafeHandle _canceller;
    // serializes calls, so commit/abort can't free the native handle under a running query
    private readonly object _sync = new();
    private bool _finished;
    private bool _disposed;

    private Transaction(TransactionSafeHandle handle, CancellerSafeHandle canceller, bool readOnly)
    {
        _handle = handle;
        _canceller = canceller;
        IsReadOnly = readOnly;
    }

    /// <summary>
    /// Whether this is a read transaction: it sees the database as of its start and refuses
    /// writes.
    /// </summary>
    public bool IsReadOnly { get; }

    internal static Transaction Begin(DatabaseSafeHandle dbHandle, bool readOnly)
    {
        bool refAdded = false;
        dbHandle.DangerousAddRef(ref refAdded);
        try
        {
            var db = dbHandle.DangerousGetHandle();
            var raw = readOnly
                ? Interop.NativeLibrary.traverse_begin_read(db)
                : Interop.NativeLibrary.traverse_begin(db);
            if (raw == IntPtr.Zero)
                Interop.NativeLibrary.ThrowLastError();

            var handle = new TransactionSafeHandle();
            Marshal.InitHandle(handle, raw);
            var rawCanceller = Interop.NativeLibrary.traverse_canceller(raw);
            if (rawCanceller == IntPtr.Zero)
            {
                handle.Dispose();
                Interop.NativeLibrary.ThrowLastError();
            }
            var canceller = new CancellerSafeHandle();
            Marshal.InitHandle(canceller, rawCanceller);
            return new Transaction(handle, canceller, readOnly);
        }
        finally
        {
            if (refAdded) dbHandle.DangerousRelease();
        }
    }

    /// <summary>
    /// Execute a Cypher query within this transaction.
    /// </summary>
    /// <param name="cypher">The Cypher query string.</param>
    /// <returns>A <see cref="QueryResult"/> with the result rows.</returns>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult Query(string cypher)
    {
        return Run(cypher, gql: false, CancellationToken.None);
    }

    /// <summary>
    /// Execute a Cypher query within this transaction; cancelling <paramref name="cancellationToken"/>
    /// stops it.
    /// </summary>
    /// <exception cref="OperationCanceledException">The token was cancelled while the query ran.</exception>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult Query(string cypher, CancellationToken cancellationToken)
    {
        return Run(cypher, gql: false, cancellationToken);
    }

    /// <summary>
    /// Execute an ISO/IEC 39075 GQL query. The GQL front-end lowers
    /// into the same engine <see cref="Query"/> uses - identical
    /// planning and execution, only the surface language differs.
    /// </summary>
    /// <returns>A <see cref="QueryResult"/> with the result rows.</returns>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult QueryGql(string query)
    {
        return Run(query, gql: true, CancellationToken.None);
    }

    /// <summary>
    /// Execute an ISO/IEC 39075 GQL query; cancelling <paramref name="cancellationToken"/> stops it.
    /// </summary>
    /// <exception cref="OperationCanceledException">The token was cancelled while the query ran.</exception>
    /// <exception cref="TraverseException">Thrown on syntax, type, semantic, or graph error.</exception>
    public QueryResult QueryGql(string query, CancellationToken cancellationToken)
    {
        return Run(query, gql: true, cancellationToken);
    }

    /// <summary>
    /// Stops the statement this transaction is running, from any thread: it throws
    /// <see cref="TvErrorCode.Interrupted"/>. A statement that had already changed data fails the
    /// transaction. Does nothing when no statement runs or the transaction ended.
    /// </summary>
    public void Cancel()
    {
        _canceller.Cancel();
    }

    private QueryResult Run(string query, bool gql, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(query);
        cancellationToken.ThrowIfCancellationRequested();
        lock (_sync)
        {
            ThrowIfFinished();
            bool refAdded = false;
            _handle.DangerousAddRef(ref refAdded);
            try
            {
                IntPtr raw;
                using (cancellationToken.Register(Cancel))
                {
                    raw = gql
                        ? Interop.NativeLibrary.traverse_query_gql(_handle.DangerousGetHandle(), query)
                        : Interop.NativeLibrary.traverse_query(_handle.DangerousGetHandle(), query);
                }
                if (raw == IntPtr.Zero)
                {
                    if (cancellationToken.IsCancellationRequested
                        && (TvErrorCode)Interop.NativeLibrary.traverse_last_error_code() == TvErrorCode.Interrupted)
                        throw new OperationCanceledException(cancellationToken);
                    Interop.NativeLibrary.ThrowLastError();
                }

                var resultHandle = new ResultSafeHandle();
                Marshal.InitHandle(resultHandle, raw);
                return new QueryResult(resultHandle);
            }
            finally
            {
                if (refAdded) _handle.DangerousRelease();
            }
        }
    }

    /// <summary>
    /// Commit the transaction, making all changes permanent (ends a read transaction).
    /// </summary>
    /// <exception cref="TraverseException">
    /// Thrown on I/O error, or with <see cref="TvErrorCode.Transaction"/> if the database was
    /// disposed meanwhile (the transaction was rolled back).
    /// </exception>
    public void Commit()
    {
        lock (_sync)
        {
            ThrowIfFinished();
            _finished = true;
            var raw = _handle.DangerousGetHandle();
            _handle.SetHandleAsInvalid(); // the native call frees it
            var rc = Interop.NativeLibrary.traverse_commit(raw);
            Interop.NativeLibrary.CheckReturnCode(rc);
        }
    }

    /// <summary>
    /// Abort the transaction, discarding all changes.
    /// </summary>
    public void Abort()
    {
        lock (_sync)
        {
            ThrowIfFinished();
            _finished = true;
            var raw = _handle.DangerousGetHandle();
            _handle.SetHandleAsInvalid(); // the native call frees it
            // fails only for a null handle; finished or rolled back by a close is fine
            _ = Interop.NativeLibrary.traverse_abort(raw);
        }
    }

    /// <inheritdoc/>
    public void Dispose()
    {
        lock (_sync)
        {
            if (_disposed) return;
            _disposed = true;
            _finished = true;
            // still open: the SafeHandle's ReleaseHandle rolls it back (traverse_abort)
            _handle.Dispose();
            _canceller.Dispose();
        }
    }

    private void ThrowIfFinished()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (_finished)
            throw new TraverseException(TvErrorCode.Transaction, "transaction already finished");
    }
}
