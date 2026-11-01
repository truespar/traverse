package com.truespar.traverse;

import java.lang.ref.Reference;
import java.time.Duration;
import java.util.Map;

/**
 * An open Traverse embedded graph database.
 *
 * <p>Open with {@link #open(String)}. Close it explicitly or via try-with-resources: closing rolls
 * back transactions and bulk writers still open on it, saves, and releases the file. Thread-safe.</p>
 *
 * <pre>{@code
 * try (Database db = Database.open("mydb.tvdb")) {
 *     try (Transaction tx = db.begin()) {
 *         tx.execute("CREATE (n:Person {name: 'Alice'})");
 *         tx.commit();
 *     }
 * }
 * }</pre>
 */
public class Database implements AutoCloseable {
    private final long handle;
    private volatile boolean closed;
    private volatile Duration busyTimeout = Duration.ofSeconds(5);
    private volatile Duration statementTimeout = Duration.ZERO;

    static {
        NativeLoader.load();
    }

    private Database(long handle) {
        this.handle = handle;
        Handles.CLEANER.register(this, () -> nativeFree(handle));
    }

    /** Open (or create) a database at the given path. */
    public static Database open(String path) {
        return new Database(nativeOpen(path));
    }

    /**
     * Begin a write transaction. One is active at a time: this waits for the active one at most
     * {@link #getBusyTimeout()}, then throws a {@link TraverseException}.
     */
    public Transaction begin() {
        checkOpen();
        try {
            return new Transaction(nativeBegin(handle), false);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Begin a read transaction: a snapshot of the database as of now. Never waits, runs alongside
     * the writer and other readers, and refuses writes.
     */
    public Transaction beginRead() {
        checkOpen();
        try {
            return new Transaction(nativeBeginRead(handle), true);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Begin a bulk load for high-throughput imports (see {@link BulkWriter}). Waits for an active
     * write transaction like {@link #begin()}.
     */
    public BulkWriter bulkWriter() {
        checkOpen();
        try {
            return new BulkWriter(nativeBulkWriter(handle));
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Flush dirty pages to disk. Waits for an active write transaction like {@link #begin()}. */
    public void flush() {
        checkOpen();
        try {
            nativeFlush(handle);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /**
     * How long {@link #begin()}, {@link #bulkWriter()} and {@link #flush()} wait for an active
     * write transaction. Default 5 seconds.
     */
    public Duration getBusyTimeout() {
        return busyTimeout;
    }

    /**
     * Sets {@link #getBusyTimeout()}. {@code ChronoUnit.FOREVER.getDuration()} (or any duration too
     * long to count in milliseconds) waits as long as it takes.
     */
    public void setBusyTimeout(Duration timeout) {
        if (timeout == null || timeout.isNegative()) {
            throw new IllegalArgumentException("busy timeout must be a non-negative duration");
        }
        checkOpen();
        long ms;
        try {
            ms = timeout.toMillis();
        } catch (ArithmeticException tooLong) {
            ms = -1; // as long as it takes
        }
        try {
            nativeSetBusyTimeout(handle, ms);
        } finally {
            Reference.reachabilityFence(this);
        }
        busyTimeout = timeout;
    }

    /**
     * How long a statement may run before it throws {@link QueryInterruptedException}.
     * {@link Duration#ZERO} (the default): no limit.
     */
    public Duration getStatementTimeout() {
        return statementTimeout;
    }

    /**
     * Sets {@link #getStatementTimeout()}; {@link Duration#ZERO} removes the limit. Applies to
     * transactions begun afterwards.
     */
    public void setStatementTimeout(Duration timeout) {
        if (timeout == null || timeout.isNegative()) {
            throw new IllegalArgumentException("statement timeout must be a non-negative duration");
        }
        checkOpen();
        long ms;
        try {
            ms = timeout.isZero() ? 0 : Math.max(1, timeout.toMillis());
        } catch (ArithmeticException tooLong) {
            ms = 0; // no limit
        }
        try {
            nativeSetStatementTimeout(handle, ms);
        } finally {
            Reference.reachabilityFence(this);
        }
        statementTimeout = timeout;
    }

    /**
     * Runs an openCypher query as a transaction of its own (auto-commit): a read on a snapshot, so
     * it never waits; a write in a write transaction that commits when the query succeeds and rolls
     * back when it fails (waiting at most {@link #getBusyTimeout()} for the writer lock), in which
     * {@code CALL { ... } IN TRANSACTIONS} commits each batch.
     */
    public QueryResult execute(String query) {
        return execute(query, null, null);
    }

    /** {@link #execute(String)} with parameters. */
    public QueryResult execute(String query, Map<String, Object> params) {
        return execute(query, params, null);
    }

    /**
     * {@link #execute(String)} with parameters; {@code dialect} selects the query language:
     * {@code null} or {@code "cypher"} (openCypher, the default), or {@code "gql"} for ISO/IEC
     * 39075 GQL.
     */
    public QueryResult execute(String query, Map<String, Object> params, String dialect) {
        checkOpen();
        boolean gql = Transaction.isGql(dialect);
        Object[][] kv = Transaction.split(params);
        try {
            return nativeExecute(handle, query, (String[]) kv[0], kv[1], gql);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Close the database. Safe to call multiple times. */
    @Override
    public void close() {
        if (closed) {
            return;
        }
        closed = true;
        try {
            nativeClose(handle);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    private void checkOpen() {
        if (closed) {
            throw new IllegalStateException("database is closed");
        }
    }

    private static native long nativeOpen(String path);
    private static native long nativeBegin(long handle);
    private static native long nativeBeginRead(long handle);
    private static native long nativeBulkWriter(long handle);
    private static native void nativeFlush(long handle);
    private static native void nativeSetBusyTimeout(long handle, long timeoutMs);
    private static native void nativeSetStatementTimeout(long handle, long timeoutMs);
    private static native QueryResult nativeExecute(long handle, String query, String[] paramKeys,
                                                    Object[] paramValues, boolean gql);
    private static native void nativeClose(long handle);
    private static native void nativeFree(long handle);
}
