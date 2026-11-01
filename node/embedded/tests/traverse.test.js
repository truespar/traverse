// Tests for the embedded Traverse binding.
//
// Usage: node tests/traverse.test.js
//
// Loads the package (../index.js): the addon `npm run build:debug` puts next to it, or the file
// NAPI_RS_NATIVE_LIBRARY_PATH names (a .node file).

const assert = require('assert');
const path = require('path');
const fs = require('fs');
const os = require('os');

const traverse = require('..');

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

let testCount = 0;
let passCount = 0;
let failCount = 0;
const failures = [];

// every temp directory, removed when the run ends
const tmpDirs = [];

function tmpDir() {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'traverse-node-test-'));
    tmpDirs.push(dir);
    return dir;
}

function tmpDbPath() {
    return path.join(tmpDir(), 'test.tvdb');
}

process.on('exit', () => {
    for (const dir of tmpDirs) {
        try {
            fs.rmSync(dir, { recursive: true, force: true });
        } catch {
            // a database a failed test left open
        }
    }
});

function test(name, fn) {
    testCount++;
    try {
        fn();
        passCount++;
        console.log(`  \u2713 ${name}`);
    } catch (e) {
        failCount++;
        failures.push({ name, error: e });
        console.log(`  \u2717 ${name}`);
        console.log(`    ${e.message}`);
    }
}

function section(name) {
    console.log(`\n${name}`);
}

// async tests run after the sync ones, one at a time, before the summary
const asyncTests = [];

function asyncTest(name, fn) {
    asyncTests.push({ name, fn });
}

async function runAsyncTests() {
    if (asyncTests.length) section('Async');
    for (const { name, fn } of asyncTests) {
        testCount++;
        try {
            await fn();
            passCount++;
            console.log(`  \u2713 ${name}`);
        } catch (e) {
            failCount++;
            failures.push({ name, error: e });
            console.log(`  \u2717 ${name}`);
            console.log(`    ${e.message}`);
        }
    }
}

function count(tx, label) {
    return tx.execute(`MATCH (n:${label}) RETURN count(n) AS c`).rows[0].c;
}

function countNodes(db, label) {
    const tx = db.beginRead();
    try {
        return count(tx, label);
    } finally {
        tx.commit();
    }
}

// ---------------------------------------------------------------------------
// Open / Database
// ---------------------------------------------------------------------------

section('Open');

test('open and flush creates file', () => {
    const dbPath = tmpDbPath();
    const db = traverse.open(dbPath);
    const tx = db.begin();
    tx.execute('CREATE (n:Test)');
    tx.commit();
    db.flush();
    db.close();
    assert.ok(fs.existsSync(dbPath));
});

test('reopen existing database', () => {
    const dbPath = tmpDbPath();
    let db = traverse.open(dbPath);
    db.flush();
    db.close();
    db = traverse.open(dbPath);
    db.flush();
    db.close();
});

test('open empty path throws', () => {
    assert.throws(() => traverse.open(''));
});

test('double close', () => {
    const db = traverse.open(tmpDbPath());
    db.close();
    db.close(); // should not throw
});

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

section('Transaction');

test('create and query', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:Person {name: 'Alice', age: 30})");
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:Person) RETURN n.name, n.age');
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['n.name'], 'Alice');
    assert.strictEqual(result.rows[0]['n.age'], 30);
    tx.abort();
    db.close();
});

test('explicit commit', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    tx.execute('CREATE (n:Foo {val: 42})');
    tx.commit();

    const tx2 = db.begin();
    const result = tx2.execute('MATCH (n:Foo) RETURN n.val');
    assert.strictEqual(result.rowCount, 1);
    tx2.abort();
    db.close();
});

test('explicit abort', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    tx.execute('CREATE (n:Aborted {val: 99})');
    tx.abort();

    const tx2 = db.begin();
    const result = tx2.execute('MATCH (n:Aborted) RETURN n.val');
    assert.strictEqual(result.rowCount, 0);
    tx2.abort();
    db.close();
});

test('commit after commit throws', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    tx.commit();
    assert.throws(() => tx.commit());
    db.close();
});

test('execute after commit throws', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    tx.commit();
    assert.throws(() => tx.execute('RETURN 1'));
    db.close();
});

test('syntax error throws', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    assert.throws(() => tx.execute('INVALID SYNTAX HERE'));
    tx.abort();
    db.close();
});

