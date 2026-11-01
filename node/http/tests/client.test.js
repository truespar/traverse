import { describe, it, before, after } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';

import { TraverseHttpClient, TraverseHttpError, ColumnMapping, EdgeEndpoint } from '../src/index.js';

const BASE_URL = process.env.TRAVERSE_BASE_URL || 'http://localhost:7691';

/** Generate a short unique id. */
function uid() {
  return crypto.randomBytes(4).toString('hex');
}

let client;
let testDb;

before(async () => {
  // Probe server availability
  let healthResp;
  try {
    const probe = new TraverseHttpClient(BASE_URL, { timeout: 3000 });
    healthResp = await probe.health();
    if (healthResp.status !== 'ok') {
      console.log('SKIP: Traverse server not healthy');
      process.exit(0);
    }
  } catch {
    console.log(`SKIP: Traverse server not available at ${BASE_URL}`);
    process.exit(0);
  }

  const apiKey = healthResp.authRequired ? process.env.TRAVERSE_API_KEY : null;
  client = new TraverseHttpClient(BASE_URL, {
    apiKey,
    timeout: 30_000,
    transferTimeout: 60_000,
  });

  // Create isolated test database
  testDb = `_httptest_${uid()}`;
  await client.createDatabase(testDb);
});

after(async () => {
  if (client && testDb) {
    try {
      await client.dropDatabase(testDb);
    } catch {
      // ignore
    }
  }
});

// -- Health ----------------------------------------------------------

describe('TestHealth', () => {
  it('healthReturnsStatusAndVersion', async () => {
    const h = await client.health();
    assert.equal(h.status, 'ok');
    assert.ok(h.version);
  });

  it('healthReportsFeatures', async () => {
    const h = await client.health();
    // The features map is present on all current servers; every value
    // is a boolean (gql, grpc, tls, mcp, ml).
    assert.equal(typeof h.features, 'object');
    assert.ok('gql' in h.features);
    for (const v of Object.values(h.features)) {
      assert.equal(typeof v, 'boolean');
    }
  });

  it('readyReturnsTrue', async () => {
    const r = await client.ready();
    assert.equal(r, true);
  });
});

// -- Query -----------------------------------------------------------

describe('TestQuery', () => {
  it('returnScalar', async () => {
    const result = await client.query('RETURN 42 AS n', null, testDb);
    assert.equal(result.rows.length, 1);
    assert.equal(result.rows[0][0], 42);
  });

  it('returnString', async () => {
    const result = await client.query("RETURN 'hello' AS s", null, testDb);
    assert.equal(result.rows[0][0], 'hello');
  });

  it('withParameters', async () => {
    const result = await client.query('RETURN $x AS val', { x: 99 }, testDb);
    assert.equal(result.rows[0][0], 99);
  });

  it('gqlDialect', async (t) => {
    const h = await client.health();
    if (!h.features?.gql) {
      t.skip('server built without the gql feature');
      return;
    }
    // `||` concat and `IS INTEGER` are GQL-only syntax - they parse
    // only when the dialect reaches the server.
    const result = await client.query(
      "RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i",
      null,
      testDb,
      'gql',
    );
    assert.deepEqual(result.rows[0], ['foobar', true]);
  });

  it('writeQueryStats', async () => {
    const result = await client.query('CREATE (n:_HttpTest) RETURN n', null, testDb);
    try {
      assert.equal(result.stats.nodesCreated, 1);
    } finally {
      await client.query('MATCH (n:_HttpTest) DELETE n', null, testDb);
    }
  });

  it('syntaxErrorThrows', async () => {
    await assert.rejects(
      () => client.query('THIS IS NOT CYPHER', null, testDb),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.equal(err.statusCode, 400);
        return true;
      }
    );
  });
});

// -- Databases -------------------------------------------------------

