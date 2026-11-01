package com.truespar.traverse;

import java.lang.ref.Reference;
import java.util.Map;

/**
 * Bulk loader for high-throughput imports: a write transaction that skips the redo log.
 *
 * <p>{@link #commit()} saves the database with the load in it, so the data is written once;
 * readers don't see the load until then, and {@link #abort()} (or closing without a commit)
 * rolls it back. Holds the writer lock while open, like {@link Database#begin()}. Thread-safe:
 * calls from several threads take turns.</p>
 *
 * <pre>{@code
 * try (BulkWriter writer = db.bulkWriter()) {
 *     long nid = writer.createNode(new String[]{"Person"}, Map.of("name", "Alice", "age", 30));
 *     writer.createEdge(nid, otherId, "KNOWS", Map.of("since", 2020));
 *     writer.commit();
 * }
 * }</pre>
 */
public class BulkWriter implements AutoCloseable {
    private final long handle;
    private volatile boolean finished;

    // Package-private constructor
    BulkWriter(long handle) {
        this.handle = handle;
        // an unfinished load rolls back when its handle is freed
        Handles.CLEANER.register(this, () -> nativeFree(handle));
    }

    /** Create a node with the given labels and optional properties. Returns the node ID. */
    public long createNode(String[] labels, Map<String, Object> properties) {
        checkActive();
        String[] keys = null;
        Object[] values = null;
        if (properties != null && !properties.isEmpty()) {
            keys = new String[properties.size()];
            values = new Object[properties.size()];
            int i = 0;
            for (Map.Entry<String, Object> e : properties.entrySet()) {
                keys[i] = e.getKey();
                values[i] = e.getValue();
                i++;
            }
        }
        try {
            return nativeCreateNode(handle, labels, keys, values);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Create a node with labels only (no properties). Returns the node ID. */
    public long createNode(String[] labels) {
        return createNode(labels, null);
    }

    /** Create an edge between two nodes. Returns the edge ID. */
    public long createEdge(long source, long target, String edgeType, Map<String, Object> properties) {
        checkActive();
        String[] keys = null;
        Object[] values = null;
        if (properties != null && !properties.isEmpty()) {
            keys = new String[properties.size()];
            values = new Object[properties.size()];
            int i = 0;
            for (Map.Entry<String, Object> e : properties.entrySet()) {
                keys[i] = e.getKey();
                values[i] = e.getValue();
                i++;
            }
        }
        try {
            return nativeCreateEdge(handle, source, target, edgeType, keys, values);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Create an edge without properties. */
    public long createEdge(long source, long target, String edgeType) {
        return createEdge(source, target, edgeType, null);
    }

    /** Create a property index on a node label. */
    public void createIndex(String name, String label, String property) {
        checkActive();
        try {
            nativeCreateIndex(handle, name, label, property);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Create an edge index (with optional property key). */
    public void createEdgeIndex(String name, String edgeType, String property) {
        checkActive();
        try {
            nativeCreateEdgeIndex(handle, name, edgeType, property);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Create a type-only edge index. */
    public void createEdgeIndex(String name, String edgeType) {
        createEdgeIndex(name, edgeType, null);
    }

    /** Drop an edge index by name. */
    public void dropEdgeIndex(String name) {
        checkActive();
        try {
            nativeDropEdgeIndex(handle, name);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Commit the load: saves the database with it and makes it visible. Durable on return; on
     * error nothing was published and the load is rolled back.
     */
    public void commit() {
        checkActive();
        finished = true;
        try {
            nativeCommit(handle);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Abort the load, rolling it back. */
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
            throw new IllegalStateException("bulk writer already finished");
        }
    }

    private static native long nativeCreateNode(long handle, String[] labels,
                                                 String[] propKeys, Object[] propValues);
    private static native long nativeCreateEdge(long handle, long source, long target,
                                                 String edgeType, String[] propKeys, Object[] propValues);
    private static native void nativeCreateIndex(long handle, String name, String label, String property);
    private static native void nativeCreateEdgeIndex(long handle, String name, String edgeType, String property);
    private static native void nativeDropEdgeIndex(long handle, String name);
    private static native void nativeCommit(long handle);
    private static native void nativeAbort(long handle);
    private static native void nativeFree(long handle);
}
