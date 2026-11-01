package com.truespar.traverse;

import org.junit.jupiter.api.*;
import org.junit.jupiter.api.io.TempDir;

import java.io.File;
import java.nio.file.Path;
import java.time.Duration;
import java.time.temporal.ChronoUnit;
import java.util.*;
import java.util.concurrent.*;

import static org.junit.jupiter.api.Assertions.*;

class TraverseTest {

    @TempDir
    Path tempDir;

    private String dbPath() {
        return tempDir.resolve("test.tvdb").toString();
    }

    private Database openDb() {
        return Database.open(dbPath());
    }

    private static long count(Transaction tx, String label) {
        QueryResult r = tx.execute("MATCH (n:" + label + ") RETURN count(n) AS c");
        return (Long) r.data().get(0).get("c");
    }

    private static long count(Database db, String label) {
        try (Transaction tx = db.beginRead()) {
            return count(tx, label);
        }
    }

    // -----------------------------------------------------------------------
    // TestOpen
    // -----------------------------------------------------------------------

    @Nested
    class TestOpen {
        @Test
        void openAndFlushCreatesFile() {
            String path = dbPath();
            Database db = Database.open(path);
            try (Transaction tx = db.begin()) {
                tx.execute("CREATE (n:Test)");
                tx.commit();
            }
            db.flush();
            db.close();
            assertTrue(new File(path).exists());
        }

        @Test
        void reopenExistingDatabase() {
            String path = dbPath();
            Database db = Database.open(path);
            db.flush();
            db.close();
            Database db2 = Database.open(path);
            db2.flush();
            db2.close();
        }