describe('TestDatabases', () => {
  it('listDatabasesContainsTestDb', async () => {
    const dbs = await client.listDatabases();
    assert.ok(dbs.some((d) => d.name === testDb));
  });

  it('createAndDropRoundtrip', async () => {
    const name = `_httptest_${uid()}`;
    await client.createDatabase(name);
    let dbs = await client.listDatabases();
    assert.ok(dbs.some((d) => d.name === name));
    await client.dropDatabase(name);
    dbs = await client.listDatabases();
    assert.ok(!dbs.some((d) => d.name === name));
  });

  it('renameRoundtrip', async () => {
    const original = `_httptest_${uid()}`;
    const renamed = `_httptest_${uid()}`;
    await client.createDatabase(original);
    try {
      await client.renameDatabase(original, renamed);
      let dbs = await client.listDatabases();
      assert.ok(!dbs.some((d) => d.name === original));
      assert.ok(dbs.some((d) => d.name === renamed));
    } finally {
      for (const n of [renamed, original]) {
        try {
          await client.dropDatabase(n);
        } catch {
          // ignore
        }
      }
    }
  });

  it('unloadAndLoadRoundtrip', async () => {
    const name = `_httptest_${uid()}`;
    await client.createDatabase(name);
    try {
      await client.unloadDatabase(name);
      let dbs = await client.listDatabases();
      assert.ok(!dbs.some((d) => d.name === name));
      const files = await client.listFiles();
      const f = files.find((fi) => fi.name === name);
      assert.ok(f);
      await client.loadDatabase(name, f.file);
      dbs = await client.listDatabases();
      assert.ok(dbs.some((d) => d.name === name));
    } finally {
      try {
        await client.dropDatabase(name);
      } catch {
        // ignore
      }
    }
  });
});

// -- Schema ----------------------------------------------------------

describe('TestSchema', () => {
  it('getSchemaReturnsLabels', async () => {
    await client.query(
      "CREATE (a:_SchemaTestNode {name: 'a'})-[:_SCHEMA_TEST_REL]->(b:_SchemaTestNode {name: 'b'})",
      null,
      testDb
    );
    try {
      const schema = await client.getSchema(testDb);
      assert.ok(schema.labels.includes('_SchemaTestNode'));
      assert.ok(schema.relationshipTypes.includes('_SCHEMA_TEST_REL'));
    } finally {
      await client.query('MATCH (n:_SchemaTestNode) DETACH DELETE n', null, testDb);
    }
  });
});

// -- Aliases ---------------------------------------------------------

describe('TestAliases', () => {
  it('createAndDropRoundtrip', async () => {
    const alias = `_httptest_alias_${uid()}`;
    await client.createAlias(alias, testDb);
    try {
      const aliases = await client.listAliases();
      assert.ok(aliases.some((a) => a.alias === alias && a.database === testDb));
    } finally {
      await client.dropAlias(alias);
    }
    const aliases = await client.listAliases();
    assert.ok(!aliases.some((a) => a.alias === alias));
  });

  it('alterChangesTarget', async () => {
    const alias = `_httptest_alias_${uid()}`;
    const db2 = `_httptest_${uid()}`;
    await client.createDatabase(db2);
    try {
      await client.createAlias(alias, testDb);
      await client.alterAlias(alias, db2);
      const aliases = await client.listAliases();
      assert.ok(aliases.some((a) => a.alias === alias && a.database === db2));
      await client.dropAlias(alias);
    } finally {
      try {
        await client.dropDatabase(db2);
      } catch {
        // ignore
      }
    }
  });
});

// -- Import ----------------------------------------------------------

