namespace Traverse;

/// <summary>
/// Error codes returned by the native Traverse library.
/// </summary>
public enum TvErrorCode
{
    /// <summary>No error.</summary>
    Ok = 0,
    /// <summary>An I/O error occurred.</summary>
    Io = 1,
    /// <summary>The database file is corrupt.</summary>
    Corrupt = 2,
    /// <summary>The database file is locked by another process.</summary>
    Locked = 3,
    /// <summary>Cypher syntax error.</summary>
    Syntax = 4,
    /// <summary>Cypher type error.</summary>
    Type = 5,
    /// <summary>Cypher semantic error.</summary>
    Semantic = 6,
    /// <summary>Graph-level error (node/edge not found, etc.).</summary>
    Graph = 7,
    /// <summary>Transaction error (already finished, read-only, database closed under it).</summary>
    Transaction = 8,
    /// <summary>Invalid argument passed to a function.</summary>
    InvalidArg = 9,
    /// <summary>An internal error (e.g. a caught panic).</summary>
    Internal = 10,
    /// <summary>A resource limit was exceeded (memory budget, max readers, etc.).</summary>
    ResourceExhausted = 11,
    /// <summary>
    /// Another write transaction held the writer lock for the whole
    /// <see cref="Database.BusyTimeout"/>. Transient: retry.
    /// </summary>
    Busy = 12,
    /// <summary>
    /// The statement was stopped before it finished: it ran past
    /// <see cref="Database.StatementTimeout"/>, it was cancelled (<see cref="Transaction.Cancel"/>,
    /// a cancellation token), or its transaction was terminated (<c>TERMINATE TRANSACTIONS</c>).
    /// </summary>
    Interrupted = 13,
}