test('multiple queries in single transaction', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    tx.execute("CREATE (a:Person {name: 'Alice'})");
    tx.execute("CREATE (b:Person {name: 'Bob'})");
    tx.execute("MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)");
    const result = tx.execute('MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name');
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['a.name'], 'Alice');
    assert.strictEqual(result.rows[0]['b.name'], 'Bob');
    tx.commit();
    db.close();
});

test('unknown function throws', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    assert.throws(() => tx.execute('RETURN no_such_function(1)'));
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// QueryResult
// ---------------------------------------------------------------------------

section('QueryResult');

test('columns', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute("RETURN 1 AS x, 'hello' AS y");
    assert.deepStrictEqual(result.columns, ['x', 'y']);
    tx.abort();
    db.close();
});

test('row count', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute('CREATE (a:N), (b:N), (c:N)');
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:N) RETURN n');
    assert.strictEqual(result.rowCount, 3);
    tx.abort();
    db.close();
});

test('iteration', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('UNWIND [1, 2, 3] AS x RETURN x');
    const values = result.rows.map(r => r.x);
    assert.deepStrictEqual(values, [1, 2, 3]);
    tx.abort();
    db.close();
});

test('stats', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('CREATE (n:S {val: 1})');
    assert.strictEqual(result.stats.nodesCreated, 1);
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// DataTypes
// ---------------------------------------------------------------------------

section('DataTypes');

test('null value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN null AS x');
    assert.strictEqual(result.rows[0].x, null);
    tx.abort();
    db.close();
});

test('bool value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN true AS x, false AS y');
    assert.strictEqual(result.rows[0].x, true);
    assert.strictEqual(result.rows[0].y, false);
    tx.abort();
    db.close();
});

test('NaN and infinite floats', () => {
    const db = traverse.open(tmpDbPath());
    const row = db.execute('RETURN 0.0 / 0.0 AS nan, 1.0 / 0.0 AS inf, -1.0 / 0.0 AS ninf').rows[0];
    assert.ok(Number.isNaN(row.nan), `nan: ${row.nan}`);
    assert.strictEqual(row.inf, Infinity);
    assert.strictEqual(row.ninf, -Infinity);
    db.close();
});

test('integer value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN 42 AS x');
    assert.strictEqual(result.rows[0].x, 42);
    tx.abort();
    db.close();
});

test('float value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN 3.14 AS x');
    assert.ok(Math.abs(result.rows[0].x - 3.14) < 0.001);
    tx.abort();
    db.close();
});

test('string value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute("RETURN 'hello' AS x");
    assert.strictEqual(result.rows[0].x, 'hello');
    tx.abort();
    db.close();
});

test('list value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN [1, 2, 3] AS x');
    assert.deepStrictEqual(result.rows[0].x, [1, 2, 3]);
    tx.abort();
    db.close();
});

test('map value', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute("RETURN {a: 1, b: 'two'} AS x");
    const map = result.rows[0].x;
    assert.strictEqual(map.a, 1);
    assert.strictEqual(map.b, 'two');
    tx.abort();
    db.close();
});

test('node value', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:Person {name: 'Bob'})");
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:Person) RETURN n');
    const node = result.rows[0].n;
    assert.ok(node._id !== undefined);
    assert.ok(node._labels.includes('Person'));
    assert.strictEqual(node.name, 'Bob');
    tx.abort();
    db.close();
});

test('edge value', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute('CREATE (a:A)-[:KNOWS {since: 2020}]->(b:B)');
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH ()-[r:KNOWS]->() RETURN r');
    const edge = result.rows[0].r;
    assert.strictEqual(edge._type, 'KNOWS');
    assert.strictEqual(edge.since, 2020);
    tx.abort();
    db.close();
});

test('path value', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute('CREATE (a:X {id: 1})-[:R]->(b:X {id: 2})');
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH p = (a:X {id: 1})-[:R]->(b:X {id: 2}) RETURN p');
    const p = result.rows[0].p;
    assert.strictEqual(p.nodes.length, 2);
    assert.strictEqual(p.edges.length, 1);
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// Property Edge Cases
// ---------------------------------------------------------------------------

section('PropertyEdgeCases');

test('unicode string roundtrips', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:I18n {name: '\u65E5\u672C\u8A9E\u30C6\u30B9\u30C8', emoji: '\uD83D\uDE80\uD83D\uDD25', arabic: '\u0645\u0631\u062D\u0628\u0627'})");
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:I18n) RETURN n.name, n.emoji, n.arabic');
    const row = result.rows[0];
    assert.strictEqual(row['n.name'], '\u65E5\u672C\u8A9E\u30C6\u30B9\u30C8');
    assert.strictEqual(row['n.emoji'], '\uD83D\uDE80\uD83D\uDD25');
    assert.strictEqual(row['n.arabic'], '\u0645\u0631\u062D\u0628\u0627');
    tx.abort();
    db.close();
});