describe('TestImport', () => {
  it('importCypherCreatesNodes', async () => {
    const cypher =
      'CREATE (:_ImportTest {id: 1});\n' +
      'CREATE (:_ImportTest {id: 2});\n' +
      'CREATE (:_ImportTest {id: 3});';
    const result = await client.importCypher(cypher, testDb);
    try {
      assert.equal(result.executed, 3);
      assert.equal(result.errors.length, 0);
    } finally {
      await client.query('MATCH (n:_ImportTest) DELETE n', null, testDb);
    }
  });

  it('importCsvCreatesNodes', async () => {
    const csv = 'name,age\nAlice,30\nBob,25';
    const columns = [
      new ColumnMapping('name', 'name', 'string'),
      new ColumnMapping('age', 'age', 'integer'),
    ];
    const result = await client.importCsv(csv, '_CsvTestNode', columns, { database: testDb });
    try {
      assert.equal(result.nodesCreated, 2);
      assert.equal(result.errors.length, 0);
    } finally {
      await client.query('MATCH (n:_CsvTestNode) DELETE n', null, testDb);
    }
  });

  it('importCsvEdgesCreatesEdges', async () => {
    await client.query(
      "CREATE (:_CsvEdgeSrc {key: 'a'}), (:_CsvEdgeDst {key: 'b'})",
      null,
      testDb
    );
    try {
      const csv = 'src,dst\na,b';
      const source = new EdgeEndpoint('_CsvEdgeSrc', 'src', 'key');
      const target = new EdgeEndpoint('_CsvEdgeDst', 'dst', 'key');
      const result = await client.importCsvEdges(csv, '_CSV_TEST_REL', source, target, {
        database: testDb,
      });
      assert.equal(result.edgesCreated, 1);
      assert.equal(result.errors.length, 0);
    } finally {
      await client.query('MATCH (n:_CsvEdgeSrc) DETACH DELETE n', null, testDb);
      await client.query('MATCH (n:_CsvEdgeDst) DETACH DELETE n', null, testDb);
    }
  });
});

// -- Users -----------------------------------------------------------

describe('TestUsers', () => {
  it('createListDeleteRoundtrip', async () => {
    const username = `_httptest_${uid()}`;
    await client.createUser(username, 'testpass123', 'reader');
    try {
      const users = await client.listUsers();
      assert.ok(users.some((u) => u.username === username));
    } finally {
      await client.deleteUser(username);
    }
    const users = await client.listUsers();
    assert.ok(!users.some((u) => u.username === username));
  });

  it('updateUserChangesRole', async () => {
    const username = `_httptest_${uid()}`;
    await client.createUser(username, 'testpass123', 'reader');
    try {
      await client.updateUser(username, { role: 'admin' });
      const users = await client.listUsers();
      const user = users.find((u) => u.username === username);
      assert.ok(user);
      assert.equal(user.role, 'admin');
    } finally {
      await client.deleteUser(username);
    }
  });
});

// -- Save ------------------------------------------------------------

describe('TestSave', () => {
  it('saveSucceeds', async () => {
    await client.save();
  });
});

// -- Files -----------------------------------------------------------

describe('TestFiles', () => {
  it('listFilesReturnsList', async () => {
    const files = await client.listFiles();
    assert.ok(Array.isArray(files));
  });

  it('downloadFileStreamsBytes', async () => {
    const data = await client.downloadFile(testDb);
    assert.ok(data.length > 0);
  });

  it('uploadAndDeleteFileRoundtrip', async () => {
    const name = `_httptest_${uid()}`;
    const tempDb = `_httptest_src_${uid()}`;
    await client.createDatabase(tempDb);
    let tempDbActive = true;
    try {
      const tvdbBytes = await client.downloadFile(tempDb);
      await client.dropDatabase(tempDb);
      tempDbActive = false;

      const upload = await client.uploadFile(name, tvdbBytes);
      assert.equal(upload.name, name);
      try {
        const files = await client.listFiles();
        assert.ok(files.some((f) => f.name === name && !f.loaded));
      } finally {
        await client.deleteFile(name);
      }
      const files = await client.listFiles();
      assert.ok(!files.some((f) => f.name === name));
    } finally {
      if (tempDbActive) {
        try {
          await client.dropDatabase(tempDb);
        } catch {
          // ignore
        }
      }
    }
  });
});

// -- Tasks -----------------------------------------------------------

