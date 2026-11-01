package com.truespar.traverse;

import java.lang.ref.Reference;
import java.util.Map;

/**
 * A transaction: the writer ({@link Database#begin()}) or a snapshot reader
 * ({@link Database#beginRead()}).
 *
 * <p>Use with try-with-resources for automatic cleanup. You must call
 * {@link #commit()} explicitly to persist changes; if close() is called
 * without a prior commit, the transaction is aborted. A transaction that is never closed is
 * rolled back once it is garbage collected; closing its database rolls it back at once (later
 * calls throw). Thread-safe: calls from several threads take turns, except {@link #cancel()},
 * which stops the running one. A statement that fails after changing data fails the transaction:
 * later statements and {@link #commit()} throw, and only an abort ends it.</p>
 *
 * <pre>{@code
 * try (Transaction tx = db.begin()) {
 *     tx.execute("CREATE (n:Person {name: 'Alice'})");
 *     tx.commit();
 * }
 * }</pre>
 */
public class Transaction implements AutoCloseable {
    private final long handle;
    private final boolean readOnly;
    private volatile boolean finished;

    // Package-private constructor
    Transaction(long handle, boolean readOnly) {
        this.handle = handle;
        this.readOnly = readOnly;
        // an unfinished transaction rolls back when its handle is freed
        Handles.CLEANER.register(this, () -> nativeFree(handle));
    }

    /** Whether this is a read transaction (writes are refused). */
    public boolean isReadOnly() {
        return readOnly;
    }

    /** Execute an openCypher query without parameters. */
    public QueryResult execute(String query) {
        return execute(query, null);
    }

    /** Execute an openCypher query with parameters. */
    public QueryResult execute(String query, Map<String, Object> params) {
        return execute(query, params, null);
    }

    /**
     * Execute a query. {@code dialect} selects the query language:
     * {@code null} or {@code "cypher"} (openCypher, the default), or
     * {@code "gql"} for ISO/IEC 39075 GQL.
     */
    public QueryResult execute(String query, Map<String, Object> params, String dialect) {
        checkActive();
        boolean gql = isGql(dialect);
        Object[][] kv = split(params);
        String[] keys = (String[]) kv[0];
        Object[] values = kv[1];
        try {
            return gql
                ? nativeExecuteGql(handle, query, keys, values)
                : nativeExecute(handle, query, keys, values);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Whether {@code dialect} is GQL; {@code null} and {@code "cypher"} are openCypher. */
    static boolean isGql(String dialect) {
        if (dialect == null || dialect.equals("cypher")) {
            return false;
        }
        if (dialect.equals("gql")) {
            return true;
        }
        throw new IllegalArgumentException(
            "unknown dialect '" + dialect + "' - expected 'cypher' or 'gql'");
    }

    /** Parameter names and values as parallel arrays, both {@code null} when there are none. */
    static Object[][] split(Map<String, Object> params) {
        if (params == null || params.isEmpty()) {
            return new Object[][] {null, null};
        }
        String[] keys = new String[params.size()];
        Object[] values = new Object[params.size()];
        int i = 0;
        for (Map.Entry<String, Object> e : params.entrySet()) {
            keys[i] = e.getKey();
            values[i] = e.getValue();
            i++;
        }
        return new Object[][] {keys, values};
    }

    /** Commit the transaction, persisting all changes (ends a read transaction). */
    public void commit() {
        checkActive();
        finished = true;
        try {
            nativeCommit(handle);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Stops the statement this transaction is running, from any thread: it throws
     * {@link QueryInterruptedException}. A statement that had already changed data fails the
     * transaction. Does nothing when no statement runs or the transaction ended.
     */
    public void cancel() {
        try {
            nativeCancel(handle);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Abort the transaction, discarding all changes. */
    public void abort() {
        checkActive();
        finished = true;
        try {
            nativeAbort(handle);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Auto-aborts if not already committed or aborted. */
    @Override
    public void close() {
        if (!finished) {
            finished = true;
            try {
                nativeAbort(handle);
            } finally {
                Reference.reachabilityFence(this);
            }
        }
    }

    private void checkActive() {
        if (finished) {
            throw new IllegalStateException("transaction already finished");
        }
    }

    private static native QueryResult nativeExecute(long handle, String query,
                                                     String[] paramKeys, Object[] paramValues);
    private static native QueryResult nativeExecuteGql(long handle, String query,
                                                        String[] paramKeys, Object[] paramValues);
    private static native void nativeCommit(long handle);
    private static native void nativeAbort(long handle);
    private static native void nativeCancel(long handle);
    private static native void nativeFree(long handle);
}