        @Test
        void openAutoCloseable() {
            try (Database db = Database.open(dbPath())) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Test {x: 1})");
                    tx.commit();
                }
            }
        }

        @Test
        void openEmptyPathThrows() {
            assertThrows(TraverseException.class, () -> Database.open(""));
        }

        @Test
        void doubleClose() {
            Database db = Database.open(dbPath());
            db.close();
            db.close(); // should not throw
        }
    }

    // -----------------------------------------------------------------------
    // TestTransaction
    // -----------------------------------------------------------------------

    @Nested
    class TestTransaction {
        @Test
        void createAndQuery() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Person {name: 'Alice', age: 30})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Person) RETURN n.name, n.age");
                    List<Map<String, Object>> rows = result.data();
                    assertEquals(1, rows.size());
                    assertEquals("Alice", rows.get(0).get("n.name"));
                    assertEquals(30L, rows.get(0).get("n.age"));
                }
            }
        }

        @Test
        void explicitCommit() {
            try (Database db = openDb()) {
                Transaction tx = db.begin();
                tx.execute("CREATE (n:Foo {val: 42})");
                tx.commit();

                try (Transaction tx2 = db.begin()) {
                    QueryResult result = tx2.execute("MATCH (n:Foo) RETURN n.val");
                    assertEquals(1, result.getRowCount());
                }
            }
        }

        @Test
        void explicitAbort() {
            try (Database db = openDb()) {
                Transaction tx = db.begin();
                tx.execute("CREATE (n:Aborted {val: 99})");
                tx.abort();

                try (Transaction tx2 = db.begin()) {
                    QueryResult result = tx2.execute("MATCH (n:Aborted) RETURN n.val");
                    assertEquals(0, result.getRowCount());
                }
            }
        }

        @Test
        void closeAutoAborts() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Bad {val: 1})");
                    // no commit - close() should auto-abort
                }

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Bad) RETURN n");
                    assertEquals(0, result.getRowCount());
                }
            }
        }

        @Test
        void commitAfterCommitThrows() {
            try (Database db = openDb()) {
                Transaction tx = db.begin();
                tx.commit();
                assertThrows(IllegalStateException.class, tx::commit);
            }
        }

        @Test
        void executeAfterCommitThrows() {
            try (Database db = openDb()) {
                Transaction tx = db.begin();
                tx.commit();
                assertThrows(IllegalStateException.class, () -> tx.execute("RETURN 1"));
            }
        }

        @Test
        void syntaxError() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    assertThrows(TraverseException.class, () -> tx.execute("INVALID SYNTAX HERE"));
                }
            }
        }

        @Test
        void multipleQueriesInSingleTransaction() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (a:Person {name: 'Alice'})");
                    tx.execute("CREATE (b:Person {name: 'Bob'})");
                    tx.execute("MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)");
                    QueryResult result = tx.execute("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name");
                    List<Map<String, Object>> rows = result.data();
                    assertEquals(1, rows.size());
                    assertEquals("Alice", rows.get(0).get("a.name"));
                    assertEquals("Bob", rows.get(0).get("b.name"));
                    tx.commit();
                }
            }
        }

        @Test
        void unknownFunctionThrows() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    assertThrows(TraverseException.class, () -> tx.execute("RETURN no_such_function(1)"));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestQueryResult
    // -----------------------------------------------------------------------

    @Nested
    class TestQueryResult {
        @Test
        void columns() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 1 AS x, 'hello' AS y");
                    assertEquals(Arrays.asList("x", "y"), result.getColumns());
                }
            }
        }

        @Test
        void rowCount() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (a:N), (b:N), (c:N)");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:N) RETURN n");
                    assertEquals(3, result.getRowCount());
                }
            }
        }

        @Test
        void iteration() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("UNWIND [1, 2, 3] AS x RETURN x");
                    List<Long> values = new ArrayList<>();
                    for (Map<String, Object> row : result) {
                        values.add((Long) row.get("x"));
                    }
                    assertEquals(Arrays.asList(1L, 2L, 3L), values);
                }
            }
        }

        @Test
        void dataMethod() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("UNWIND ['a', 'b'] AS x RETURN x");
                    List<Map<String, Object>> data = result.data();
                    assertEquals(2, data.size());
                    assertEquals("a", data.get(0).get("x"));
                    assertEquals("b", data.get(1).get("x"));
                }
            }
        }

        @Test
        void stats() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("CREATE (n:S {val: 1})");
                    assertEquals(1L, result.getStats().get("nodes_created"));
                }
            }
        }

        @Test
        void toStringContainsInfo() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 1 AS x");
                    String s = result.toString();
                    assertTrue(s.contains("QueryResult"));
                    assertTrue(s.contains("rows=1"));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestDataTypes
    // -----------------------------------------------------------------------

    @Nested
    class TestDataTypes {
        @Test
        void nullValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN null AS x");
                    assertNull(result.data().get(0).get("x"));
                }
            }
        }

        @Test
        void boolValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN true AS x, false AS y");
                    Map<String, Object> row = result.data().get(0);
                    assertEquals(true, row.get("x"));
                    assertEquals(false, row.get("y"));
                }
            }
        }

        @Test
        void integerValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 42 AS x");
                    assertEquals(42L, result.data().get(0).get("x"));
                }
            }
        }

        @Test
        void floatValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 3.14 AS x");
                    double val = (Double) result.data().get(0).get("x");
                    assertEquals(3.14, val, 0.001);
                }
            }
        }

        @Test
        void stringValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 'hello' AS x");
                    assertEquals("hello", result.data().get(0).get("x"));
                }
            }
        }

        @Test
        @SuppressWarnings("unchecked")
        void listValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN [1, 2, 3] AS x");
                    List<Long> list = (List<Long>) result.data().get(0).get("x");
                    assertEquals(Arrays.asList(1L, 2L, 3L), list);
                }
            }
        }

        @Test
        @SuppressWarnings("unchecked")
        void mapValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN {a: 1, b: 'two'} AS x");
                    Map<String, Object> map = (Map<String, Object>) result.data().get(0).get("x");
                    assertEquals(1L, map.get("a"));
                    assertEquals("two", map.get("b"));
                }
            }
        }

        @Test
        @SuppressWarnings("unchecked")
        void nodeValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Person {name: 'Bob'})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Person) RETURN n");
                    Map<String, Object> node = (Map<String, Object>) result.data().get(0).get("n");
                    assertNotNull(node.get("_id"));
                    List<String> labels = (List<String>) node.get("_labels");
                    assertTrue(labels.contains("Person"));
                    assertEquals("Bob", node.get("name"));
                }
            }
        }

        @Test
        @SuppressWarnings("unchecked")
        void edgeValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (a:A)-[:KNOWS {since: 2020}]->(b:B)");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH ()-[r:KNOWS]->() RETURN r");
                    Map<String, Object> edge = (Map<String, Object>) result.data().get(0).get("r");
                    assertEquals("KNOWS", edge.get("_type"));
                    assertEquals(2020L, edge.get("since"));
                }
            }
        }

        @Test
        @SuppressWarnings("unchecked")
        void pathValue() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (a:X {id: 1})-[:R]->(b:X {id: 2})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute(
                            "MATCH p = (a:X {id: 1})-[:R]->(b:X {id: 2}) RETURN p");
                    Map<String, Object> path = (Map<String, Object>) result.data().get(0).get("p");
                    List<?> nodes = (List<?>) path.get("nodes");
                    List<?> edges = (List<?>) path.get("edges");
                    assertEquals(2, nodes.size());
                    assertEquals(1, edges.size());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestPropertyEdgeCases
    // -----------------------------------------------------------------------

    @Nested
    class TestPropertyEdgeCases {
        @Test
        void unicodeStringRoundtrips() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:I18n {name: '日本語テスト', emoji: '🚀🔥', arabic: 'مرحبا'})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:I18n) RETURN n.name, n.emoji, n.arabic");
                    Map<String, Object> row = result.data().get(0);
                    assertEquals("日本語テスト", row.get("n.name"));
                    assertEquals("🚀🔥", row.get("n.emoji"));
                    assertEquals("مرحبا", row.get("n.arabic"));
                }
            }
        }

        @Test
        void emptyStringProperty() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:E {val: ''})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:E) RETURN n.val");
                    assertEquals("", result.data().get(0).get("n.val"));
                }
            }
        }

        @Test
        void largeStringProperty() {
            try (Database db = openDb()) {
                String big = "x".repeat(100_000);
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Big {val: $s})", Map.of("s", big));
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Big) RETURN n.val");
                    assertEquals(big, result.data().get(0).get("n.val"));
                }
            }
        }

        @Test
        void negativeInteger() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN -9999999 AS val");
                    assertEquals(-9999999L, result.data().get(0).get("val"));
                }
            }
        }

        @Test
        void largeIntegerBoundary() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 9223372036854775807 AS val");
                    assertEquals(Long.MAX_VALUE, result.data().get(0).get("val"));
                }
            }
        }

        @Test
        void negativeFloat() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN -273.15 AS val");
                    assertEquals(-273.15, (Double) result.data().get(0).get("val"), 0.001);
                }
            }
        }

        @Test
        void zeroValues() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 0 AS i, 0.0 AS f, false AS b");
                    Map<String, Object> row = result.data().get(0);
                    assertEquals(0L, row.get("i"));
                    assertEquals(0.0, (Double) row.get("f"), 0.001);
                    assertEquals(false, row.get("b"));
                }
            }
        }

        @Test
        void multipleNullColumns() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN null AS a, null AS b, null AS c");
                    Map<String, Object> row = result.data().get(0);
                    assertNull(row.get("a"));
                    assertNull(row.get("b"));
                    assertNull(row.get("c"));
                }
            }
        }

        @Test
        void mixedNullAndValues() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN 42 AS a, null AS b, 'hello' AS c");
                    Map<String, Object> row = result.data().get(0);
                    assertEquals(42L, row.get("a"));
                    assertNull(row.get("b"));
                    assertEquals("hello", row.get("c"));
                }
            }
        }

        @Test
        void specialCharactersInString() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Special {val: 'line1\\nline2\\ttab'})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Special) RETURN n.val");
                    String val = (String) result.data().get(0).get("n.val");
                    assertTrue(val.contains("line1"));
                }
            }
        }

        @Test
        void bulkWriterUnicodeProperties() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"I18n"}, Map.of("name", "Ñoño", "city", "München"));
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:I18n) RETURN n.name, n.city");
                    Map<String, Object> row = result.data().get(0);
                    assertEquals("Ñoño", row.get("n.name"));
                    assertEquals("München", row.get("n.city"));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestQueryParameters
    // -----------------------------------------------------------------------

    @Nested
    class TestQueryParameters {
        @Test
        void stringParam() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:P {name: 'Alice'})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute(
                            "MATCH (n:P) WHERE n.name = $name RETURN n.name",
                            Map.of("name", "Alice"));
                    assertEquals("Alice", result.data().get(0).get("n.name"));
                }
            }
        }

        @Test
        void intParam() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("RETURN $x + 1 AS y", Map.of("x", 41));
                    assertEquals(42L, result.data().get(0).get("y"));
                }
            }
        }

        @Test
        void nullParam() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    Map<String, Object> params = new HashMap<>();
                    params.put("x", null);
                    QueryResult result = tx.execute("RETURN $x IS NULL AS y", params);
                    assertEquals(true, result.data().get(0).get("y"));
                }
            }
        }

        @Test
        void listParam() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute(
                            "UNWIND $items AS x RETURN x",
                            Map.of("items", Arrays.asList(10, 20, 30)));
                    List<Long> values = new ArrayList<>();
                    for (Map<String, Object> row : result) {
                        values.add((Long) row.get("x"));
                    }
                    assertEquals(Arrays.asList(10L, 20L, 30L), values);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestBulkWriter
    // -----------------------------------------------------------------------

    @Nested
    class TestBulkWriter {
        @Test
        void createNodes() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long nid1 = writer.createNode(new String[]{"Person"}, Map.of("name", "Alice", "age", 30));
                long nid2 = writer.createNode(new String[]{"Person"}, Map.of("name", "Bob", "age", 25));
                assertNotEquals(nid1, nid2);
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Person) RETURN n.name ORDER BY n.name");
                    List<Map<String, Object>> rows = result.data();
                    assertEquals("Alice", rows.get(0).get("n.name"));
                    assertEquals("Bob", rows.get(1).get("n.name"));
                }
            }
        }

        @Test
        void createNodeMultipleLabels() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"Person", "Employee"}, Map.of("name", "Bob"));
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Person:Employee) RETURN n.name");
                    assertEquals(1, result.data().size());
                    assertEquals("Bob", result.data().get(0).get("n.name"));
                }
            }
        }

        @Test
        void createEdges() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long a = writer.createNode(new String[]{"A"});
                long b = writer.createNode(new String[]{"B"});
                long eid = writer.createEdge(a, b, "KNOWS", Map.of("since", 2020));
                assertTrue(eid >= 0);
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH ()-[r:KNOWS]->() RETURN r.since");
                    assertEquals(2020L, result.data().get(0).get("r.since"));
                }
            }
        }

        @Test
        void createEdgeNoProperties() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long a = writer.createNode(new String[]{"X"});
                long b = writer.createNode(new String[]{"X"});
                writer.createEdge(a, b, "LINKS");
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH ()-[e:LINKS]->() RETURN count(e)");
                    assertEquals(1L, result.data().get(0).get("count(e)"));
                }
            }
        }

        @Test
        void createIndex() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"Person"}, Map.of("name", "Test"));
                writer.createIndex("idx_person_name", "Person", "name");
                writer.commit();
            }
        }

        @Test
        void createEdgeIndex() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long a = writer.createNode(new String[]{"X"});
                long b = writer.createNode(new String[]{"Y"});
                writer.createEdge(a, b, "REL", Map.of("w", 1));
                writer.createEdgeIndex("idx_rel_w", "REL", "w");
                writer.commit();
            }
        }

        @Test
        void dropEdgeIndex() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long a = writer.createNode(new String[]{"X"});
                long b = writer.createNode(new String[]{"Y"});
                writer.createEdge(a, b, "REL", Map.of("w", 1));
                writer.createEdgeIndex("idx_rel_w", "REL", "w");
                writer.commit();

                BulkWriter writer2 = db.bulkWriter();
                writer2.dropEdgeIndex("idx_rel_w");
                writer2.commit();
            }
        }

        @Test
        void autoCloseAborts() {
            try (Database db = openDb()) {
                try (BulkWriter writer = db.bulkWriter()) {
                    writer.createNode(new String[]{"CM"}, Map.of("x", 1));
                    // no commit - close() auto-aborts
                }
                assertEquals(0L, count(db, "CM"));
            }
        }

        @Test
        void abort() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"Nope"});
                writer.abort();
                assertEquals(0L, count(db, "Nope"));
            }
        }

        @Test
        void loadIsInvisibleUntilCommit() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                for (int i = 0; i < 100; i++) {
                    writer.createNode(new String[]{"Item"}, Map.of("i", i));
                }
                assertEquals(0L, count(db, "Item"));
                writer.commit();
                assertEquals(100L, count(db, "Item"));
            }
        }

        @Test
        void nodeWithoutProperties() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long nid = writer.createNode(new String[]{"Empty"});
                assertTrue(nid >= 0);
                writer.commit();
            }
        }

        @Test
        void abortReleasesWriteLock() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"First"});
                writer.abort();

                BulkWriter writer2 = db.bulkWriter();
                writer2.createNode(new String[]{"Second"});
                writer2.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Second) RETURN count(n)");
                    assertEquals(1L, result.data().get(0).get("count(n)"));
                }
            }
        }

        @Test
        void commitAfterCommitThrows() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"X"});
                writer.commit();
                assertThrows(IllegalStateException.class, writer::commit);
            }
        }

        @Test
        void createNodeAfterCommitThrows() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                writer.commit();
                assertThrows(IllegalStateException.class, () -> writer.createNode(new String[]{"X"}));
            }
        }

        @Test
        void createEdgeInvalidSourceThrows() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long a = writer.createNode(new String[]{"X"});
                assertThrows(TraverseException.class, () -> writer.createEdge(a, 999999, "BAD"));
                writer.abort();
            }
        }

        @Test
        void bulkInsertManyNodes() {
            try (Database db = openDb()) {
                int count = 1000;
                BulkWriter writer = db.bulkWriter();
                for (int i = 0; i < count; i++) {
                    writer.createNode(new String[]{"Batch"}, Map.of("idx", i));
                }
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Batch) RETURN count(n)");
                    assertEquals((long) count, result.data().get(0).get("count(n)"));
                }
            }
        }

        @Test
        void bulkInsertNodesAndEdgesGraph() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                long[] ids = new long[5];
                for (int i = 0; i < 5; i++)
                    ids[i] = writer.createNode(new String[]{"Chain"}, Map.of("pos", i));
                for (int i = 0; i < 4; i++)
                    writer.createEdge(ids[i], ids[i + 1], "NEXT");
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult rn = tx.execute("MATCH (n:Chain) RETURN count(n)");
                    assertEquals(5L, rn.data().get(0).get("count(n)"));

                    QueryResult re = tx.execute("MATCH ()-[e:NEXT]->() RETURN count(e)");
                    assertEquals(4L, re.data().get(0).get("count(e)"));

                    QueryResult rp = tx.execute(
                            "MATCH p = (start:Chain {pos: 0})-[:NEXT*]->(end:Chain) " +
                            "WHERE NOT (end)-[:NEXT]->() RETURN end.pos");
                    assertEquals(4L, rp.data().get(0).get("end.pos"));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestIndex
    // -----------------------------------------------------------------------

    @Nested
    class TestIndex {
        @Test
        void createIndexAndLookup() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    for (int i = 0; i < 100; i++)
                        tx.execute(String.format("CREATE (n:User {id: %d, name: 'user%d'})", i, i));
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE INDEX idx_user_id FOR (n:User) ON (n.id)");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:User {id: 42}) RETURN n.name");
                    assertEquals(1, result.data().size());
                    assertEquals("user42", result.data().get(0).get("n.name"));
                }
            }
        }

        @Test
        void dropIndex() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Item {code: 'A'})");
                    tx.execute("CREATE INDEX idx_item_code FOR (n:Item) ON (n.code)");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    tx.execute("DROP INDEX idx_item_code");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Item) RETURN n.code");
                    assertEquals(1, result.data().size());
                    assertEquals("A", result.data().get(0).get("n.code"));
                }
            }
        }

        @Test
        void uniqueConstraint() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE CONSTRAINT uniq_email FOR (n:User) REQUIRE n.email IS UNIQUE");
                    tx.execute("CREATE (n:User {email: 'alice@test.com'})");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    assertThrows(TraverseException.class,
                            () -> tx.execute("CREATE (n:User {email: 'alice@test.com'})"));
                }
            }
        }

        @Test
        void indexSurvivesFlushReopen() {
            String path = dbPath();
            Database db = Database.open(path);
            try (Transaction tx = db.begin()) {
                tx.execute("CREATE INDEX idx_name FOR (n:Person) ON (n.name)");
                for (int i = 0; i < 50; i++)
                    tx.execute(String.format("CREATE (n:Person {name: 'person%d'})", i));
                tx.commit();
            }
            db.flush();
            db.close();

            Database db2 = Database.open(path);
            try (Transaction tx = db2.begin()) {
                QueryResult result = tx.execute("MATCH (n:Person {name: 'person25'}) RETURN n.name");
                assertEquals(1, result.data().size());
                assertEquals("person25", result.data().get(0).get("n.name"));
            }
            db2.close();
        }
    }

    // -----------------------------------------------------------------------
    // TestConcurrency
    // -----------------------------------------------------------------------

    @Nested
    class TestConcurrency {
        @Test
        void multipleSequentialTransactions() {
            try (Database db = openDb()) {
                for (int i = 0; i < 10; i++) {
                    try (Transaction tx = db.begin()) {
                        tx.execute(String.format("CREATE (n:Seq {idx: %d})", i));
                        tx.commit();
                    }
                }

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Seq) RETURN count(n)");
                    assertEquals(10L, result.data().get(0).get("count(n)"));
                }
            }
        }

        @Test
        void transactionAfterBulkWrite() {
            try (Database db = openDb()) {
                BulkWriter writer = db.bulkWriter();
                for (int i = 0; i < 100; i++)
                    writer.createNode(new String[]{"Bulk"}, Map.of("idx", i));
                writer.commit();

                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Tx {val: 'after_bulk'})");
                    tx.commit();
                }

                try (Transaction tx = db.begin()) {
                    assertEquals(100L, tx.execute("MATCH (n:Bulk) RETURN count(n)").data().get(0).get("count(n)"));
                    assertEquals("after_bulk", tx.execute("MATCH (n:Tx) RETURN n.val").data().get(0).get("n.val"));
                }
            }
        }

        @Test
        void bulkWriteAfterTransaction() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE (n:Pre {val: 'before_bulk'})");
                    tx.commit();
                }

                BulkWriter writer = db.bulkWriter();
                writer.createNode(new String[]{"Post"}, Map.of("val", "after_tx"));
                writer.commit();

                try (Transaction tx = db.begin()) {
                    assertEquals("before_bulk", tx.execute("MATCH (n:Pre) RETURN n.val").data().get(0).get("n.val"));
                    assertEquals("after_tx", tx.execute("MATCH (n:Post) RETURN n.val").data().get(0).get("n.val"));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestScale
    // -----------------------------------------------------------------------

    @Nested
    class TestScale {
        @Test
        void bulkInsert10kNodes() {
            try (Database db = openDb()) {
                int count = 10_000;
                BulkWriter writer = db.bulkWriter();
                for (int i = 0; i < count; i++)
                    writer.createNode(new String[]{"User"}, Map.of("id", i, "name", "user_" + i));
                writer.commit();

                try (Transaction tx = db.begin()) {
                    assertEquals((long) count, tx.execute("MATCH (n:User) RETURN count(n)").data().get(0).get("count(n)"));
                }
            }
        }

        @Test
        void bulkInsert10kNodesAnd20kEdges() {
            try (Database db = openDb()) {
                int nodeCount = 10_000;
                int edgeCount = 20_000;
                Random rng = new Random(42);

                BulkWriter writer = db.bulkWriter();
                long[] ids = new long[nodeCount];
                for (int i = 0; i < nodeCount; i++)
                    ids[i] = writer.createNode(new String[]{"N"}, Map.of("idx", i));
                for (int i = 0; i < edgeCount; i++)
                    writer.createEdge(ids[rng.nextInt(nodeCount)], ids[rng.nextInt(nodeCount)], "LINKS");
                writer.commit();

                try (Transaction tx = db.begin()) {
                    assertEquals((long) nodeCount, tx.execute("MATCH (n:N) RETURN count(n)").data().get(0).get("count(n)"));
                    assertEquals((long) edgeCount, tx.execute("MATCH ()-[e:LINKS]->() RETURN count(e)").data().get(0).get("count(e)"));
                }
            }
        }

        @Test
        void transactionInsert1kWithIndex() {
            try (Database db = openDb()) {
                int count = 1000;
                try (Transaction tx = db.begin()) {
                    tx.execute("CREATE INDEX idx_item_id FOR (n:Item) ON (n.id)");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    for (int i = 0; i < count; i++)
                        tx.execute(String.format("CREATE (n:Item {id: %d, data: 'payload_%d'})", i, i));
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    assertEquals((long) count, tx.execute("MATCH (n:Item) RETURN count(n)").data().get(0).get("count(n)"));
                    assertEquals("payload_777", tx.execute("MATCH (n:Item {id: 777}) RETURN n.data").data().get(0).get("n.data"));
                }
            }
        }

        @Test
        void largeResultSet() {
            try (Database db = openDb()) {
                int count = 5000;
                BulkWriter writer = db.bulkWriter();
                for (int i = 0; i < count; i++)
                    writer.createNode(new String[]{"Row"}, Map.of("val", i));
                writer.commit();

                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute("MATCH (n:Row) RETURN n.val ORDER BY n.val");
                    assertEquals(count, result.getRowCount());
                }
            }
        }

        @Test
        void complexGraphMultipleEdgeTypes() {
            try (Database db = openDb()) {
                Random rng = new Random(123);
                BulkWriter writer = db.bulkWriter();

                long[] users = new long[100];
                long[] posts = new long[200];
                for (int i = 0; i < 100; i++)
                    users[i] = writer.createNode(new String[]{"User", "Active"}, Map.of("name", "user_" + i, "age", 18 + i % 60));
                for (int i = 0; i < 200; i++)
                    posts[i] = writer.createNode(new String[]{"Post"}, Map.of("title", "Post title " + i, "likes", i * 3));
                for (int i = 0; i < 300; i++)
                    writer.createEdge(users[rng.nextInt(100)], users[rng.nextInt(100)], "FOLLOWS");
                for (int i = 0; i < 200; i++)
                    writer.createEdge(users[i % 100], posts[i], "CREATED");
                for (int i = 0; i < 500; i++)
                    writer.createEdge(users[rng.nextInt(100)], posts[rng.nextInt(200)], "LIKED");
                writer.commit();

                try (Transaction tx = db.begin()) {
                    assertEquals(100L, tx.execute("MATCH (n:User) RETURN count(n)").data().get(0).get("count(n)"));
                    assertEquals(200L, tx.execute("MATCH (n:Post) RETURN count(n)").data().get(0).get("count(n)"));
                    assertEquals(300L, tx.execute("MATCH ()-[e:FOLLOWS]->() RETURN count(e)").data().get(0).get("count(e)"));
                    assertEquals(200L, tx.execute("MATCH ()-[e:CREATED]->() RETURN count(e)").data().get(0).get("count(e)"));
                    assertEquals(500L, tx.execute("MATCH ()-[e:LIKED]->() RETURN count(e)").data().get(0).get("count(e)"));

                    long followers = (Long) tx.execute(
                            "MATCH (u:User)-[:FOLLOWS]->(f:User)-[:CREATED]->(p:Post) RETURN count(DISTINCT u)")
                            .data().get(0).get("count(DISTINCT u)");
                    assertTrue(followers > 0);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestFlush
    // -----------------------------------------------------------------------

    @Nested
    class TestFlush {
        @Test
        void flushPersists() {
            String path = dbPath();
            Database db = Database.open(path);
            try (Transaction tx = db.begin()) {
                tx.execute("CREATE (n:Persist {val: 42})");
                tx.commit();
            }
            db.flush();
            db.close();

            // Reopen and verify
            Database db2 = Database.open(path);
            try (Transaction tx = db2.begin()) {
                QueryResult result = tx.execute("MATCH (n:Persist) RETURN n.val");
                assertEquals(42L, result.data().get(0).get("n.val"));
            }
            db2.close();
        }

        @Test
        void flushPersistsAcrossReopen() {
            String path = dbPath();
            Database db = Database.open(path);
            try (Transaction tx = db.begin()) {
                tx.execute("CREATE (n:Persist {val: 'survived'})");
                tx.commit();
            }
            db.flush();
            db.close();

            Database db2 = Database.open(path);
            try (Transaction tx = db2.begin()) {
                QueryResult result = tx.execute("MATCH (n:Persist) RETURN n.val");
                assertEquals("survived", result.data().get(0).get("n.val"));
            }
            db2.close();
        }
    }

    // -----------------------------------------------------------------------
    // TestGqlDialect - ISO/IEC 39075 GQL via execute(query, params, "gql").
    // The queries use GQL-only syntax (|| concat, IS INTEGER,
    // INSERT/FILTER) that the openCypher parser rejects, so passing
    // proves the dialect option reached the GQL front-end.
    // -----------------------------------------------------------------------

    @Nested
    class TestGqlDialect {
        @Test
        void gqlOnlySyntaxExecutes() {
            try (Database db = Database.open(dbPath());
                 Transaction tx = db.begin()) {
                QueryResult result = tx.execute(
                    "RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i", null, "gql");
                assertEquals("foobar", result.data().get(0).get("s"));
                assertEquals(true, result.data().get(0).get("i"));
            }
        }

        @Test
        void gqlInsertAndFilterRoundTrip() {
            try (Database db = Database.open(dbPath())) {
                try (Transaction tx = db.begin()) {
                    tx.execute("INSERT (:Person {name: 'Ada', age: 36})", null, "gql");
                    tx.commit();
                }
                try (Transaction tx = db.begin()) {
                    QueryResult result = tx.execute(
                        "MATCH (p:Person) FILTER p.age > 10 RETURN p.name AS name",
                        null, "gql");
                    assertEquals("Ada", result.data().get(0).get("name"));
                }
            }
        }

        @Test
        void gqlWithParameters() {
            try (Database db = Database.open(dbPath());
                 Transaction tx = db.begin()) {
                QueryResult result = tx.execute(
                    "RETURN $x + 1 AS n", Map.of("x", 41), "gql");
                assertEquals(42L, result.data().get(0).get("n"));
            }
        }

        @Test
        void cypherRejectsGqlSyntax() {
            try (Database db = Database.open(dbPath());
                 Transaction tx = db.begin()) {
                assertThrows(TraverseException.class,
                    () -> tx.execute("RETURN 1 IS INTEGER AS i"));
            }
        }

        @Test
        void unknownDialectThrows() {
            try (Database db = Database.open(dbPath());
                 Transaction tx = db.begin()) {
                assertThrows(IllegalArgumentException.class,
                    () -> tx.execute("RETURN 1", null, "sql"));
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestLifetimes: handle lifetimes, read transactions, busy timeout, threads
    // -----------------------------------------------------------------------

    @Nested
    class TestLifetimes {
        @Test
        void closeRollsBackOpenTransactions() {
            Database db = openDb();
            Transaction tx = db.begin();
            tx.execute("CREATE (:Lost)");
            Transaction reader = db.beginRead();
            db.close();

            TraverseException e = assertThrows(TraverseException.class, () -> tx.execute("RETURN 1"));
            assertTrue(e.getMessage().contains("closed"), e.getMessage());
            assertThrows(TraverseException.class, tx::commit);
            reader.close();
            assertThrows(IllegalStateException.class, db::begin);

            try (Database db2 = openDb()) {
                assertEquals(0L, count(db2, "Lost"));
            }
        }

        @Test
        void readTransactionSeesItsSnapshot() {
            try (Database db = openDb()) {
                Transaction writer = db.begin();
                writer.execute("CREATE (:N)");
                try (Transaction reader = db.beginRead()) {
                    assertTrue(reader.isReadOnly());
                    assertEquals(0L, count(reader, "N"));
                    assertThrows(TraverseException.class, () -> reader.execute("CREATE (:M)"));
                    writer.commit();
                    assertEquals(0L, count(reader, "N"));
                }
                assertEquals(1L, count(db, "N"));
            }
        }

        @Test
        void busyTimeout() {
            try (Database db = openDb()) {
                assertEquals(Duration.ofSeconds(5), db.getBusyTimeout());
                db.setBusyTimeout(Duration.ofMillis(20));
                Transaction writer = db.begin();
                TraverseException e = assertThrows(TraverseException.class, db::begin);
                assertTrue(e.getMessage().contains("busy"), e.getMessage());
                assertThrows(TraverseException.class, db::bulkWriter);
                assertThrows(TraverseException.class, db::flush);
                writer.abort();
                db.begin().commit();

                assertThrows(IllegalArgumentException.class, () -> db.setBusyTimeout(Duration.ofSeconds(-1)));
                db.setBusyTimeout(ChronoUnit.FOREVER.getDuration());
            }
        }

        @Test
        void transactionFinishesOnAnotherThread() throws Exception {
            try (Database db = openDb()) {
                Transaction tx = db.begin();
                tx.execute("CREATE (:Moved)");
                CompletableFuture.runAsync(tx::commit).get(30, TimeUnit.SECONDS);
                assertEquals(1L, count(db, "Moved"));
            }
        }

        private void abandonWriter(Database db) {
            Transaction tx = db.begin();
            tx.execute("CREATE (:Abandoned)");
        }

        @Test
        void abandonedTransactionIsRolledBack() throws Exception {
            try (Database db = openDb()) {
                db.setBusyTimeout(Duration.ofMillis(10));
                abandonWriter(db);
                // the cleaner thread rolls the writer back and releases the lock
                long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
                while (true) {
                    System.gc();
                    try (Transaction tx = db.begin()) {
                        assertEquals(0L, count(tx, "Abandoned"));
                        return;
                    } catch (TraverseException busy) {
                        if (System.nanoTime() > deadline) {
                            throw busy;
                        }
                        Thread.sleep(10);
                    }
                }
            }
        }

        @Test
        void threadsShareTheDatabase() throws Exception {
            try (Database db = openDb()) {
                db.setBusyTimeout(ChronoUnit.FOREVER.getDuration());
                ExecutorService pool = Executors.newFixedThreadPool(6);
                List<Future<?>> work = new ArrayList<>();
                for (int t = 0; t < 6; t++) {
                    boolean writes = t % 2 == 1;
                    work.add(pool.submit(() -> {
                        for (int i = 0; i < 20; i++) {
                            if (writes) {
                                try (Transaction tx = db.begin()) {
                                    tx.execute("CREATE (:T)");
                                    tx.commit();
                                }
                            } else {
                                try (Transaction tx = db.beginRead()) {
                                    long a = count(tx, "T");
                                    assertEquals(a, count(tx, "T"));
                                }
                            }
                        }
                    }));
                }
                for (Future<?> f : work) {
                    f.get(60, TimeUnit.SECONDS);
                }
                pool.shutdown();
                assertEquals(60L, count(db, "T"));
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestCancellation
    // -----------------------------------------------------------------------

    @Nested
    class TestCancellation {
        // a three-way cross product over 2,000 nodes: minutes of work unless stopped
        private static final String LONG = "MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)";

        private void seed(Database db) {
            try (Transaction tx = db.begin()) {
                tx.execute("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})");
                tx.commit();
            }
        }

        @Test
        void cancelFromAnotherThreadStopsTheQuery() throws Exception {
            try (Database db = openDb()) {
                seed(db);
                try (Transaction tx = db.beginRead()) {
                    CompletableFuture<Void> canceller = CompletableFuture.runAsync(() -> {
                        try {
                            Thread.sleep(200);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                        }
                        tx.cancel();
                    });
                    long start = System.nanoTime();
                    assertThrows(QueryInterruptedException.class, () -> tx.execute(LONG));
                    assertTrue(System.nanoTime() - start < TimeUnit.SECONDS.toNanos(20));
                    canceller.get(30, TimeUnit.SECONDS);
                    // the transaction carries on: a read changed nothing
                    assertEquals(1L, tx.execute("RETURN 1 AS n").data().get(0).get("n"));
                }
            }
        }

        @Test
        void statementTimeoutStopsTheQuery() {
            try (Database db = openDb()) {
                seed(db);
                assertEquals(Duration.ZERO, db.getStatementTimeout());
                db.setStatementTimeout(Duration.ofMillis(200));
                assertEquals(Duration.ofMillis(200), db.getStatementTimeout());
                try (Transaction tx = db.beginRead()) {
                    QueryInterruptedException e =
                        assertThrows(QueryInterruptedException.class, () -> tx.execute(LONG));
                    assertTrue(e.getMessage().contains("timed out"), e.getMessage());
                }
                assertThrows(IllegalArgumentException.class,
                    () -> db.setStatementTimeout(Duration.ofSeconds(-1)));
                db.setStatementTimeout(Duration.ZERO);
            }
        }

        @Test
        void terminateTransactionsEndsAnotherTransaction() {
            // a name of its own: SHOW TRANSACTIONS lists a database by its file name
            String name = "terminate_" + UUID.randomUUID().toString().replace("-", "");
            try (Database db = Database.open(tempDir.resolve(name + ".tvdb").toString())) {
                Transaction victim = db.begin();
                victim.execute("CREATE (:Pending)");
                try (Transaction admin = db.beginRead()) {
                    QueryResult r = admin.execute(
                        "SHOW TRANSACTIONS YIELD transactionId AS id, database, protocol, currentQuery "
                            + "WHERE database = '" + name + "' AND protocol = 'embedded' AND currentQuery = '' "
                            + "TERMINATE TRANSACTIONS id YIELD message RETURN message");
                    assertEquals(1, r.data().size());
                    assertEquals("Transaction terminated.", r.data().get(0).get("message"));
                }
                assertThrows(QueryInterruptedException.class, () -> victim.execute("RETURN 1"));
                assertThrows(QueryInterruptedException.class, victim::commit);
                assertEquals(0L, count(db, "Pending"));
            }
        }

        @Test
        void aStatementThatFailsAfterWritingFailsTheTransaction() {
            try (Database db = openDb()) {
                try (Transaction tx = db.begin()) {
                    // fails before writing: the transaction carries on
                    assertThrows(TraverseException.class, () -> tx.execute("CREATE (:Kept"));
                    tx.execute("CREATE (:Kept)");
                    // writes, then fails
                    assertThrows(TraverseException.class,
                        () -> tx.execute("CREATE (:Partial) WITH 1 AS one RETURN 10 / 0"));
                    TraverseException later =
                        assertThrows(TraverseException.class, () -> tx.execute("RETURN 1"));
                    assertTrue(later.getMessage().contains("failed"), later.getMessage());
                    assertThrows(TraverseException.class, tx::commit);
                }
                assertEquals(0L, count(db, "Kept"));
            }
        }
    }

    // -----------------------------------------------------------------------
    // TestAutoCommit
    // -----------------------------------------------------------------------

    @Nested
    class TestAutoCommit {
        private long autoCount(Database db, String label) {
            return (Long) db.execute("MATCH (n:" + label + ") RETURN count(n) AS c").data().get(0).get("c");
        }

        @Test
        void writesCommitAndReadsSeeThem() {
            try (Database db = openDb()) {
                db.execute("CREATE (:A {v: $v}), (:A {v: 2})", Map.of("v", 1L));
                assertEquals(2L, autoCount(db, "A"));
                db.execute("INSERT (:G {v: 1})", null, "gql");
                assertEquals(1L, db.execute("MATCH (n:G) RETURN n.v AS v", null, "gql").data().get(0).get("v"));
            }
        }

        @Test
        void errorsCarryTheirGqlStatus() {
            try (Database db = openDb()) {
                assertEquals("22012",
                    assertThrows(TraverseException.class, () -> db.execute("RETURN 1 / 0")).getGqlStatus());
                assertEquals("42N62",
                    assertThrows(TraverseException.class, () -> db.execute("RETURN x")).getGqlStatus());
            }
        }

        @Test
        void aFailedWriteLeavesNothingBehind() {
            try (Database db = openDb()) {
                assertThrows(TraverseException.class,
                    () -> db.execute("CREATE (:B) WITH 1 AS one RETURN 10 / 0"));
                assertEquals(0L, autoCount(db, "B"));
            }
        }

        @Test
        void callInTransactionsCommitsEachBatch() {
            try (Database db = openDb()) {
                db.execute("UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:C {v: 10 / i}) } "
                    + "IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE");
                assertEquals(2L, autoCount(db, "C"));
            }
        }

        @Test
        void aProcedureThatWritesRunsAsAWrite() {
            try (Database db = openDb()) {
                db.execute("CALL apoc.create.node(['P'], {v: 1}) YIELD node RETURN node");
                assertEquals(1L, autoCount(db, "P"));
            }
        }

        @Test
        @SuppressWarnings("try") // the writer is only held, for its lock
        void readsNeverWaitForTheWriterAndWritesDo() {
            try (Database db = openDb()) {
                db.setBusyTimeout(Duration.ofMillis(50));
                try (Transaction writer = db.begin()) {
                    assertEquals(1L, db.execute("RETURN 1 AS n").data().get(0).get("n"));
                    TraverseException e = assertThrows(TraverseException.class, () -> db.execute("CREATE (:D)"));
                    assertTrue(e.getMessage().contains("busy"), e.getMessage());
                }
            }
        }

        @Test
        void aStatementTimeoutStopsAnAutoCommitQuery() {
            try (Database db = openDb()) {
                db.execute("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})");
                db.setStatementTimeout(Duration.ofMillis(200));
                assertThrows(QueryInterruptedException.class,
                    () -> db.execute("MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)"));
            }
        }
    }
}