describe('TestTasks', () => {
  it('asyncSaveSucceeds', async () => {
    await client.save();
  });

  it('getTaskNotFoundThrows', async () => {
    await assert.rejects(
      () => client.getTask('tsk_0000000000000000'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.equal(err.statusCode, 404);
        return true;
      }
    );
  });

  it('asyncUnloadAndReloadRoundtrip', async () => {
    const name = `_httptest_${uid()}`;
    await client.createDatabase(name);
    try {
      await client.query('CREATE (:_TaskMarker {v: 42})', null, name);
      await client.unloadDatabase(name);
      let dbs = await client.listDatabases();
      assert.ok(!dbs.some((d) => d.name === name));
      const files = await client.listFiles();
      const f = files.find((fi) => fi.name === name);
      assert.ok(f);
      await client.loadDatabase(name, f.file);
      const result = await client.query('MATCH (n:_TaskMarker) RETURN n.v AS v', null, name);
      assert.equal(result.rows.length, 1);
      assert.equal(result.rows[0][0], 42);
    } finally {
      try {
        await client.dropDatabase(name);
      } catch {
        // ignore
      }
    }
  });
});

// -- Errors ----------------------------------------------------------

describe('TestErrors', () => {
  it('invalidBaseUrlThrows', async () => {
    const bad = new TraverseHttpClient('http://localhost:19999', { timeout: 2000 });
    await assert.rejects(() => bad.health());
  });

  it('createDuplicateDatabaseThrows', async () => {
    await assert.rejects(
      () => client.createDatabase(testDb),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.match(err.message, /already exists/);
        return true;
      }
    );
  });

  it('dropNonexistentDatabaseThrows', async () => {
    await assert.rejects(
      () => client.dropDatabase('_httptest_does_not_exist_ever'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        return true;
      }
    );
  });

  it('renameToExistingNameThrows', async () => {
    const temp = `_httptest_${uid()}`;
    await client.createDatabase(temp);
    try {
      await assert.rejects(
        () => client.renameDatabase(temp, testDb),
        (err) => {
          assert.ok(err instanceof TraverseHttpError);
          assert.match(err.message, /already exists/);
          return true;
        }
      );
    } finally {
      try {
        await client.dropDatabase(temp);
      } catch {
        // ignore
      }
    }
  });

  it('renameNonexistentDatabaseThrows', async () => {
    await assert.rejects(
      () => client.renameDatabase('_httptest_does_not_exist_ever', 'whatever'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.match(err.message, /does not exist/);
        return true;
      }
    );
  });

  it('createDuplicateAliasThrows', async () => {
    const alias = `_httptest_alias_${uid()}`;
    await client.createAlias(alias, testDb);
    try {
      await assert.rejects(
        () => client.createAlias(alias, testDb),
        (err) => {
          assert.ok(err instanceof TraverseHttpError);
          assert.match(err.message, /already exists/);
          return true;
        }
      );
    } finally {
      await client.dropAlias(alias);
    }
  });

  it('createAliasToNonexistentDbThrows', async () => {
    await assert.rejects(
      () => client.createAlias('_httptest_alias_bad', '_httptest_does_not_exist_ever'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.match(err.message, /does not exist/);
        return true;
      }
    );
  });

  it('dropNonexistentAliasThrows', async () => {
    await assert.rejects(
      () => client.dropAlias('_httptest_alias_does_not_exist'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        return true;
      }
    );
  });

  it('deleteNonexistentUserThrows', async () => {
    await assert.rejects(
      () => client.deleteUser('_httptest_user_does_not_exist'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.match(err.message, /does not exist/);
        return true;
      }
    );
  });

  it('queryNonexistentDatabaseThrows', async () => {
    await assert.rejects(
      () => client.query('RETURN 1', null, '_httptest_does_not_exist_ever'),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        return true;
      }
    );
  });

  it('deleteLoadedFileThrows', async () => {
    await assert.rejects(
      () => client.deleteFile(testDb),
      (err) => {
        assert.ok(err instanceof TraverseHttpError);
        assert.match(err.message, /loaded/);
        return true;
      }
    );
  });
});
