"""Tests for the traverse Python bindings."""

import gc
import math
import os
import tempfile
import threading
import time

import pytest

import traverse


@pytest.fixture
def db_path(tmp_path):
    """Return a path for a temporary database."""
    return str(tmp_path / "test.tvdb")


@pytest.fixture
def db(db_path):
    """Open a temporary database, close on teardown."""
    database = traverse.open(db_path)
    yield database
    database.close()


# ---------------------------------------------------------------------------
# Open / Database
# ---------------------------------------------------------------------------


class TestOpen:
    def test_open_and_flush_creates_file(self, db_path):
        db = traverse.open(db_path)
        with db.begin() as tx:
            tx.execute("CREATE (n:Test)")
        db.flush()
        db.close()
        assert os.path.exists(db_path)

    def test_reopen_existing_database(self, db_path):
        db = traverse.open(db_path)
        db.flush()
        db.close()
        db2 = traverse.open(db_path)
        db2.flush()
        db2.close()

    def test_open_context_manager(self, db_path):
        with traverse.open(db_path) as db:
            with db.begin() as tx:
                tx.execute("CREATE (n:Test {x: 1})")

    def test_open_invalid_path(self):
        with pytest.raises(traverse.TraverseError):
            traverse.open("/nonexistent/dir/test.tvdb")

    def test_open_empty_path(self):
        with pytest.raises(traverse.TraverseError):
            traverse.open("")

    def test_double_close(self, db_path):
        db = traverse.open(db_path)
        db.close()
        db.close()  # should not raise

    def test_flush_after_close_raises(self, db_path):
        db = traverse.open(db_path)
        db.close()
        with pytest.raises(Exception):
            db.flush()


# ---------------------------------------------------------------------------
# Transaction
# ---------------------------------------------------------------------------


class TestTransaction:
    def test_create_and_query(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:Person {name: 'Alice', age: 30})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Person) RETURN n.name, n.age")
            rows = list(result)
            assert len(rows) == 1
            assert rows[0]["n.name"] == "Alice"
            assert rows[0]["n.age"] == 30

    def test_explicit_commit(self, db):
        tx = db.begin()
        tx.execute("CREATE (n:Foo {val: 42})")
        tx.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Foo) RETURN n.val")
            assert len(list(result)) == 1

    def test_explicit_abort(self, db):
        tx = db.begin()
        tx.execute("CREATE (n:Aborted {val: 99})")
        tx.abort()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Aborted) RETURN n.val")
            assert len(list(result)) == 0

    def test_context_manager_auto_commits(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:Temp {val: 1})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Temp) RETURN n.val")
            rows = list(result)
            # context manager commits on clean exit
            assert len(rows) == 1

    def test_context_manager_aborts_on_exception(self, db):
        with pytest.raises(ValueError):
            with db.begin() as tx:
                tx.execute("CREATE (n:Bad {val: 1})")
                raise ValueError("oops")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Bad) RETURN n")
            assert len(list(result)) == 0

    def test_commit_after_commit_raises(self, db):
        tx = db.begin()
        tx.commit()
        with pytest.raises(RuntimeError):
            tx.commit()

    def test_execute_after_commit_raises(self, db):
        tx = db.begin()
        tx.commit()
        with pytest.raises(RuntimeError):
            tx.execute("RETURN 1")

    def test_syntax_error(self, db):
        with db.begin() as tx:
            with pytest.raises(traverse.TraverseError):
                tx.execute("INVALID SYNTAX HERE")

    def test_multiple_queries_in_single_transaction(self, db):
        tx = db.begin()
        tx.execute("CREATE (a:Person {name: 'Alice'})")
        tx.execute("CREATE (b:Person {name: 'Bob'})")
        tx.execute(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) "
            "CREATE (a)-[:KNOWS]->(b)"
        )
        result = tx.execute("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name")
        rows = list(result)
        assert len(rows) == 1
        assert rows[0]["a.name"] == "Alice"
        assert rows[0]["b.name"] == "Bob"
        tx.commit()

    def test_unknown_function_raises(self, db):
        with db.begin() as tx:
            with pytest.raises(traverse.TraverseError):
                tx.execute("RETURN no_such_function(1)")


# ---------------------------------------------------------------------------
# QueryResult
# ---------------------------------------------------------------------------