test('empty string property', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:E {val: ''})");
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:E) RETURN n.val');
    assert.strictEqual(result.rows[0]['n.val'], '');
    tx.abort();
    db.close();
});

test('large string property', () => {
    const big = 'x'.repeat(100000);
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute('CREATE (n:Big {val: $s})', { s: big });
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:Big) RETURN n.val');
    assert.strictEqual(result.rows[0]['n.val'], big);
    tx.abort();
    db.close();
});

test('negative integer', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN -9999999 AS val');
    assert.strictEqual(result.rows[0].val, -9999999);
    tx.abort();
    db.close();
});

test('negative float', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN -273.15 AS val');
    assert.ok(Math.abs(result.rows[0].val - (-273.15)) < 0.001);
    tx.abort();
    db.close();
});

test('zero values', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN 0 AS i, 0.0 AS f, false AS b');
    const row = result.rows[0];
    assert.strictEqual(row.i, 0);
    assert.strictEqual(row.f, 0);
    assert.strictEqual(row.b, false);
    tx.abort();
    db.close();
});

test('multiple null columns', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN null AS a, null AS b, null AS c');
    const row = result.rows[0];
    assert.strictEqual(row.a, null);
    assert.strictEqual(row.b, null);
    assert.strictEqual(row.c, null);
    tx.abort();
    db.close();
});

test('mixed null and values', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute("RETURN 42 AS a, null AS b, 'hello' AS c");
    const row = result.rows[0];
    assert.strictEqual(row.a, 42);
    assert.strictEqual(row.b, null);
    assert.strictEqual(row.c, 'hello');
    tx.abort();
    db.close();
});

test('special characters in string', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:Special {val: 'line1\\nline2\\ttab'})");
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:Special) RETURN n.val');
    assert.ok(result.rows[0]['n.val'].includes('line1'));
    tx.abort();
    db.close();
});

test('bulk writer unicode properties', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    writer.createNode(['I18n'], { name: '\u00D1o\u00F1o', city: 'M\u00FCnchen' });
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH (n:I18n) RETURN n.name, n.city');
    assert.strictEqual(result.rows[0]['n.name'], '\u00D1o\u00F1o');
    assert.strictEqual(result.rows[0]['n.city'], 'M\u00FCnchen');
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// QueryParameters
// ---------------------------------------------------------------------------

section('QueryParameters');

test('string param', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:P {name: 'Alice'})");
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:P) WHERE n.name = $name RETURN n.name', { name: 'Alice' });
    assert.strictEqual(result.rows[0]['n.name'], 'Alice');
    tx.abort();
    db.close();
});

test('int param', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN $x + 1 AS y', { x: 41 });
    assert.strictEqual(result.rows[0].y, 42);
    tx.abort();
    db.close();
});

test('null param', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN $x IS NULL AS y', { x: null });
    assert.strictEqual(result.rows[0].y, true);
    tx.abort();
    db.close();
});

test('list param', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('UNWIND $items AS x RETURN x', { items: [10, 20, 30] });
    const values = result.rows.map(r => r.x);
    assert.deepStrictEqual(values, [10, 20, 30]);
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// BulkWriter
// ---------------------------------------------------------------------------

section('BulkWriter');

test('create nodes', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const nid1 = writer.createNode(['Person'], { name: 'Alice', age: 30 });
    const nid2 = writer.createNode(['Person'], { name: 'Bob', age: 25 });
    assert.notStrictEqual(nid1, nid2);
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH (n:Person) RETURN n.name ORDER BY n.name');
    assert.strictEqual(result.rows[0]['n.name'], 'Alice');
    assert.strictEqual(result.rows[1]['n.name'], 'Bob');
    tx.abort();
    db.close();
});

test('create node multiple labels', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    writer.createNode(['Person', 'Employee'], { name: 'Bob' });
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH (n:Person:Employee) RETURN n.name');
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['n.name'], 'Bob');
    tx.abort();
    db.close();
});

test('create edges', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const a = writer.createNode(['A']);
    const b = writer.createNode(['B']);
    const eid = writer.createEdge(a, b, 'KNOWS', { since: 2020 });
    assert.ok(eid >= 0);
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH ()-[r:KNOWS]->() RETURN r.since');
    assert.strictEqual(result.rows[0]['r.since'], 2020);
    tx.abort();
    db.close();
});

