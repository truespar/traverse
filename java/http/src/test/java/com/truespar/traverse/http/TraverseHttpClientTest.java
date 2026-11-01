package com.truespar.traverse.http;

import org.junit.jupiter.api.*;
import java.io.ByteArrayInputStream;
import java.time.Duration;
import java.util.*;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

@TestInstance(TestInstance.Lifecycle.PER_CLASS)
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
class TraverseHttpClientTest {

    static final String BASE_URL = System.getenv("TRAVERSE_BASE_URL") != null
            ? System.getenv("TRAVERSE_BASE_URL") : "http://localhost:7691";

    TraverseHttpClient client;
    String testDatabase;
    boolean available;

    @BeforeAll
    void setUp() {
        try {
            var probe = new TraverseHttpClient(BASE_URL, new TraverseHttpOptions(
                    null, Duration.ofSeconds(3), null, false));
            var health = probe.health();
            if (!"ok".equals(health.status())) { available = false; return; }

            // Management endpoints require X-API-Key even when user
            // auth is disabled - always forward the env key when set.
            String apiKey = System.getenv("TRAVERSE_API_KEY");

            client = new TraverseHttpClient(BASE_URL, new TraverseHttpOptions(
                    apiKey, Duration.ofSeconds(30), Duration.ofSeconds(60), false));
            testDatabase = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(testDatabase);
            available = true;
        } catch (Exception e) {
            available = false;
        }
    }

    @AfterAll
    void tearDown() {
        if (client != null) {
            try { client.dropDatabase(testDatabase); } catch (Exception ignored) {}
            client.close();
        }
    }

    void requireServer() {
        assumeTrue(available, "Traverse HTTP server not available at " + BASE_URL);
    }

    // -- Health ----------------------------------------------------------

    @Nested class HealthTests {
        @Test void healthReturnsStatusAndVersion() {
            requireServer();
            var health = client.health();
            assertEquals("ok", health.status());
            assertFalse(health.version().isEmpty());
        }

        @Test void readyReturnsTrue() {
            requireServer();
            assertTrue(client.ready());
        }

        @Test void healthReportsFeatures() {
            requireServer();
            var health = client.health();
            // The features map is present on all current servers
            // (gql, grpc, tls, mcp, ml).
            assertNotNull(health.features());
            assertTrue(health.features().containsKey("gql"));
        }
    }

    // -- Query -----------------------------------------------------------

    @Nested class QueryTests {
        @Test void returnScalar() {
            requireServer();
            var result = client.query("RETURN 42 AS n", null, testDatabase);
            assertEquals(1, result.rows().size());
            assertEquals(42, result.rows().getFirst().getFirst().asInt());
        }

        @Test void returnString() {
            requireServer();
            var result = client.query("RETURN 'hello' AS s", null, testDatabase);
            assertEquals("hello", result.rows().getFirst().getFirst().asString());
        }

        @Test void withParameters() {
            requireServer();
            var result = client.query("RETURN $x AS val",
                    Map.of("x", 99), testDatabase);
            assertEquals(99, result.rows().getFirst().getFirst().asInt());
        }

        @Test void gqlDialect() {
            requireServer();
            var features = client.health().features();
            org.junit.jupiter.api.Assumptions.assumeTrue(
                    features != null && Boolean.TRUE.equals(features.get("gql")),
                    "server built without the gql feature");
            // `||` concat and `IS INTEGER` are GQL-only syntax - they
            // parse only when the dialect reaches the server.
            var result = client.query("RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i",
                    null, testDatabase, "gql");
            assertEquals("foobar", result.rows().getFirst().get(0).asString());
            assertTrue(result.rows().getFirst().get(1).asBoolean());
        }

        @Test void writeQueryStats() {
            requireServer();
            var result = client.query("CREATE (n:_HttpTest) RETURN n", null, testDatabase);
            try {
                assertEquals(1, result.stats().nodesCreated());
            } finally {
                client.query("MATCH (n:_HttpTest) DELETE n", null, testDatabase);
            }
        }

