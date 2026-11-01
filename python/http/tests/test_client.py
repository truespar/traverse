"""Tests for the Traverse HTTP client. Requires a running server on localhost:7691."""

from __future__ import annotations

import os
import uuid

import pytest

from traverse_http import (
    ColumnMapping,
    EdgeEndpoint,
    TraverseHttpClient,
    TraverseHttpException,
)

BASE_URL = os.environ.get("TRAVERSE_BASE_URL", "http://localhost:7691")


@pytest.fixture(scope="session")
def client():
    """Create a client, check server availability, yield, then close."""
    try:
        probe = TraverseHttpClient(BASE_URL, timeout=3.0)
        health = probe.health()
        if health.status != "ok":
            pytest.skip("Traverse server not healthy")
        probe.close()
    except Exception:
        pytest.skip(f"Traverse server not available at {BASE_URL}")

    api_key = os.environ.get("TRAVERSE_API_KEY") if health.auth_required else None
    c = TraverseHttpClient(
        BASE_URL, api_key=api_key, timeout=30.0, transfer_timeout=60.0
    )
    yield c
    c.close()


@pytest.fixture(scope="session")
def test_db(client: TraverseHttpClient):
    """Create an isolated test database, drop it after all tests."""
    name = f"_httptest_{uuid.uuid4().hex[:8]}"
    client.create_database(name)
    yield name
    try:
        client.drop_database(name)
    except Exception:
        pass


def _uid() -> str:
    return uuid.uuid4().hex[:8]


# -- Health ----------------------------------------------------------


class TestHealth:
    def test_health_returns_status_and_version(self, client: TraverseHttpClient):
        h = client.health()
        assert h.status == "ok"
        assert h.version

    def test_ready_returns_true(self, client: TraverseHttpClient):
        assert client.ready() is True

    def test_health_reports_features(self, client: TraverseHttpClient):
        h = client.health()
        # The features map is present on all current servers; every
        # value is a bool (gql, grpc, tls, mcp, ml).
        assert isinstance(h.features, dict)
        assert "gql" in h.features
        assert all(isinstance(v, bool) for v in h.features.values())


# -- Query -----------------------------------------------------------


class TestQuery:
    def test_return_scalar(self, client: TraverseHttpClient, test_db: str):
        result = client.query("RETURN 42 AS n", database=test_db)
        assert len(result.rows) == 1
        assert result.rows[0][0] == 42

    def test_return_string(self, client: TraverseHttpClient, test_db: str):
        result = client.query("RETURN 'hello' AS s", database=test_db)
        assert result.rows[0][0] == "hello"

    def test_with_parameters(self, client: TraverseHttpClient, test_db: str):
        result = client.query("RETURN $x AS val", parameters={"x": 99}, database=test_db)
        assert result.rows[0][0] == 99

    def test_gql_dialect(self, client: TraverseHttpClient, test_db: str):
        if not client.health().features.get("gql"):
            pytest.skip("server built without the gql feature")
        # `||` concat and `IS INTEGER` are GQL-only syntax - they parse
        # only when the dialect reaches the server.
        result = client.query(
            "RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i",
            database=test_db,
            dialect="gql",
        )
        assert result.rows[0] == ["foobar", True]

    def test_write_query_stats(self, client: TraverseHttpClient, test_db: str):
        result = client.query("CREATE (n:_HttpTest) RETURN n", database=test_db)
        try:
            assert result.stats.nodes_created == 1
        finally:
            client.query("MATCH (n:_HttpTest) DELETE n", database=test_db)

    def test_syntax_error_throws(self, client: TraverseHttpClient, test_db: str):
        with pytest.raises(TraverseHttpException) as exc_info:
            client.query("THIS IS NOT CYPHER", database=test_db)
        assert exc_info.value.status_code == 400


# -- Databases -------------------------------------------------------


class TestDatabases:
    def test_list_databases_contains_test_db(
        self, client: TraverseHttpClient, test_db: str
    ):
        dbs = client.list_databases()
        assert any(d.name == test_db for d in dbs)

    def test_create_and_drop_roundtrip(self, client: TraverseHttpClient):
        name = f"_httptest_{_uid()}"
        client.create_database(name)
        assert any(d.name == name for d in client.list_databases())
        client.drop_database(name)
        assert not any(d.name == name for d in client.list_databases())

    def test_rename_roundtrip(self, client: TraverseHttpClient):
        original = f"_httptest_{_uid()}"
        renamed = f"_httptest_{_uid()}"
        client.create_database(original)
        try:
            client.rename_database(original, renamed)
            assert not any(d.name == original for d in client.list_databases())
            assert any(d.name == renamed for d in client.list_databases())
        finally:
            for n in (renamed, original):
                try:
                    client.drop_database(n)
                except Exception:
                    pass

    def test_unload_and_load_roundtrip(self, client: TraverseHttpClient):
        name = f"_httptest_{_uid()}"
        client.create_database(name)
        try:
            client.unload_database(name)
            assert not any(d.name == name for d in client.list_databases())
            files = client.list_files()
            f = next((f for f in files if f.name == name), None)
            assert f is not None
            client.load_database(name, f.file)
            assert any(d.name == name for d in client.list_databases())
        finally:
            try:
                client.drop_database(name)
            except Exception:
                pass