test('create edge no properties', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const a = writer.createNode(['X']);
    const b = writer.createNode(['X']);
    writer.createEdge(a, b, 'LINKS');
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH ()-[e:LINKS]->() RETURN count(e)');
    assert.strictEqual(result.rows[0]['count(e)'], 1);
    tx.abort();
    db.close();
});

test('create index', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    writer.createNode(['Person'], { name: 'Test' });
    writer.createIndex('idx_person_name', 'Person', 'name');
    writer.commit();
    db.close();
});

test('create edge index', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const a = writer.createNode(['X']);
    const b = writer.createNode(['Y']);
    writer.createEdge(a, b, 'REL', { w: 1 });
    writer.createEdgeIndex('idx_rel_w', 'REL', 'w');
    writer.commit();
    db.close();
});

test('drop edge index', () => {
    const db = traverse.open(tmpDbPath());
    let writer = db.bulkWriter();
    const a = writer.createNode(['X']);
    const b = writer.createNode(['Y']);
    writer.createEdge(a, b, 'REL', { w: 1 });
    writer.createEdgeIndex('idx_rel_w', 'REL', 'w');
    writer.commit();

    writer = db.bulkWriter();
    writer.dropEdgeIndex('idx_rel_w');
    writer.commit();
    db.close();
});

test('abort', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    writer.createNode(['Nope']);
    writer.abort();
    assert.strictEqual(countNodes(db, 'Nope'), 0);
    db.close();
});

test('load is invisible until commit', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    for (let i = 0; i < 100; i++) {
        writer.createNode(['Item'], { i });
    }
    assert.strictEqual(countNodes(db, 'Item'), 0);
    writer.commit();
    assert.strictEqual(countNodes(db, 'Item'), 100);
    db.close();
});

test('node without properties', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const nid = writer.createNode(['Empty']);
    assert.ok(nid >= 0);
    writer.commit();
    db.close();
});

test('abort releases write lock', () => {
    const db = traverse.open(tmpDbPath());
    let writer = db.bulkWriter();
    writer.createNode(['First']);
    writer.abort();

    writer = db.bulkWriter();
    writer.createNode(['Second']);
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH (n:Second) RETURN count(n)');
    assert.strictEqual(result.rows[0]['count(n)'], 1);
    tx.abort();
    db.close();
});

test('commit after commit throws', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    writer.createNode(['X']);
    writer.commit();
    assert.throws(() => writer.commit());
    db.close();
});

test('create node after commit throws', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    writer.commit();
    assert.throws(() => writer.createNode(['X']));
    db.close();
});

test('create edge invalid source throws', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const a = writer.createNode(['X']);
    assert.throws(() => writer.createEdge(a, 999999, 'BAD'));
    writer.abort();
    db.close();
});

test('bulk insert many nodes', () => {
    const count = 1000;
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    for (let i = 0; i < count; i++) {
        writer.createNode(['Batch'], { idx: i });
    }
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH (n:Batch) RETURN count(n)');
    assert.strictEqual(result.rows[0]['count(n)'], count);
    tx.abort();
    db.close();
});

test('bulk insert nodes and edges graph', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    const ids = [];
    for (let i = 0; i < 5; i++) {
        ids.push(writer.createNode(['Chain'], { pos: i }));
    }
    for (let i = 0; i < 4; i++) {
        writer.createEdge(ids[i], ids[i + 1], 'NEXT');
    }
    writer.commit();

    const tx = db.begin();
    assert.strictEqual(tx.execute('MATCH (n:Chain) RETURN count(n)').rows[0]['count(n)'], 5);
    assert.strictEqual(tx.execute('MATCH ()-[e:NEXT]->() RETURN count(e)').rows[0]['count(e)'], 4);
    const rp = tx.execute(
        'MATCH p = (start:Chain {pos: 0})-[:NEXT*]->(end:Chain) WHERE NOT (end)-[:NEXT]->() RETURN end.pos'
    );
    assert.strictEqual(rp.rows[0]['end.pos'], 4);
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// Index / Constraint
// ---------------------------------------------------------------------------

section('Index');

test('create index and lookup', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    for (let i = 0; i < 100; i++) {
        tx.execute(`CREATE (n:User {id: ${i}, name: 'user${i}'})`);
    }
    tx.commit();

    tx = db.begin();
    tx.execute('CREATE INDEX idx_user_id FOR (n:User) ON (n.id)');
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:User {id: 42}) RETURN n.name');
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['n.name'], 'user42');
    tx.abort();
    db.close();
});

