package com.truespar.traverse;

import java.util.*;

/**
 * The result of a Cypher query execution.
 *
 * <p>Iterable over rows, where each row is a {@code Map<String, Object>}
 * mapping column names to values.</p>
 *
 * <pre>{@code
 * QueryResult result = tx.execute("MATCH (n:Person) RETURN n.name, n.age");
 * for (Map<String, Object> row : result) {
 *     System.out.println(row.get("n.name") + " " + row.get("n.age"));
 * }
 * }</pre>
 */
public class QueryResult implements Iterable<Map<String, Object>> {
    private final String[] columns;
    private final List<Map<String, Object>> rows;
    private final Map<String, Long> stats;

    // Package-private constructor, called from JNI
    QueryResult(String[] columns, List<Map<String, Object>> rows, Map<String, Long> stats) {
        this.columns = columns;
        this.rows = rows != null ? rows : Collections.emptyList();
        this.stats = stats != null ? stats : Collections.emptyMap();
    }

    /** Column names from the query. */
    public List<String> getColumns() {
        return Collections.unmodifiableList(Arrays.asList(columns));
    }

    /** Number of result rows. */
    public int getRowCount() {
        return rows.size();
    }

    /** Mutation statistics (non-zero counters only). */
    public Map<String, Long> getStats() {
        return Collections.unmodifiableMap(stats);
    }

    /** Get all rows as a list of maps. */
    public List<Map<String, Object>> data() {
        return Collections.unmodifiableList(rows);
    }

    @Override
    public Iterator<Map<String, Object>> iterator() {
        return rows.iterator();
    }

    @Override
    public String toString() {
        return "QueryResult{columns=" + Arrays.toString(columns) + ", rows=" + rows.size() + "}";
    }
}
