using System.Runtime.InteropServices;
using Traverse.Interop;

namespace Traverse;

/// <summary>
/// Bulk loader for high-throughput graph loading: a write transaction that skips the redo log.
/// <see cref="Commit"/> saves the database with the load in it, so the data is written once;
/// readers don't see the load until then, and <see cref="Abort"/> (or disposing without commit)
/// rolls it back. Holds the writer lock while open, like <see cref="Database.BeginTransaction"/>.
/// Any thread may use it, one call at a time; disposing the database rolls it back.
/// </summary>
public sealed class BulkWriter : IDisposable
{
    private readonly BulkWriterSafeHandle _handle;
    // serializes calls, so commit/abort can't free the native handle under a running call
    private readonly object _sync = new();
    private bool _finished;
    private bool _disposed;

    private BulkWriter(BulkWriterSafeHandle handle)
    {
        _handle = handle;
    }

    internal static BulkWriter Begin(DatabaseSafeHandle dbHandle)
    {
        bool refAdded = false;
        dbHandle.DangerousAddRef(ref refAdded);
        try
        {
            var raw = Interop.NativeLibrary.traverse_bulk_begin(dbHandle.DangerousGetHandle());
            if (raw == IntPtr.Zero)
                Interop.NativeLibrary.ThrowLastError();

            var handle = new BulkWriterSafeHandle();
            Marshal.InitHandle(handle, raw);
            return new BulkWriter(handle);
        }
        finally
        {
            if (refAdded) dbHandle.DangerousRelease();
        }
    }

    /// <summary>
    /// Create a node with the given labels and properties.
    /// Property values are auto-inferred: "42" becomes Int, "3.14" becomes Float,
    /// "true"/"false" becomes Bool, empty string becomes Null, otherwise String.
    /// </summary>
    /// <param name="labels">Node labels.</param>
    /// <param name="propKeys">Property keys (parallel with <paramref name="propValues"/>).</param>
    /// <param name="propValues">Property values as strings (auto-inferred types).</param>
    /// <returns>The ID of the newly created node.</returns>
    /// <exception cref="TraverseException">Thrown on error.</exception>
    public long CreateNode(string[] labels, string[]? propKeys = null, string[]? propValues = null)
    {
        ArgumentNullException.ThrowIfNull(labels);
        var keys = propKeys ?? [];
        var values = propValues ?? [];
        if (keys.Length != values.Length)
            throw new ArgumentException("propKeys and propValues must have the same length");

        return Call(h =>
        {
            var id = Interop.NativeLibrary.traverse_bulk_create_node(
                h, labels, labels.Length, keys, values, keys.Length);
            if (id < 0)
                Interop.NativeLibrary.ThrowLastError();
            return id;
        });
    }

    /// <summary>
    /// Create an edge between two existing nodes.
    /// </summary>
    /// <param name="source">Source node ID.</param>
    /// <param name="target">Target node ID.</param>
    /// <param name="edgeType">Edge type name.</param>
    /// <param name="propKeys">Property keys (parallel with <paramref name="propValues"/>).</param>
    /// <param name="propValues">Property values as strings (auto-inferred types).</param>
    /// <returns>The ID of the newly created edge.</returns>
    /// <exception cref="TraverseException">Thrown if source/target not found or other error.</exception>
    public long CreateEdge(long source, long target, string edgeType,
                           string[]? propKeys = null, string[]? propValues = null)
    {
        ArgumentNullException.ThrowIfNull(edgeType);
        var keys = propKeys ?? [];
        var values = propValues ?? [];
        if (keys.Length != values.Length)
            throw new ArgumentException("propKeys and propValues must have the same length");

        return Call(h =>
        {
            var id = Interop.NativeLibrary.traverse_bulk_create_edge(
                h, source, target, edgeType, keys, values, keys.Length);
            if (id < 0)
                Interop.NativeLibrary.ThrowLastError();
            return id;
        });
    }