test('drop index', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:Item {code: 'A'})");
    tx.execute('CREATE INDEX idx_item_code FOR (n:Item) ON (n.code)');
    tx.commit();

    tx = db.begin();
    tx.execute('DROP INDEX idx_item_code');
    tx.commit();

    tx = db.begin();
    const result = tx.execute('MATCH (n:Item) RETURN n.code');
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['n.code'], 'A');
    tx.abort();
    db.close();
});

test('unique constraint', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute('CREATE CONSTRAINT uniq_email FOR (n:User) REQUIRE n.email IS UNIQUE');
    tx.execute("CREATE (n:User {email: 'alice@test.com'})");
    tx.commit();

    tx = db.begin();
    assert.throws(() => tx.execute("CREATE (n:User {email: 'alice@test.com'})"));
    tx.abort();
    db.close();
});

test('index survives flush reopen', () => {
    const dbPath = tmpDbPath();
    let db = traverse.open(dbPath);
    let tx = db.begin();
    tx.execute('CREATE INDEX idx_name FOR (n:Person) ON (n.name)');
    for (let i = 0; i < 50; i++) {
        tx.execute(`CREATE (n:Person {name: 'person${i}'})`);
    }
    tx.commit();
    db.flush();
    db.close();

    db = traverse.open(dbPath);
    tx = db.begin();
    const result = tx.execute("MATCH (n:Person {name: 'person25'}) RETURN n.name");
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['n.name'], 'person25');
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// Concurrency / MVCC
// ---------------------------------------------------------------------------

section('Concurrency');

test('multiple sequential transactions', () => {
    const db = traverse.open(tmpDbPath());
    for (let i = 0; i < 10; i++) {
        const tx = db.begin();
        tx.execute(`CREATE (n:Seq {idx: ${i}})`);
        tx.commit();
    }

    const tx = db.begin();
    const result = tx.execute('MATCH (n:Seq) RETURN count(n)');
    assert.strictEqual(result.rows[0]['count(n)'], 10);
    tx.abort();
    db.close();
});

test('transaction after bulk write', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    for (let i = 0; i < 100; i++) {
        writer.createNode(['Bulk'], { idx: i });
    }
    writer.commit();

    let tx = db.begin();
    tx.execute("CREATE (n:Tx {val: 'after_bulk'})");
    tx.commit();

    tx = db.begin();
    assert.strictEqual(tx.execute('MATCH (n:Bulk) RETURN count(n)').rows[0]['count(n)'], 100);
    assert.strictEqual(tx.execute('MATCH (n:Tx) RETURN n.val').rows[0]['n.val'], 'after_bulk');
    tx.abort();
    db.close();
});