# -- Schema ----------------------------------------------------------


class TestSchema:
    def test_get_schema_returns_labels(
        self, client: TraverseHttpClient, test_db: str
    ):
        client.query(
            "CREATE (a:_SchemaTestNode {name: 'a'})"
            "-[:_SCHEMA_TEST_REL]->"
            "(b:_SchemaTestNode {name: 'b'})",
            database=test_db,
        )
        try:
            schema = client.get_schema(test_db)
            assert "_SchemaTestNode" in schema.labels
            assert "_SCHEMA_TEST_REL" in schema.relationship_types
        finally:
            client.query(
                "MATCH (n:_SchemaTestNode) DETACH DELETE n", database=test_db
            )


# -- Aliases ---------------------------------------------------------


class TestAliases:
    def test_create_and_drop_roundtrip(
        self, client: TraverseHttpClient, test_db: str
    ):
        alias = f"_httptest_alias_{_uid()}"
        client.create_alias(alias, test_db)
        try:
            assert any(
                a.alias == alias and a.database == test_db
                for a in client.list_aliases()
            )
        finally:
            client.drop_alias(alias)
        assert not any(a.alias == alias for a in client.list_aliases())

    def test_alter_changes_target(
        self, client: TraverseHttpClient, test_db: str
    ):
        alias = f"_httptest_alias_{_uid()}"
        db2 = f"_httptest_{_uid()}"
        client.create_database(db2)
        try:
            client.create_alias(alias, test_db)
            client.alter_alias(alias, db2)
            assert any(
                a.alias == alias and a.database == db2
                for a in client.list_aliases()
            )
            client.drop_alias(alias)
        finally:
            try:
                client.drop_database(db2)
            except Exception:
                pass


# -- Import ----------------------------------------------------------


class TestImport:
    def test_import_cypher_creates_nodes(
        self, client: TraverseHttpClient, test_db: str
    ):
        cypher = (
            "CREATE (:_ImportTest {id: 1});\n"
            "CREATE (:_ImportTest {id: 2});\n"
            "CREATE (:_ImportTest {id: 3});"
        )
        result = client.import_cypher(cypher, database=test_db)
        try:
            assert result.executed == 3
            assert len(result.errors) == 0
        finally:
            client.query("MATCH (n:_ImportTest) DELETE n", database=test_db)

    def test_import_csv_creates_nodes(
        self, client: TraverseHttpClient, test_db: str
    ):
        csv = "name,age\nAlice,30\nBob,25"
        columns = [
            ColumnMapping("name", "name", "string"),
            ColumnMapping("age", "age", "integer"),
        ]
        result = client.import_csv(csv, "_CsvTestNode", columns, database=test_db)
        try:
            assert result.nodes_created == 2
            assert len(result.errors) == 0
        finally:
            client.query("MATCH (n:_CsvTestNode) DELETE n", database=test_db)

    def test_import_csv_edges_creates_edges(
        self, client: TraverseHttpClient, test_db: str
    ):
        client.query(
            "CREATE (:_CsvEdgeSrc {key: 'a'}), (:_CsvEdgeDst {key: 'b'})",
            database=test_db,
        )
        try:
            csv = "src,dst\na,b"
            source = EdgeEndpoint("_CsvEdgeSrc", "src", "key")
            target = EdgeEndpoint("_CsvEdgeDst", "dst", "key")
            result = client.import_csv_edges(
                csv, "_CSV_TEST_REL", source, target, database=test_db
            )
            assert result.edges_created == 1
            assert len(result.errors) == 0
        finally:
            client.query(
                "MATCH (n:_CsvEdgeSrc) DETACH DELETE n", database=test_db
            )
            client.query(
                "MATCH (n:_CsvEdgeDst) DETACH DELETE n", database=test_db
            )


# -- Users -----------------------------------------------------------


class TestUsers:
    def test_create_list_delete_roundtrip(self, client: TraverseHttpClient):
        username = f"_httptest_{_uid()}"
        client.create_user(username, "testpass123", "reader")
        try:
            assert any(u.username == username for u in client.list_users())
        finally:
            client.delete_user(username)
        assert not any(u.username == username for u in client.list_users())

    def test_update_user_changes_role(self, client: TraverseHttpClient):
        username = f"_httptest_{_uid()}"
        client.create_user(username, "testpass123", "reader")
        try:
            client.update_user(username, role="admin")
            user = next(
                (u for u in client.list_users() if u.username == username), None
            )
            assert user is not None
            assert user.role == "admin"
        finally:
            client.delete_user(username)


# -- Save ------------------------------------------------------------


class TestSave:
    def test_save_succeeds(self, client: TraverseHttpClient):
        client.save()