    /// <summary>
    /// Create an index on a label/property combination.
    /// The index is populated immediately from existing nodes with matching labels.
    /// </summary>
    /// <param name="name">Unique index name.</param>
    /// <param name="label">Node label to index.</param>
    /// <param name="property">Property key to index.</param>
    /// <exception cref="TraverseException">Thrown if an index with the same name or label/property already exists.</exception>
    public void CreateIndex(string name, string label, string property)
    {
        ArgumentNullException.ThrowIfNull(name);
        ArgumentNullException.ThrowIfNull(label);
        ArgumentNullException.ThrowIfNull(property);
        Call(h => Interop.NativeLibrary.CheckReturnCode(
            Interop.NativeLibrary.traverse_bulk_create_index(h, name, label, property)));
    }

    /// <summary>
    /// Create an edge property index on an edge type/property combination.
    /// The index is populated immediately from existing edges with matching type.
    /// </summary>
    /// <param name="name">Unique index name.</param>
    /// <param name="edgeType">Edge type to index.</param>
    /// <param name="property">Property key to index.</param>
    /// <exception cref="TraverseException">Thrown if an index with the same name already exists.</exception>
    public void CreateEdgeIndex(string name, string edgeType, string property)
    {
        ArgumentNullException.ThrowIfNull(name);
        ArgumentNullException.ThrowIfNull(edgeType);
        ArgumentNullException.ThrowIfNull(property);
        Call(h => Interop.NativeLibrary.CheckReturnCode(
            Interop.NativeLibrary.traverse_bulk_create_edge_index(h, name, edgeType, property)));
    }

    /// <summary>
    /// Create a type-only edge index on an edge type.
    /// This registers the edge type for DDL tracking.
    /// </summary>
    /// <param name="name">Unique index name.</param>
    /// <param name="edgeType">Edge type to index.</param>
    /// <exception cref="TraverseException">Thrown if an index with the same name already exists.</exception>
    public void CreateEdgeIndex(string name, string edgeType)
    {
        ArgumentNullException.ThrowIfNull(name);
        ArgumentNullException.ThrowIfNull(edgeType);
        Call(h => Interop.NativeLibrary.CheckReturnCode(
            Interop.NativeLibrary.traverse_bulk_create_edge_index(h, name, edgeType, null)));
    }

    /// <summary>
    /// Drop an edge index by name.
    /// </summary>
    /// <param name="name">Name of the edge index to drop.</param>
    /// <exception cref="TraverseException">Thrown if the index does not exist.</exception>
    public void DropEdgeIndex(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        Call(h => Interop.NativeLibrary.CheckReturnCode(
            Interop.NativeLibrary.traverse_bulk_drop_edge_index(h, name)));
    }

    /// <summary>
    /// Commit the load: saves the database with it and makes it visible. Durable when this
    /// returns; on error nothing was published and the load is rolled back.
    /// </summary>
    /// <exception cref="TraverseException">
    /// Thrown on I/O error, or with <see cref="TvErrorCode.Transaction"/> if the database was
    /// disposed meanwhile (the load was rolled back).
    /// </exception>
    public void Commit()
    {
        lock (_sync)
        {
            ThrowIfFinished();
            _finished = true;
            var raw = _handle.DangerousGetHandle();
            _handle.SetHandleAsInvalid(); // the native call frees it
            var rc = Interop.NativeLibrary.traverse_bulk_commit(raw);
            Interop.NativeLibrary.CheckReturnCode(rc);
        }
    }

    /// <summary>
    /// Abort the load, rolling it back.
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
            _ = Interop.NativeLibrary.traverse_bulk_abort(raw);
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
            // still open: the SafeHandle's ReleaseHandle rolls it back (traverse_bulk_abort)
            _handle.Dispose();
        }
    }

    private T Call<T>(Func<IntPtr, T> call)
    {
        lock (_sync)
        {
            ThrowIfFinished();
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

    private void Call(Action<IntPtr> call) => Call<bool>(h => { call(h); return true; });

    private void ThrowIfFinished()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (_finished)
            throw new TraverseException(TvErrorCode.InvalidArg, "bulk writer already finished");
    }
}