test('bulk write after transaction', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("CREATE (n:Pre {val: 'before_bulk'})");
    tx.commit();

    const writer = db.bulkWriter();
    writer.createNode(['Post'], { val: 'after_tx' });
    writer.commit();

    tx = db.begin();
    assert.strictEqual(tx.execute('MATCH (n:Pre) RETURN n.val').rows[0]['n.val'], 'before_bulk');
    assert.strictEqual(tx.execute('MATCH (n:Post) RETURN n.val').rows[0]['n.val'], 'after_tx');
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// Scale
// ---------------------------------------------------------------------------

section('Scale');

test('bulk insert 10k nodes', () => {
    const count = 10000;
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    for (let i = 0; i < count; i++) {
        writer.createNode(['User'], { id: i, name: `user_${i}` });
    }
    writer.commit();

    const tx = db.begin();
    assert.strictEqual(tx.execute('MATCH (n:User) RETURN count(n)').rows[0]['count(n)'], count);
    tx.abort();
    db.close();
});

test('transaction insert 1k with index', () => {
    const count = 1000;
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute('CREATE INDEX idx_item_id FOR (n:Item) ON (n.id)');
    tx.commit();

    tx = db.begin();
    for (let i = 0; i < count; i++) {
        tx.execute(`CREATE (n:Item {id: ${i}, data: 'payload_${i}'})`);
    }
    tx.commit();

    tx = db.begin();
    assert.strictEqual(tx.execute('MATCH (n:Item) RETURN count(n)').rows[0]['count(n)'], count);
    assert.strictEqual(tx.execute('MATCH (n:Item {id: 777}) RETURN n.data').rows[0]['n.data'], 'payload_777');
    tx.abort();
    db.close();
});

test('large result set', () => {
    const count = 5000;
    const db = traverse.open(tmpDbPath());
    const writer = db.bulkWriter();
    for (let i = 0; i < count; i++) {
        writer.createNode(['Row'], { val: i });
    }
    writer.commit();

    const tx = db.begin();
    const result = tx.execute('MATCH (n:Row) RETURN n.val ORDER BY n.val');
    assert.strictEqual(result.rowCount, count);
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// Flush
// ---------------------------------------------------------------------------

section('Flush');

test('flush persists', () => {
    const dbPath = tmpDbPath();
    let db = traverse.open(dbPath);
    let tx = db.begin();
    tx.execute('CREATE (n:Persist {val: 42})');
    tx.commit();
    db.flush();
    db.close();

    db = traverse.open(dbPath);
    tx = db.begin();
    const result = tx.execute('MATCH (n:Persist) RETURN n.val');
    assert.strictEqual(result.rows[0]['n.val'], 42);
    tx.abort();
    db.close();
});

test('flush persists across reopen', () => {
    const dbPath = tmpDbPath();
    let db = traverse.open(dbPath);
    let tx = db.begin();
    tx.execute("CREATE (n:Persist {val: 'survived'})");
    tx.commit();
    db.flush();
    db.close();

    db = traverse.open(dbPath);
    tx = db.begin();
    const result = tx.execute('MATCH (n:Persist) RETURN n.val');
    assert.strictEqual(result.rows[0]['n.val'], 'survived');
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// GQL dialect - ISO/IEC 39075 via execute(query, params, 'gql'). The
// queries use GQL-only syntax (|| concat, IS INTEGER, INSERT/FILTER)
// that the openCypher parser rejects, so passing proves the dialect
// option reached the GQL front-end.
// ---------------------------------------------------------------------------

section('GQL dialect');

test('gql-only syntax executes', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute("RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i", null, 'gql');
    assert.strictEqual(result.rows[0]['s'], 'foobar');
    assert.strictEqual(result.rows[0]['i'], true);
    tx.abort();
    db.close();
});

test('gql insert and filter round trip', () => {
    const db = traverse.open(tmpDbPath());
    let tx = db.begin();
    tx.execute("INSERT (:Person {name: 'Ada', age: 36})", null, 'gql');
    tx.commit();

    tx = db.begin();
    const result = tx.execute(
        'MATCH (p:Person) FILTER p.age > 10 RETURN p.name AS name', null, 'gql');
    assert.strictEqual(result.rows.length, 1);
    assert.strictEqual(result.rows[0]['name'], 'Ada');
    tx.abort();
    db.close();
});

test('gql with parameters', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    const result = tx.execute('RETURN $x + 1 AS n', { x: 41 }, 'gql');
    assert.strictEqual(result.rows[0]['n'], 42);
    tx.abort();
    db.close();
});

test('cypher rejects gql syntax', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    assert.throws(() => tx.execute('RETURN 1 IS INTEGER AS i'));
    tx.abort();
    db.close();
});

test('unknown dialect throws', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    assert.throws(() => tx.execute('RETURN 1', null, 'sql'));
    tx.abort();
    db.close();
});

// ---------------------------------------------------------------------------
// Handle lifetimes, read transactions, busy timeout
// ---------------------------------------------------------------------------

section('Lifetimes');

test('close rolls back open transactions', () => {
    const dbPath = tmpDbPath();
    const db = traverse.open(dbPath);
    const tx = db.begin();
    tx.execute('CREATE (:Lost)');
    const reader = db.beginRead();
    db.close();
    assert.ok(db.closed);
    assert.throws(() => tx.execute('RETURN 1'), /closed/);
    assert.throws(() => tx.commit(), /closed/);
    reader.abort();
    assert.throws(() => db.begin(), /closed/);

    const db2 = traverse.open(dbPath);
    assert.strictEqual(countNodes(db2, 'Lost'), 0);
    db2.close();
});

test('read transaction sees its snapshot', () => {
    const db = traverse.open(tmpDbPath());
    const writer = db.begin();
    writer.execute('CREATE (:N)');
    const reader = db.beginRead();
    assert.ok(reader.readOnly);
    assert.strictEqual(count(reader, 'N'), 0);
    assert.throws(() => reader.execute('CREATE (:M)'));
    writer.commit();
    assert.strictEqual(count(reader, 'N'), 0);
    reader.commit();
    assert.strictEqual(countNodes(db, 'N'), 1);
    db.close();
});

test('busy timeout', () => {
    const db = traverse.open(tmpDbPath());
    assert.strictEqual(db.busyTimeout, 5000);
    db.busyTimeout = 20;
    assert.strictEqual(db.busyTimeout, 20);
    const writer = db.begin();
    // a second begin on this thread would wait forever without the timeout
    assert.throws(() => db.begin(), /busy/);
    assert.throws(() => db.bulkWriter(), /busy/);
    assert.throws(() => db.flush(), /busy/);
    writer.abort();
    db.begin().commit();
    db.busyTimeout = Infinity;
    assert.strictEqual(db.busyTimeout, Infinity);
    assert.throws(() => { db.busyTimeout = -1; });
    db.close();
});

test('transaction outlives its database object', () => {
    let db = traverse.open(tmpDbPath());
    const tx = db.begin();
    db = null;
    if (global.gc) global.gc();
    tx.execute('CREATE (:Kept)');
    assert.strictEqual(count(tx, 'Kept'), 1);
    tx.commit();
});

// ---------------------------------------------------------------------------
// Stopping statements
// ---------------------------------------------------------------------------

section('Stopping statements');

// a three-way cross product over 2,000 nodes: minutes of work unless stopped
const LONG = 'MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)';

function seedBig(db) {
    const tx = db.begin();
    tx.execute('UNWIND range(1, 2000) AS i CREATE (:Big {i: i})');
    tx.commit();
}

const interrupted = (e) => e.code === 'ERR_TRAVERSE_INTERRUPTED';
const aborted = (e) => e.name === 'AbortError';

test('statement timeout stops the query', () => {
    const db = traverse.open(tmpDbPath());
    seedBig(db);
    assert.strictEqual(db.statementTimeout, Infinity);
    db.statementTimeout = 200;
    assert.strictEqual(db.statementTimeout, 200);
    const tx = db.beginRead();
    assert.throws(() => tx.execute(LONG), (e) => interrupted(e) && /timed out/.test(e.message));
    tx.commit();
    assert.throws(() => { db.statementTimeout = -1; });
    db.statementTimeout = 0;
    assert.strictEqual(db.statementTimeout, Infinity);
    db.close();
});

test('a statement that fails after writing fails the transaction', () => {
    const db = traverse.open(tmpDbPath());
    const tx = db.begin();
    // fails before writing: the transaction carries on
    assert.throws(() => tx.execute('CREATE (:Kept'));
    tx.execute('CREATE (:Kept)');
    // writes, then fails
    assert.throws(() => tx.execute('CREATE (:Partial) WITH 1 AS one RETURN 10 / 0'));
    const failed = (e) => e.code === 'ERR_TRAVERSE_TRANSACTION_FAILED';
    assert.throws(() => tx.execute('RETURN 1'), failed);
    assert.throws(() => tx.commit(), failed);
    assert.strictEqual(countNodes(db, 'Kept'), 0);
    db.close();
});

asyncTest('executeAsync leaves the event loop running; cancel() stops it', async () => {
    const db = traverse.open(tmpDbPath());
    seedBig(db);
    const tx = db.beginRead();
    assert.strictEqual((await tx.executeAsync('MATCH (n:Big) RETURN count(n) AS c')).rows[0].c, 2000);
    let ticks = 0;
    const ticker = setInterval(() => ticks++, 10);
    setTimeout(() => tx.cancel(), 200);
    const start = Date.now();
    await assert.rejects(tx.executeAsync(LONG), interrupted);
    clearInterval(ticker);
    assert.ok(Date.now() - start < 20000);
    assert.ok(ticks >= 5, `event loop ticked ${ticks} times`);
    // the transaction carries on: a read changed nothing
    assert.strictEqual((await tx.executeAsync('RETURN 1 AS n')).rows[0].n, 1);
    tx.commit();
    db.close();
});

asyncTest('an AbortSignal stops executeAsync', async () => {
    const db = traverse.open(tmpDbPath());
    seedBig(db);
    const tx = db.beginRead();
    await assert.rejects(tx.executeAsync(LONG, undefined, undefined, AbortSignal.timeout(200)), aborted);
    assert.strictEqual((await tx.executeAsync('RETURN $n AS n', { n: 7 })).rows[0].n, 7);
    // an aborted signal never starts the statement
    const done = new AbortController();
    done.abort();
    await assert.rejects(tx.executeAsync('RETURN 1', undefined, undefined, done.signal), aborted);
    // a signal aborted after its statement finished leaves later ones alone
    const late = new AbortController();
    await tx.executeAsync('RETURN 1', undefined, undefined, late.signal);
    const next = tx.executeAsync('MATCH (n:Big) RETURN count(n) AS c');
    late.abort();
    assert.strictEqual((await next).rows[0].c, 2000);
    tx.commit();
    db.close();
});

asyncTest('SHOW and TERMINATE TRANSACTIONS see an executeAsync statement', async () => {
    // a name of its own: SHOW TRANSACTIONS lists a database by its file name
    const name = `terminate_${process.pid}_${Date.now()}`;
    const dir = tmpDir();
    const db = traverse.open(path.join(dir, `${name}.tvdb`));
    seedBig(db);
    const victim = db.begin();
    victim.execute('CREATE (:Pending)');
    const running = victim.executeAsync(LONG, { tag: 'victim' });
    const admin = db.beginRead();
    let rows = [];
    for (let i = 0; i < 200 && rows.length === 0; i++) {
        await new Promise((resolve) => setTimeout(resolve, 10));
        rows = admin.execute(
            'SHOW TRANSACTIONS YIELD transactionId AS id, database, currentQuery, parameters ' +
            'WHERE database = $name AND currentQuery = $q RETURN id, parameters',
            { name, q: LONG },
        ).rows;
    }
    assert.strictEqual(rows.length, 1);
    assert.deepStrictEqual(rows[0].parameters, { tag: 'victim' });
    const terminated = admin.execute('TERMINATE TRANSACTIONS $id', { id: rows[0].id }).rows;
    assert.strictEqual(terminated[0].message, 'Transaction terminated.');
    admin.commit();
    await assert.rejects(running, interrupted);
    assert.throws(() => victim.commit(), interrupted);
    assert.strictEqual(countNodes(db, 'Pending'), 0);
    db.close();
});

// ---------------------------------------------------------------------------
// Auto-commit
// ---------------------------------------------------------------------------

section('Auto-commit');

const autoCount = (db, label) => db.execute(`MATCH (n:${label}) RETURN count(n) AS c`).rows[0].c;

test('db.execute commits writes and reads see them', () => {
    const db = traverse.open(tmpDbPath());
    db.execute('CREATE (:A {v: $v}), (:A {v: 2})', { v: 1 });
    assert.strictEqual(autoCount(db, 'A'), 2);
    db.execute('INSERT (:G {v: 1})', undefined, 'gql');
    assert.strictEqual(db.execute('MATCH (n:G) RETURN n.v AS v', undefined, 'gql').rows[0].v, 1);
    // a failed write leaves nothing behind
    assert.throws(() => db.execute('CREATE (:B) WITH 1 AS one RETURN 10 / 0'));
    assert.strictEqual(autoCount(db, 'B'), 0);
    // batches commit on their own
    db.execute('UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:C {v: 10 / i}) } IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE');
    assert.strictEqual(autoCount(db, 'C'), 2);
    // a procedure that writes runs as a write
    db.execute("CALL apoc.create.node(['P'], {v: 1}) YIELD node RETURN node");
    assert.strictEqual(autoCount(db, 'P'), 1);
    db.close();
});

test('errors carry their GQLSTATUS', () => {
    const db = traverse.open(tmpDbPath());
    assert.throws(() => db.execute('RETURN 1 / 0'), (e) => e.gqlStatus === '22012');
    assert.throws(() => db.execute('RETURN x'), (e) => e.gqlStatus === '42N62');
    db.close();
});

test('db.execute reads never wait for the writer; writes do', () => {
    const db = traverse.open(tmpDbPath());
    db.busyTimeout = 50;
    const writer = db.begin();
    assert.strictEqual(db.execute('RETURN 1 AS n').rows[0].n, 1);
    assert.throws(() => db.execute('CREATE (:D)'), /busy/);
    writer.abort();
    db.close();
});

asyncTest('db.executeAsync runs off the event loop and an AbortSignal stops it', async () => {
    const db = traverse.open(tmpDbPath());
    seedBig(db);
    assert.strictEqual((await db.executeAsync('MATCH (n:Big) RETURN count(n) AS c')).rows[0].c, 2000);
    await assert.rejects(db.executeAsync(LONG, undefined, undefined, AbortSignal.timeout(200)), aborted);
    // a write waiting for the writer lock stops too
    db.busyTimeout = Infinity;
    const writer = db.begin();
    await assert.rejects(db.executeAsync('CREATE (:W)', undefined, undefined, AbortSignal.timeout(200)), aborted);
    writer.abort();
    await db.executeAsync('CREATE (:W)');
    assert.strictEqual(autoCount(db, 'W'), 1);
    db.close();
});

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

runAsyncTests().then(() => {
    console.log(`\n${'-'.repeat(50)}`);
    console.log(`Total: ${testCount}  Passed: ${passCount}  Failed: ${failCount}`);
    if (failures.length > 0) {
        console.log('\nFailures:');
        for (const f of failures) {
            console.log(`  ${f.name}: ${f.error.message}`);
        }
        process.exit(1);
    }
});