        @Test void syntaxErrorThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.query("THIS IS NOT CYPHER", null, testDatabase));
            assertEquals(400, ex.statusCode());
        }
    }

    // -- Databases -------------------------------------------------------

    @Nested class DatabaseTests {
        @Test void listDatabasesContainsTestDb() {
            requireServer();
            var dbs = client.listDatabases();
            assertTrue(dbs.stream().anyMatch(d -> d.name().equals(testDatabase)));
        }

        @Test void createAndDropRoundtrip() {
            requireServer();
            var name = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(name);
            assertTrue(client.listDatabases().stream().anyMatch(d -> d.name().equals(name)));
            client.dropDatabase(name);
            assertFalse(client.listDatabases().stream().anyMatch(d -> d.name().equals(name)));
        }

        @Test void renameRoundtrip() {
            requireServer();
            var original = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            var renamed = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(original);
            try {
                client.renameDatabase(original, renamed);
                assertFalse(client.listDatabases().stream().anyMatch(d -> d.name().equals(original)));
                assertTrue(client.listDatabases().stream().anyMatch(d -> d.name().equals(renamed)));
            } finally {
                try { client.dropDatabase(renamed); } catch (Exception ignored) {}
                try { client.dropDatabase(original); } catch (Exception ignored) {}
            }
        }

        @Test void unloadAndLoadRoundtrip() {
            requireServer();
            var name = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(name);
            try {
                client.unloadDatabase(name);
                assertFalse(client.listDatabases().stream().anyMatch(d -> d.name().equals(name)));
                var files = client.listFiles();
                var file = files.stream().filter(f -> f.name().equals(name)).findFirst();
                assertTrue(file.isPresent());
                client.loadDatabase(name, file.get().file());
                assertTrue(client.listDatabases().stream().anyMatch(d -> d.name().equals(name)));
            } finally {
                try { client.dropDatabase(name); } catch (Exception ignored) {}
            }
        }
    }

    // -- Schema ----------------------------------------------------------

    @Nested class SchemaTests {
        @Test void getSchemaReturnsLabels() {
            requireServer();
            client.query("CREATE (a:_SchemaTestNode {name: 'a'})-[:_SCHEMA_TEST_REL]->(b:_SchemaTestNode {name: 'b'})",
                    null, testDatabase);
            try {
                var schema = client.getSchema(testDatabase);
                assertTrue(schema.labels().contains("_SchemaTestNode"));
                assertTrue(schema.relationshipTypes().contains("_SCHEMA_TEST_REL"));
            } finally {
                client.query("MATCH (n:_SchemaTestNode) DETACH DELETE n", null, testDatabase);
            }
        }
    }

    // -- Aliases ---------------------------------------------------------

    @Nested class AliasTests {
        @Test void createAndDropRoundtrip() {
            requireServer();
            var alias = "_httptest_alias_" + UUID.randomUUID().toString().substring(0, 8);
            client.createAlias(alias, testDatabase);
            try {
                assertTrue(client.listAliases().stream()
                        .anyMatch(a -> a.alias().equals(alias) && a.database().equals(testDatabase)));
            } finally {
                client.dropAlias(alias);
            }
            assertFalse(client.listAliases().stream().anyMatch(a -> a.alias().equals(alias)));
        }

        @Test void alterChangesTarget() {
            requireServer();
            var alias = "_httptest_alias_" + UUID.randomUUID().toString().substring(0, 8);
            var db2 = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(db2);
            try {
                client.createAlias(alias, testDatabase);
                client.alterAlias(alias, db2);
                assertTrue(client.listAliases().stream()
                        .anyMatch(a -> a.alias().equals(alias) && a.database().equals(db2)));
                client.dropAlias(alias);
            } finally {
                try { client.dropDatabase(db2); } catch (Exception ignored) {}
            }
        }
    }

    // -- Import ----------------------------------------------------------

    @Nested class ImportTests {
        @Test void importCypherCreatesNodes() {
            requireServer();
            var cypher = "CREATE (:_ImportTest {id: 1});\nCREATE (:_ImportTest {id: 2});\nCREATE (:_ImportTest {id: 3});";
            var result = client.importCypher(cypher, testDatabase);
            try {
                assertEquals(3, result.executed());
                assertTrue(result.errors().isEmpty());
            } finally {
                client.query("MATCH (n:_ImportTest) DELETE n", null, testDatabase);
            }
        }

        @Test void importCsvCreatesNodes() {
            requireServer();
            var csv = "name,age\nAlice,30\nBob,25";
            var columns = new CsvImportResult.ColumnMapping[]{
                    new CsvImportResult.ColumnMapping("name", "name", "string"),
                    new CsvImportResult.ColumnMapping("age", "age", "integer"),
            };
            var result = client.importCsv(csv, "_CsvTestNode", columns, testDatabase, null, null);
            try {
                assertEquals(2, result.nodesCreated());
                assertTrue(result.errors().isEmpty());
            } finally {
                client.query("MATCH (n:_CsvTestNode) DELETE n", null, testDatabase);
            }
        }

        @Test void importCsvEdgesCreatesEdges() {
            requireServer();
            client.query("CREATE (:_CsvEdgeSrc {key: 'a'}), (:_CsvEdgeDst {key: 'b'})",
                    null, testDatabase);
            try {
                var csv = "src,dst\na,b";
                var source = new CsvImportResult.EdgeEndpoint("_CsvEdgeSrc", "src", "key");
                var target = new CsvImportResult.EdgeEndpoint("_CsvEdgeDst", "dst", "key");
                var result = client.importCsvEdges(csv, "_CSV_TEST_REL", source, target,
                        null, testDatabase, null, null);
                assertEquals(1, result.edgesCreated());
                assertTrue(result.errors().isEmpty());
            } finally {
                client.query("MATCH (n:_CsvEdgeSrc) DETACH DELETE n", null, testDatabase);
                client.query("MATCH (n:_CsvEdgeDst) DETACH DELETE n", null, testDatabase);
            }
        }
    }

    // -- Users -----------------------------------------------------------

    @Nested class UserTests {
        @Test void createListDeleteRoundtrip() {
            requireServer();
            var username = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createUser(username, "testpass123", "reader");
            try {
                assertTrue(client.listUsers().stream().anyMatch(u -> u.username().equals(username)));
            } finally {
                client.deleteUser(username);
            }
            assertFalse(client.listUsers().stream().anyMatch(u -> u.username().equals(username)));
        }

        @Test void updateUserChangesRole() {
            requireServer();
            var username = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createUser(username, "testpass123", "reader");
            try {
                client.updateUser(username, null, "admin");
                var user = client.listUsers().stream()
                        .filter(u -> u.username().equals(username)).findFirst();
                assertTrue(user.isPresent());
                assertEquals("admin", user.get().role());
            } finally {
                client.deleteUser(username);
            }
        }
    }

    // -- Save ------------------------------------------------------------

    @Nested class SaveTests {
        @Test void saveSucceeds() {
            requireServer();
            assertDoesNotThrow(() -> client.save());
        }
    }

    // -- Files -----------------------------------------------------------

    @Nested class FileTests {
        @Test void listFilesReturnsArray() {
            requireServer();
            var files = client.listFiles();
            assertNotNull(files);
        }

        @Test void downloadFileStreamsBytes() {
            requireServer();
            var bytes = client.downloadFile(testDatabase);
            assertTrue(bytes.length > 0);
        }

        @Test void uploadAndDeleteFileRoundtrip() {
            requireServer();
            var name = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            // Create a temp db, download its tvdb, then use it for upload test
            var tempDb = "_httptest_src_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(tempDb);
            try {
                var tvdbBytes = client.downloadFile(tempDb);
                client.dropDatabase(tempDb);
                tempDb = null;

                var upload = client.uploadFile(name, new ByteArrayInputStream(tvdbBytes));
                assertEquals(name, upload.name());
                try {
                    assertTrue(client.listFiles().stream()
                            .anyMatch(f -> f.name().equals(name) && !f.loaded()));
                } finally {
                    client.deleteFile(name);
                }
                assertFalse(client.listFiles().stream().anyMatch(f -> f.name().equals(name)));
            } finally {
                if (tempDb != null) try { client.dropDatabase(tempDb); } catch (Exception ignored) {}
            }
        }
    }

    // -- Tasks -----------------------------------------------------------

    @Nested class TaskTests {
        @Test void asyncSaveSucceeds() {
            requireServer();
            assertDoesNotThrow(() -> client.save());
        }

        @Test void getTaskNotFoundThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.getTask("tsk_0000000000000000"));
            assertEquals(404, ex.statusCode());
        }

        @Test void asyncUnloadAndReloadRoundtrip() {
            requireServer();
            var name = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(name);
            try {
                client.query("CREATE (:_TaskMarker {v: 42})", null, name);
                client.unloadDatabase(name);
                assertFalse(client.listDatabases().stream().anyMatch(d -> d.name().equals(name)));
                var file = client.listFiles().stream().filter(f -> f.name().equals(name)).findFirst();
                assertTrue(file.isPresent());
                client.loadDatabase(name, file.get().file());
                var result = client.query("MATCH (n:_TaskMarker) RETURN n.v AS v", null, name);
                assertEquals(1, result.rows().size());
                assertEquals(42, result.rows().getFirst().getFirst().asInt());
            } finally {
                try { client.dropDatabase(name); } catch (Exception ignored) {}
            }
        }
    }

    // -- Errors ----------------------------------------------------------

    @Nested class ErrorTests {
        @Test void invalidBaseUrlThrows() {
            var bad = new TraverseHttpClient("http://localhost:19999",
                    new TraverseHttpOptions(null, Duration.ofSeconds(2), null, false));
            assertThrows(Exception.class, bad::health);
            bad.close();
        }

        @Test void createDuplicateDatabaseThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.createDatabase(testDatabase));
            assertTrue(ex.getMessage().contains("already exists"));
        }

        @Test void dropNonexistentDatabaseThrows() {
            requireServer();
            assertThrows(TraverseHttpException.class, () ->
                    client.dropDatabase("_httptest_does_not_exist_ever"));
        }

        @Test void renameToExistingNameThrows() {
            requireServer();
            var temp = "_httptest_" + UUID.randomUUID().toString().substring(0, 8);
            client.createDatabase(temp);
            try {
                var ex = assertThrows(TraverseHttpException.class, () ->
                        client.renameDatabase(temp, testDatabase));
                assertTrue(ex.getMessage().contains("already exists"));
            } finally {
                try { client.dropDatabase(temp); } catch (Exception ignored) {}
            }
        }

        @Test void renameNonexistentDatabaseThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.renameDatabase("_httptest_does_not_exist_ever", "whatever"));
            assertTrue(ex.getMessage().contains("does not exist"));
        }

        @Test void createDuplicateAliasThrows() {
            requireServer();
            var alias = "_httptest_alias_" + UUID.randomUUID().toString().substring(0, 8);
            client.createAlias(alias, testDatabase);
            try {
                var ex = assertThrows(TraverseHttpException.class, () ->
                        client.createAlias(alias, testDatabase));
                assertTrue(ex.getMessage().contains("already exists"));
            } finally {
                client.dropAlias(alias);
            }
        }

        @Test void createAliasToNonexistentDbThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.createAlias("_httptest_alias_bad", "_httptest_does_not_exist_ever"));
            assertTrue(ex.getMessage().contains("does not exist"));
        }

        @Test void dropNonexistentAliasThrows() {
            requireServer();
            assertThrows(TraverseHttpException.class, () ->
                    client.dropAlias("_httptest_alias_does_not_exist"));
        }

        @Test void deleteNonexistentUserThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.deleteUser("_httptest_user_does_not_exist"));
            assertTrue(ex.getMessage().contains("does not exist"));
        }

        @Test void queryNonexistentDatabaseThrows() {
            requireServer();
            assertThrows(TraverseHttpException.class, () ->
                    client.query("RETURN 1", null, "_httptest_does_not_exist_ever"));
        }

        @Test void deleteLoadedFileThrows() {
            requireServer();
            var ex = assertThrows(TraverseHttpException.class, () ->
                    client.deleteFile(testDatabase));
            assertTrue(ex.getMessage().contains("loaded"));
        }
    }
}