class TestQueryResult:
    def test_columns(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 1 AS x, 'hello' AS y")
            assert result.columns == ["x", "y"]

    def test_row_count(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (a:N), (b:N), (c:N)")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:N) RETURN n")
            assert result.row_count == 3
            assert len(result) == 3

    def test_iteration(self, db):
        with db.begin() as tx:
            result = tx.execute("UNWIND [1, 2, 3] AS x RETURN x")
            values = [row["x"] for row in result]
            assert values == [1, 2, 3]

    def test_data_method(self, db):
        with db.begin() as tx:
            result = tx.execute("UNWIND ['a', 'b'] AS x RETURN x")
            data = result.data()
            assert len(data) == 2
            assert data[0]["x"] == "a"
            assert data[1]["x"] == "b"

    def test_stats(self, db):
        with db.begin() as tx:
            result = tx.execute("CREATE (n:S {val: 1})")
            assert result.stats["nodes_created"] == 1

    def test_repr(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 1 AS x")
            r = repr(result)
            assert "QueryResult" in r
            assert "rows=1" in r


# ---------------------------------------------------------------------------
# Data Types
# ---------------------------------------------------------------------------


class TestDataTypes:
    def test_null(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN null AS x")
            assert list(result)[0]["x"] is None

    def test_bool(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN true AS x, false AS y")
            row = list(result)[0]
            assert row["x"] is True
            assert row["y"] is False

    def test_integer(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 42 AS x")
            assert list(result)[0]["x"] == 42

    def test_float(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 3.14 AS x")
            assert abs(list(result)[0]["x"] - 3.14) < 0.001

    def test_string(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 'hello' AS x")
            assert list(result)[0]["x"] == "hello"

    def test_list(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN [1, 2, 3] AS x")
            assert list(result)[0]["x"] == [1, 2, 3]

    def test_map(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN {a: 1, b: 'two'} AS x")
            row = list(result)[0]
            assert row["x"]["a"] == 1
            assert row["x"]["b"] == "two"

    def test_node(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:Person {name: 'Bob'})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Person) RETURN n")
            row = list(result)[0]
            node = row["n"]
            assert "_id" in node
            assert "Person" in node["_labels"]
            assert node["name"] == "Bob"

    def test_edge(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (a:A)-[:KNOWS {since: 2020}]->(b:B)")

        with db.begin() as tx:
            result = tx.execute("MATCH ()-[r:KNOWS]->() RETURN r")
            row = list(result)[0]
            edge = row["r"]
            assert "_type" in edge
            assert edge["_type"] == "KNOWS"
            assert edge["since"] == 2020

    def test_path(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (a:X {id: 1})-[:R]->(b:X {id: 2})")

        with db.begin() as tx:
            result = tx.execute(
                "MATCH p = (a:X {id: 1})-[:R]->(b:X {id: 2}) RETURN p"
            )
            row = list(result)[0]
            path = row["p"]
            assert "nodes" in path
            assert "edges" in path
            assert len(path["nodes"]) == 2
            assert len(path["edges"]) == 1


# ---------------------------------------------------------------------------
# Property Edge Cases
# ---------------------------------------------------------------------------


class TestPropertyEdgeCases:
    def test_unicode_string_roundtrips(self, db):
        with db.begin() as tx:
            tx.execute(
                "CREATE (n:I18n {name: '日本語テスト', emoji: '🚀🔥', arabic: 'مرحبا'})"
            )

        with db.begin() as tx:
            result = tx.execute("MATCH (n:I18n) RETURN n.name, n.emoji, n.arabic")
            row = list(result)[0]
            assert row["n.name"] == "日本語テスト"
            assert row["n.emoji"] == "🚀🔥"
            assert row["n.arabic"] == "مرحبا"

    def test_empty_string_property(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:E {val: ''})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:E) RETURN n.val")
            assert list(result)[0]["n.val"] == ""

    def test_large_string_property(self, db):
        big = "x" * 100_000
        with db.begin() as tx:
            tx.execute("CREATE (n:Big {val: $s})", {"s": big})

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Big) RETURN n.val")
            assert list(result)[0]["n.val"] == big

    def test_negative_integer(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN -9999999 AS val")
            assert list(result)[0]["val"] == -9999999

    def test_large_integer_boundary(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 9223372036854775807 AS val")
            assert list(result)[0]["val"] == 2**63 - 1

    def test_negative_float(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN -273.15 AS val")
            assert abs(list(result)[0]["val"] - (-273.15)) < 0.001

    def test_zero_values(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 0 AS i, 0.0 AS f, false AS b")
            row = list(result)[0]
            assert row["i"] == 0
            assert row["f"] == 0.0
            assert row["b"] is False

    def test_multiple_null_columns(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN null AS a, null AS b, null AS c")
            row = list(result)[0]
            assert row["a"] is None
            assert row["b"] is None
            assert row["c"] is None

    def test_mixed_null_and_values(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN 42 AS a, null AS b, 'hello' AS c")
            row = list(result)[0]
            assert row["a"] == 42
            assert row["b"] is None
            assert row["c"] == "hello"

    def test_special_characters_in_string(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:Special {val: 'line1\\nline2\\ttab'})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Special) RETURN n.val")
            val = list(result)[0]["n.val"]
            assert "line1" in val

    def test_bulk_writer_unicode_properties(self, db):
        writer = db.bulk_writer()
        writer.create_node(["I18n"], {"name": "Ñoño", "city": "München"})
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:I18n) RETURN n.name, n.city")
            row = list(result)[0]
            assert row["n.name"] == "Ñoño"
            assert row["n.city"] == "München"


# ---------------------------------------------------------------------------
# Query Parameters
# ---------------------------------------------------------------------------


class TestQueryParameters:
    def test_string_param(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:P {name: 'Alice'})")

        with db.begin() as tx:
            result = tx.execute(
                "MATCH (n:P) WHERE n.name = $name RETURN n.name",
                {"name": "Alice"},
            )
            assert list(result)[0]["n.name"] == "Alice"

    def test_int_param(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN $x + 1 AS y", {"x": 41})
            assert list(result)[0]["y"] == 42

    def test_null_param(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN $x IS NULL AS y", {"x": None})
            assert list(result)[0]["y"] is True

    def test_list_param(self, db):
        with db.begin() as tx:
            result = tx.execute(
                "UNWIND $items AS x RETURN x", {"items": [10, 20, 30]}
            )
            values = [row["x"] for row in result]
            assert values == [10, 20, 30]


# ---------------------------------------------------------------------------
# GQL dialect
# ---------------------------------------------------------------------------


class TestGqlDialect:
    def test_gql_only_syntax(self, db):
        # `||` concat and `IS INTEGER` are GQL-only - the openCypher
        # parser rejects both, so this passing proves the dialect
        # option reached the GQL front-end.
        with db.begin() as tx:
            result = tx.execute(
                "RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i", dialect="gql"
            )
            row = list(result)[0]
            assert row["s"] == "foobar"
            assert row["i"] is True

    def test_gql_with_params(self, db):
        with db.begin() as tx:
            result = tx.execute("RETURN $x + 1 AS n", {"x": 41}, dialect="gql")
            assert list(result)[0]["n"] == 42

    def test_gql_insert_and_filter(self, db):
        with db.begin() as tx:
            tx.execute("INSERT (:Person {name: 'Ada', age: 36})", dialect="gql")
            result = tx.execute(
                "MATCH (p:Person) FILTER p.age > 10 RETURN p.name AS name",
                dialect="gql",
            )
            assert list(result)[0]["name"] == "Ada"

    def test_cypher_rejects_gql_syntax(self, db):
        with db.begin() as tx:
            with pytest.raises(traverse.TraverseError):
                tx.execute("RETURN 1 IS INTEGER AS i")

    def test_unknown_dialect_raises(self, db):
        with db.begin() as tx:
            with pytest.raises(traverse.TraverseError):
                tx.execute("RETURN 1", dialect="sql")


# ---------------------------------------------------------------------------
# BulkWriter
# ---------------------------------------------------------------------------


class TestBulkWriter:
    def test_create_nodes(self, db):
        writer = db.bulk_writer()
        nid1 = writer.create_node(["Person"], {"name": "Alice", "age": 30})
        nid2 = writer.create_node(["Person"], {"name": "Bob", "age": 25})
        assert isinstance(nid1, int)
        assert isinstance(nid2, int)
        assert nid1 != nid2
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Person) RETURN n.name ORDER BY n.name")
            names = [row["n.name"] for row in result]
            assert names == ["Alice", "Bob"]

    def test_create_node_multiple_labels(self, db):
        writer = db.bulk_writer()
        writer.create_node(["Person", "Employee"], {"name": "Bob"})
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Person:Employee) RETURN n.name")
            rows = list(result)
            assert len(rows) == 1
            assert rows[0]["n.name"] == "Bob"

    def test_create_edges(self, db):
        writer = db.bulk_writer()
        a = writer.create_node(["A"])
        b = writer.create_node(["B"])
        eid = writer.create_edge(a, b, "KNOWS", {"since": 2020})
        assert isinstance(eid, int)
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH ()-[r:KNOWS]->() RETURN r.since")
            assert list(result)[0]["r.since"] == 2020

    def test_create_edge_no_properties(self, db):
        writer = db.bulk_writer()
        a = writer.create_node(["X"])
        b = writer.create_node(["X"])
        writer.create_edge(a, b, "LINKS")
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH ()-[e:LINKS]->() RETURN count(e)")
            assert list(result)[0]["count(e)"] == 1

    def test_create_index(self, db):
        writer = db.bulk_writer()
        writer.create_node(["Person"], {"name": "Test"})
        writer.create_index("idx_person_name", "Person", "name")
        writer.commit()

    def test_create_edge_index(self, db):
        writer = db.bulk_writer()
        a = writer.create_node(["X"])
        b = writer.create_node(["Y"])
        writer.create_edge(a, b, "REL", {"w": 1})
        writer.create_edge_index("idx_rel_w", "REL", "w")
        writer.commit()

    def test_drop_edge_index(self, db):
        writer = db.bulk_writer()
        a = writer.create_node(["X"])
        b = writer.create_node(["Y"])
        writer.create_edge(a, b, "REL", {"w": 1})
        writer.create_edge_index("idx_rel_w", "REL", "w")
        writer.commit()

        writer2 = db.bulk_writer()
        writer2.drop_edge_index("idx_rel_w")
        writer2.commit()

    def test_context_manager(self, db):
        with db.bulk_writer() as writer:
            writer.create_node(["CM"], {"x": 1})

        with db.begin() as tx:
            result = tx.execute("MATCH (n:CM) RETURN n.x")
            assert list(result)[0]["n.x"] == 1

    def test_abort(self, db):
        writer = db.bulk_writer()
        writer.create_node(["Nope"])
        writer.abort()

        with db.begin_read() as tx:
            result = tx.execute("MATCH (n:Nope) RETURN count(n)")
            assert list(result)[0]["count(n)"] == 0

    def test_context_manager_rolls_back_on_exception(self, db):
        with pytest.raises(RuntimeError):
            with db.bulk_writer() as writer:
                writer.create_node(["Rolled"])
                raise RuntimeError("boom")

        with db.begin_read() as tx:
            result = tx.execute("MATCH (n:Rolled) RETURN count(n)")
            assert list(result)[0]["count(n)"] == 0

    def test_load_is_invisible_until_commit(self, db):
        writer = db.bulk_writer()
        for i in range(100):
            writer.create_node(["Item"], {"i": i})
        with db.begin_read() as tx:
            assert list(tx.execute("MATCH (n:Item) RETURN count(n) AS c"))[0]["c"] == 0
        writer.commit()
        with db.begin_read() as tx:
            assert list(tx.execute("MATCH (n:Item) RETURN count(n) AS c"))[0]["c"] == 100

    def test_node_without_properties(self, db):
        writer = db.bulk_writer()
        nid = writer.create_node(["Empty"])
        assert isinstance(nid, int)
        writer.commit()

    def test_abort_releases_write_lock(self, db):
        writer = db.bulk_writer()
        writer.create_node(["First"])
        writer.abort()

        writer2 = db.bulk_writer()
        writer2.create_node(["Second"])
        writer2.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Second) RETURN count(n)")
            assert list(result)[0]["count(n)"] == 1

    def test_commit_after_commit_raises(self, db):
        writer = db.bulk_writer()
        writer.create_node(["X"])
        writer.commit()
        with pytest.raises(Exception):
            writer.commit()

    def test_create_node_after_commit_raises(self, db):
        writer = db.bulk_writer()
        writer.commit()
        with pytest.raises(Exception):
            writer.create_node(["X"])

    def test_create_edge_invalid_source_raises(self, db):
        writer = db.bulk_writer()
        a = writer.create_node(["X"])
        with pytest.raises(Exception):
            writer.create_edge(a, 999999, "BAD")
        writer.abort()

    def test_bulk_insert_many_nodes(self, db):
        count = 1000
        writer = db.bulk_writer()
        for i in range(count):
            writer.create_node(["Batch"], {"idx": i})
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Batch) RETURN count(n)")
            assert list(result)[0]["count(n)"] == count

    def test_bulk_insert_nodes_and_edges_graph(self, db):
        writer = db.bulk_writer()
        ids = [writer.create_node(["Chain"], {"pos": i}) for i in range(5)]
        for i in range(4):
            writer.create_edge(ids[i], ids[i + 1], "NEXT")
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Chain) RETURN count(n)")
            assert list(result)[0]["count(n)"] == 5
            result = tx.execute("MATCH ()-[e:NEXT]->() RETURN count(e)")
            assert list(result)[0]["count(e)"] == 4
            result = tx.execute(
                "MATCH p = (start:Chain {pos: 0})-[:NEXT*]->(end:Chain) "
                "WHERE NOT (end)-[:NEXT]->() RETURN end.pos"
            )
            assert list(result)[0]["end.pos"] == 4


# ---------------------------------------------------------------------------
# Index / Constraint
# ---------------------------------------------------------------------------


class TestIndex:
    def test_create_index_and_lookup(self, db):
        with db.begin() as tx:
            for i in range(100):
                tx.execute(f"CREATE (n:User {{id: {i}, name: 'user{i}'}})")

        with db.begin() as tx:
            tx.execute("CREATE INDEX idx_user_id FOR (n:User) ON (n.id)")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:User {id: 42}) RETURN n.name")
            rows = list(result)
            assert len(rows) == 1
            assert rows[0]["n.name"] == "user42"

    def test_drop_index(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:Item {code: 'A'})")
            tx.execute("CREATE INDEX idx_item_code FOR (n:Item) ON (n.code)")

        with db.begin() as tx:
            tx.execute("DROP INDEX idx_item_code")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Item) RETURN n.code")
            rows = list(result)
            assert len(rows) == 1
            assert rows[0]["n.code"] == "A"

    def test_unique_constraint(self, db):
        with db.begin() as tx:
            tx.execute(
                "CREATE CONSTRAINT uniq_email "
                "FOR (n:User) REQUIRE n.email IS UNIQUE"
            )
            tx.execute("CREATE (n:User {email: 'alice@test.com'})")

        with db.begin() as tx:
            with pytest.raises(traverse.TraverseError):
                tx.execute("CREATE (n:User {email: 'alice@test.com'})")
            # the failed CREATE had written: only a rollback ends the transaction
            with pytest.raises(traverse.TraverseError, match="roll it back"):
                tx.commit()
        with db.begin_read() as tx:
            assert count(tx, "User") == 1

    def test_index_survives_flush_reopen(self, db_path):
        db = traverse.open(db_path)
        with db.begin() as tx:
            tx.execute("CREATE INDEX idx_name FOR (n:Person) ON (n.name)")
            for i in range(50):
                tx.execute(f"CREATE (n:Person {{name: 'person{i}'}})")
        db.flush()
        db.close()

        db2 = traverse.open(db_path)
        with db2.begin() as tx:
            result = tx.execute("MATCH (n:Person {name: 'person25'}) RETURN n.name")
            rows = list(result)
            assert len(rows) == 1
            assert rows[0]["n.name"] == "person25"
        db2.close()


# ---------------------------------------------------------------------------
# Concurrency / MVCC
# ---------------------------------------------------------------------------


class TestConcurrency:
    def test_multiple_sequential_transactions(self, db):
        for i in range(10):
            with db.begin() as tx:
                tx.execute(f"CREATE (n:Seq {{idx: {i}}})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Seq) RETURN count(n)")
            assert list(result)[0]["count(n)"] == 10

    def test_transaction_after_bulk_write(self, db):
        writer = db.bulk_writer()
        for i in range(100):
            writer.create_node(["Bulk"], {"idx": i})
        writer.commit()

        with db.begin() as tx:
            tx.execute("CREATE (n:Tx {val: 'after_bulk'})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Bulk) RETURN count(n)")
            assert list(result)[0]["count(n)"] == 100
            result = tx.execute("MATCH (n:Tx) RETURN n.val")
            assert list(result)[0]["n.val"] == "after_bulk"

    def test_bulk_write_after_transaction(self, db):
        with db.begin() as tx:
            tx.execute("CREATE (n:Pre {val: 'before_bulk'})")

        writer = db.bulk_writer()
        writer.create_node(["Post"], {"val": "after_tx"})
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Pre) RETURN n.val")
            assert list(result)[0]["n.val"] == "before_bulk"
            result = tx.execute("MATCH (n:Post) RETURN n.val")
            assert list(result)[0]["n.val"] == "after_tx"

    def test_flush_persists_across_reopen(self, db_path):
        db = traverse.open(db_path)
        with db.begin() as tx:
            tx.execute("CREATE (n:Persist {val: 'survived'})")
        db.flush()
        db.close()

        db2 = traverse.open(db_path)
        with db2.begin() as tx:
            result = tx.execute("MATCH (n:Persist) RETURN n.val")
            rows = list(result)
            assert len(rows) == 1
            assert rows[0]["n.val"] == "survived"
        db2.close()


# ---------------------------------------------------------------------------
# Scale
# ---------------------------------------------------------------------------


class TestScale:
    def test_bulk_insert_10k_nodes(self, db):
        count = 10_000
        writer = db.bulk_writer()
        for i in range(count):
            writer.create_node(["User"], {"id": i, "name": f"user_{i}"})
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:User) RETURN count(n)")
            assert list(result)[0]["count(n)"] == count

    def test_bulk_insert_10k_nodes_20k_edges(self, db):
        import random
        rng = random.Random(42)
        node_count = 10_000
        edge_count = 20_000

        writer = db.bulk_writer()
        ids = [writer.create_node(["N"], {"idx": i}) for i in range(node_count)]
        for _ in range(edge_count):
            src = ids[rng.randint(0, node_count - 1)]
            tgt = ids[rng.randint(0, node_count - 1)]
            writer.create_edge(src, tgt, "LINKS")
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:N) RETURN count(n)")
            assert list(result)[0]["count(n)"] == node_count
            result = tx.execute("MATCH ()-[e:LINKS]->() RETURN count(e)")
            assert list(result)[0]["count(e)"] == edge_count

    def test_transaction_insert_1k_with_index(self, db):
        count = 1000
        with db.begin() as tx:
            tx.execute("CREATE INDEX idx_item_id FOR (n:Item) ON (n.id)")

        with db.begin() as tx:
            for i in range(count):
                tx.execute(f"CREATE (n:Item {{id: {i}, data: 'payload_{i}'}})")

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Item) RETURN count(n)")
            assert list(result)[0]["count(n)"] == count
            result = tx.execute("MATCH (n:Item {id: 777}) RETURN n.data")
            assert list(result)[0]["n.data"] == "payload_777"

    def test_large_result_set(self, db):
        count = 5000
        writer = db.bulk_writer()
        for i in range(count):
            writer.create_node(["Row"], {"val": i})
        writer.commit()

        with db.begin() as tx:
            result = tx.execute("MATCH (n:Row) RETURN n.val ORDER BY n.val")
            rows = list(result)
            assert len(rows) == count

    def test_complex_graph_multiple_edge_types(self, db):
        import random
        rng = random.Random(123)

        writer = db.bulk_writer()
        users = [writer.create_node(["User", "Active"], {"name": f"user_{i}", "age": 18 + i % 60}) for i in range(100)]
        posts = [writer.create_node(["Post"], {"title": f"Post title {i}", "likes": i * 3}) for i in range(200)]

        for _ in range(300):
            writer.create_edge(users[rng.randint(0, 99)], users[rng.randint(0, 99)], "FOLLOWS")
        for i in range(200):
            writer.create_edge(users[i % 100], posts[i], "CREATED")
        for _ in range(500):
            writer.create_edge(users[rng.randint(0, 99)], posts[rng.randint(0, 199)], "LIKED")
        writer.commit()

        with db.begin() as tx:
            assert list(tx.execute("MATCH (n:User) RETURN count(n)"))[0]["count(n)"] == 100
            assert list(tx.execute("MATCH (n:Post) RETURN count(n)"))[0]["count(n)"] == 200
            assert list(tx.execute("MATCH ()-[e:FOLLOWS]->() RETURN count(e)"))[0]["count(e)"] == 300
            assert list(tx.execute("MATCH ()-[e:CREATED]->() RETURN count(e)"))[0]["count(e)"] == 200
            assert list(tx.execute("MATCH ()-[e:LIKED]->() RETURN count(e)"))[0]["count(e)"] == 500

            result = tx.execute(
                "MATCH (u:User)-[:FOLLOWS]->(f:User)-[:CREATED]->(p:Post) "
                "RETURN count(DISTINCT u)"
            )
            followers = list(result)[0]["count(DISTINCT u)"]
            assert followers > 0


# ---------------------------------------------------------------------------
# Flush
# ---------------------------------------------------------------------------


class TestFlush:
    def test_flush_persists(self, db_path):
        db = traverse.open(db_path)
        with db.begin() as tx:
            tx.execute("CREATE (n:Persist {val: 42})")
        db.flush()
        db.close()

        db2 = traverse.open(db_path)
        with db2.begin() as tx:
            result = tx.execute("MATCH (n:Persist) RETURN n.val")
            assert list(result)[0]["n.val"] == 42
        db2.close()


# ---------------------------------------------------------------------------
# Handle lifetimes, read transactions, busy timeout, threads
# ---------------------------------------------------------------------------


def count(tx, label):
    return list(tx.execute(f"MATCH (n:{label}) RETURN count(n) AS c"))[0]["c"]


class TestLifetimes:
    def test_close_rolls_back_open_transactions(self, db_path):
        db = traverse.open(db_path)
        tx = db.begin()
        tx.execute("CREATE (:Lost)")
        reader = db.begin_read()
        db.close()
        assert db.closed

        with pytest.raises(RuntimeError, match="closed"):
            tx.execute("RETURN 1")
        with pytest.raises(RuntimeError, match="closed"):
            tx.commit()
        reader.abort()  # nothing left to undo: fine

        with traverse.open(db_path) as db2, db2.begin_read() as check:
            assert count(check, "Lost") == 0

    def test_transaction_outlives_its_database_object(self, db_path):
        db = traverse.open(db_path)
        tx = db.begin()
        del db
        gc.collect()
        tx.execute("CREATE (:Kept)")
        tx.commit()
        del tx
        gc.collect()
        # the last handle saved and released the file
        with traverse.open(db_path) as db2, db2.begin_read() as check:
            assert count(check, "Kept") == 1

    def test_dropped_transaction_rolls_back(self, db):
        db.busy_timeout = 0.05
        tx = db.begin()
        tx.execute("CREATE (:Dropped)")
        del tx
        gc.collect()
        with db.begin() as tx2:
            assert count(tx2, "Dropped") == 0

    def test_read_transaction_sees_its_snapshot(self, db):
        writer = db.begin()
        writer.execute("CREATE (:N)")
        with db.begin_read() as reader:
            assert reader.read_only
            assert count(reader, "N") == 0
            with pytest.raises(traverse.TraverseError):
                reader.execute("CREATE (:M)")
            writer.commit()
            assert count(reader, "N") == 0
        with db.begin_read() as reader:
            assert count(reader, "N") == 1

    def test_busy_timeout(self, db):
        assert db.busy_timeout == 5.0
        db.busy_timeout = 0.02
        assert db.busy_timeout == pytest.approx(0.02)
        writer = db.begin()
        with pytest.raises(traverse.BusyError):
            db.begin()
        with pytest.raises(traverse.BusyError):
            db.bulk_writer()
        with pytest.raises(traverse.BusyError):
            db.flush()
        assert issubclass(traverse.BusyError, traverse.TraverseError)
        writer.abort()
        db.begin().commit()

        db.busy_timeout = math.inf
        assert db.busy_timeout == math.inf
        with pytest.raises(ValueError):
            db.busy_timeout = -1

    def test_waiting_writer_lets_the_holder_thread_finish(self, db):
        # begin() waits without holding the GIL, so the thread holding the writer can commit
        held = db.begin()
        held.execute("CREATE (:First)")

        def finish():
            time.sleep(0.05)
            held.commit()

        t = threading.Thread(target=finish)
        t.start()
        with db.begin() as tx:
            assert count(tx, "First") == 1
        t.join()

    def test_transaction_finishes_on_another_thread(self, db):
        tx = db.begin()
        tx.execute("CREATE (:Moved)")
        t = threading.Thread(target=tx.commit)
        t.start()
        t.join()
        with db.begin_read() as check:
            assert count(check, "Moved") == 1

    def test_threads_share_the_database(self, db):
        db.busy_timeout = math.inf
        errors = []

        def work(i):
            try:
                for _ in range(20):
                    if i % 2:
                        with db.begin() as tx:
                            tx.execute("CREATE (:T)")
                    else:
                        with db.begin_read() as tx:
                            a = count(tx, "T")
                            assert count(tx, "T") == a
            except Exception as e:  # noqa: BLE001
                errors.append(e)

        threads = [threading.Thread(target=work, args=(i,)) for i in range(6)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        assert not errors
        with db.begin_read() as check:
            assert count(check, "T") == 60


# ---------------------------------------------------------------------------
# Stopping statements
# ---------------------------------------------------------------------------

# a three-way cross product over 2,000 nodes: minutes of work unless stopped
LONG = "MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)"


def seed_big(db):
    with db.begin() as tx:
        tx.execute("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})")


class TestCancellation:
    def test_cancel_from_another_thread_stops_the_query(self, db):
        seed_big(db)
        with db.begin_read() as tx:
            timer = threading.Timer(0.2, tx.cancel)
            timer.start()
            start = time.monotonic()
            with pytest.raises(traverse.QueryInterruptedError):
                tx.execute(LONG)
            assert time.monotonic() - start < 20
            timer.join()
            # the transaction carries on: a read changed nothing
            assert list(tx.execute("RETURN 1 AS n"))[0]["n"] == 1
            # nothing runs: a cancel does nothing
            tx.cancel()
        assert issubclass(traverse.QueryInterruptedError, traverse.TraverseError)

    def test_statement_timeout_stops_the_query(self, db):
        seed_big(db)
        assert db.statement_timeout is None
        db.statement_timeout = 0.2
        assert db.statement_timeout == pytest.approx(0.2)
        with db.begin_read() as tx:
            with pytest.raises(traverse.QueryInterruptedError, match="timed out"):
                tx.execute(LONG)
        with pytest.raises(ValueError):
            db.statement_timeout = -1
        db.statement_timeout = None
        assert db.statement_timeout is None

    def test_terminate_transactions_ends_another_transaction(self, tmp_path):
        # a name of its own: SHOW TRANSACTIONS lists a database by its file name
        name = f"terminate_{os.getpid()}_{time.monotonic_ns()}"
        with traverse.open(str(tmp_path / f"{name}.tvdb")) as db:
            victim = db.begin()
            victim.execute("CREATE (:Pending)")
            with db.begin_read() as admin:
                rows = list(
                    admin.execute(
                        "SHOW TRANSACTIONS YIELD transactionId AS id, database, protocol, "
                        "currentQuery WHERE database = $name AND protocol = 'embedded' "
                        "AND currentQuery = '' "
                        "TERMINATE TRANSACTIONS id YIELD message RETURN message",
                        {"name": name},
                    )
                )
            assert rows == [{"message": "Transaction terminated."}]
            with pytest.raises(traverse.QueryInterruptedError):
                victim.execute("RETURN 1")
            with pytest.raises(traverse.QueryInterruptedError):
                victim.commit()
            with db.begin_read() as check:
                assert count(check, "Pending") == 0

    def test_a_statement_that_fails_after_writing_fails_the_transaction(self, db):
        with db.begin() as tx:
            # fails before writing: the transaction carries on
            with pytest.raises(traverse.TraverseError):
                tx.execute("CREATE (:Kept")
            tx.execute("CREATE (:Kept)")
            # writes, then fails
            with pytest.raises(traverse.TraverseError):
                tx.execute("CREATE (:Partial) WITH 1 AS one RETURN 10 / 0")
            with pytest.raises(traverse.TraverseError, match="roll it back"):
                tx.execute("RETURN 1")
            with pytest.raises(traverse.TraverseError, match="roll it back"):
                tx.commit()
        with db.begin_read() as check:
            assert count(check, "Kept") == 0


# ---------------------------------------------------------------------------
# Auto-commit
# ---------------------------------------------------------------------------


def auto_count(db, label):
    return list(db.execute(f"MATCH (n:{label}) RETURN count(n) AS c"))[0]["c"]


class TestAutoCommit:
    def test_writes_commit_and_reads_see_them(self, db):
        db.execute("CREATE (:A {v: $v}), (:A {v: 2})", {"v": 1})
        assert auto_count(db, "A") == 2
        db.execute("INSERT (:G {v: 1})", dialect="gql")
        assert list(db.execute("MATCH (n:G) RETURN n.v AS v", dialect="gql"))[0]["v"] == 1

    def test_errors_carry_their_gql_status(self, db):
        with pytest.raises(traverse.TraverseError) as e:
            db.execute("RETURN 1 / 0")
        assert e.value.gql_status == "22012"
        with pytest.raises(traverse.TraverseError) as e:
            db.execute("RETURN x")
        assert e.value.gql_status == "42N62"
        with pytest.raises(traverse.BusyError) as e:
            writer = db.begin()
            db.busy_timeout = 0.01
            try:
                db.execute("CREATE ()")
            finally:
                writer.abort()
        assert e.value.gql_status == "25N06"

    def test_a_failed_write_leaves_nothing_behind(self, db):
        with pytest.raises(traverse.TraverseError):
            db.execute("CREATE (:B) WITH 1 AS one RETURN 10 / 0")
        assert auto_count(db, "B") == 0

    def test_call_in_transactions_commits_each_batch(self, db):
        db.execute(
            "UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:C {v: 10 / i}) } "
            "IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE"
        )
        assert auto_count(db, "C") == 2

    def test_a_procedure_that_writes_runs_as_a_write(self, db):
        db.execute("CALL apoc.create.node(['P'], {v: 1}) YIELD node RETURN node")
        assert auto_count(db, "P") == 1

    def test_reads_never_wait_for_the_writer_and_writes_do(self, db):
        db.busy_timeout = 0.05
        writer = db.begin()
        assert list(db.execute("RETURN 1 AS n"))[0]["n"] == 1
        with pytest.raises(traverse.BusyError):
            db.execute("CREATE (:D)")
        writer.abort()

    def test_a_statement_timeout_stops_an_auto_commit_query(self, db):
        seed_big(db)
        db.statement_timeout = 0.2
        with pytest.raises(traverse.QueryInterruptedError):
            db.execute(LONG)