# -- Files -----------------------------------------------------------


class TestFiles:
    def test_list_files_returns_list(self, client: TraverseHttpClient):
        files = client.list_files()
        assert isinstance(files, list)

    def test_download_file_streams_bytes(
        self, client: TraverseHttpClient, test_db: str
    ):
        data = client.download_file(test_db)
        assert len(data) > 0

    def test_upload_and_delete_file_roundtrip(self, client: TraverseHttpClient):
        name = f"_httptest_{_uid()}"
        temp_db = f"_httptest_src_{_uid()}"
        client.create_database(temp_db)
        try:
            tvdb_bytes = client.download_file(temp_db)
            client.drop_database(temp_db)
            temp_db = None

            upload = client.upload_file(name, tvdb_bytes)
            assert upload.name == name
            try:
                assert any(
                    f.name == name and not f.loaded for f in client.list_files()
                )
            finally:
                client.delete_file(name)
            assert not any(f.name == name for f in client.list_files())
        finally:
            if temp_db is not None:
                try:
                    client.drop_database(temp_db)
                except Exception:
                    pass


# -- Tasks -----------------------------------------------------------


class TestTasks:
    def test_async_save_succeeds(self, client: TraverseHttpClient):
        client.save()

    def test_get_task_not_found_throws(self, client: TraverseHttpClient):
        with pytest.raises(TraverseHttpException) as exc_info:
            client.get_task("tsk_0000000000000000")
        assert exc_info.value.status_code == 404

    def test_async_unload_and_reload_roundtrip(
        self, client: TraverseHttpClient
    ):
        name = f"_httptest_{_uid()}"
        client.create_database(name)
        try:
            client.query("CREATE (:_TaskMarker {v: 42})", database=name)
            client.unload_database(name)
            assert not any(d.name == name for d in client.list_databases())
            f = next(
                (f for f in client.list_files() if f.name == name), None
            )
            assert f is not None
            client.load_database(name, f.file)
            result = client.query(
                "MATCH (n:_TaskMarker) RETURN n.v AS v", database=name
            )
            assert len(result.rows) == 1
            assert result.rows[0][0] == 42
        finally:
            try:
                client.drop_database(name)
            except Exception:
                pass


# -- Errors ----------------------------------------------------------


class TestErrors:
    def test_invalid_base_url_throws(self):
        bad = TraverseHttpClient("http://localhost:19999", timeout=2.0)
        with pytest.raises(Exception):
            bad.health()
        bad.close()

    def test_create_duplicate_database_throws(
        self, client: TraverseHttpClient, test_db: str
    ):
        with pytest.raises(TraverseHttpException, match="already exists"):
            client.create_database(test_db)

    def test_drop_nonexistent_database_throws(
        self, client: TraverseHttpClient
    ):
        with pytest.raises(TraverseHttpException):
            client.drop_database("_httptest_does_not_exist_ever")

    def test_rename_to_existing_name_throws(
        self, client: TraverseHttpClient, test_db: str
    ):
        temp = f"_httptest_{_uid()}"
        client.create_database(temp)
        try:
            with pytest.raises(TraverseHttpException, match="already exists"):
                client.rename_database(temp, test_db)
        finally:
            try:
                client.drop_database(temp)
            except Exception:
                pass

    def test_rename_nonexistent_database_throws(
        self, client: TraverseHttpClient
    ):
        with pytest.raises(TraverseHttpException, match="does not exist"):
            client.rename_database("_httptest_does_not_exist_ever", "whatever")

    def test_create_duplicate_alias_throws(
        self, client: TraverseHttpClient, test_db: str
    ):
        alias = f"_httptest_alias_{_uid()}"
        client.create_alias(alias, test_db)
        try:
            with pytest.raises(TraverseHttpException, match="already exists"):
                client.create_alias(alias, test_db)
        finally:
            client.drop_alias(alias)

    def test_create_alias_to_nonexistent_db_throws(
        self, client: TraverseHttpClient
    ):
        with pytest.raises(TraverseHttpException, match="does not exist"):
            client.create_alias(
                "_httptest_alias_bad", "_httptest_does_not_exist_ever"
            )

    def test_drop_nonexistent_alias_throws(self, client: TraverseHttpClient):
        with pytest.raises(TraverseHttpException):
            client.drop_alias("_httptest_alias_does_not_exist")

    def test_delete_nonexistent_user_throws(
        self, client: TraverseHttpClient
    ):
        with pytest.raises(TraverseHttpException, match="does not exist"):
            client.delete_user("_httptest_user_does_not_exist")

    def test_query_nonexistent_database_throws(
        self, client: TraverseHttpClient
    ):
        with pytest.raises(TraverseHttpException):
            client.query("RETURN 1", database="_httptest_does_not_exist_ever")

    def test_delete_loaded_file_throws(
        self, client: TraverseHttpClient, test_db: str
    ):
        with pytest.raises(TraverseHttpException, match="loaded"):
            client.delete_file(test_db)
