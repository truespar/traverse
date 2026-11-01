using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class DatabaseTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public DatabaseTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task ListDatabases_Returns_NonEmpty()
    {
        var client = _fixture.RequireServer();
        var dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);

        Assert.NotEmpty(dbs);
        Assert.Contains(dbs, db => db.Name == _fixture.TestDatabase);
    }

    [Fact]
    public async Task CreateAndDrop_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var name = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateDatabaseAsync(name, TestContext.Current.CancellationToken);
        var dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);
        Assert.Contains(dbs, db => db.Name == name);

        await client.DropDatabaseAsync(name, ct: TestContext.Current.CancellationToken);
        dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);
        Assert.DoesNotContain(dbs, db => db.Name == name);
    }

    [Fact]
    public async Task Rename_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var original = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";
        var renamed = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateDatabaseAsync(original, TestContext.Current.CancellationToken);
        try
        {
            await client.RenameDatabaseAsync(original, renamed, TestContext.Current.CancellationToken);

            var dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);
            Assert.DoesNotContain(dbs, db => db.Name == original);
            Assert.Contains(dbs, db => db.Name == renamed);
        }
        finally
        {
            // Clean up whichever name exists
            try { await client.DropDatabaseAsync(renamed, ct: TestContext.Current.CancellationToken); } catch { }
            try { await client.DropDatabaseAsync(original, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }

    [Fact]
    public async Task UnloadAndLoad_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var name = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateDatabaseAsync(name, TestContext.Current.CancellationToken);
        try
        {
            // Unload - should disappear from loaded databases
            await client.UnloadDatabaseAsync(name, ct: TestContext.Current.CancellationToken);
            var dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);
            Assert.DoesNotContain(dbs, db => db.Name == name);

            // The .tvdb file should still exist on disk
            var files = await client.ListFilesAsync(TestContext.Current.CancellationToken);
            Assert.Contains(files, f => f.Name == name);

            // Load it back
            var file = files.First(f => f.Name == name).File;
            await client.LoadDatabaseAsync(name, file, ct: TestContext.Current.CancellationToken);
            dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);
            Assert.Contains(dbs, db => db.Name == name);
        }
        finally
        {
            try { await client.DropDatabaseAsync(name, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }

    [Fact]
    public async Task Hotswap_Via_Alias()
    {
        var client = _fixture.RequireServer();
        var v1 = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";
        var v2 = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";
        var alias = $"_httptest_alias_{Guid.NewGuid().ToString("N")[..8]}";

        // Build two .tvdb files with different pre-baked data using the embedded engine
        using var factory = new TvdbFactory();
        var v1Bytes = TvdbFactory.ReadBytes(
            factory.Create("v1", "CREATE (:_HotswapMarker {version: 1})"));
        var v2Bytes = TvdbFactory.ReadBytes(
            factory.Create("v2", "CREATE (:_HotswapMarker {version: 2})"));

        try
        {
            // Upload and activate v1
            var upload1 = await client.UploadFileAsync(v1, new MemoryStream(v1Bytes), ct: TestContext.Current.CancellationToken);
            await client.LoadDatabaseAsync(v1, upload1.File, ct: TestContext.Current.CancellationToken);

            // Create alias pointing to v1
            await client.CreateAliasAsync(alias, v1, TestContext.Current.CancellationToken);

            // Query through the alias - should see v1's data
            var r1 = await client.QueryAsync("MATCH (n:_HotswapMarker) RETURN n.version AS v", database: alias, ct: TestContext.Current.CancellationToken);
            Assert.Single(r1.Rows);
            Assert.Equal(1, r1.Rows[0][0].GetInt32());

            // Upload and activate v2
            var upload2 = await client.UploadFileAsync(v2, new MemoryStream(v2Bytes), ct: TestContext.Current.CancellationToken);
            await client.LoadDatabaseAsync(v2, upload2.File, ct: TestContext.Current.CancellationToken);

            // Hotswap: point alias to v2
            await client.AlterAliasAsync(alias, v2, TestContext.Current.CancellationToken);

            // Query through the alias - should now see v2's data
            var r2 = await client.QueryAsync("MATCH (n:_HotswapMarker) RETURN n.version AS v", database: alias, ct: TestContext.Current.CancellationToken);
            Assert.Single(r2.Rows);
            Assert.Equal(2, r2.Rows[0][0].GetInt32());

            // Drop v1 - alias must still resolve to v2
            await client.DropDatabaseAsync(v1, ct: TestContext.Current.CancellationToken);
            v1 = null; // prevent double-drop in finally

            // Verify alias still returns v2 data after v1 is gone
            var r3 = await client.QueryAsync("MATCH (n:_HotswapMarker) RETURN n.version AS v", database: alias, ct: TestContext.Current.CancellationToken);
            Assert.Single(r3.Rows);
            Assert.Equal(2, r3.Rows[0][0].GetInt32());
        }
        finally
        {
            try { await client.DropAliasAsync(alias, TestContext.Current.CancellationToken); } catch { }
            if (v1 is not null) try { await client.DropDatabaseAsync(v1, ct: TestContext.Current.CancellationToken); } catch { }
            try { await client.DropDatabaseAsync(v2, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }

    [Fact]
    public async Task Hotswap_Zero_Downtime()
    {
        var client = _fixture.RequireServer();
        var v1 = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";
        var v2 = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";
        var alias = $"_httptest_alias_{Guid.NewGuid().ToString("N")[..8]}";

        using var factory = new TvdbFactory();
        var v1Bytes = TvdbFactory.ReadBytes(
            factory.Create("v1", "CREATE (:_HotswapMarker {version: 1})"));
        var v2Bytes = TvdbFactory.ReadBytes(
            factory.Create("v2", "CREATE (:_HotswapMarker {version: 2})"));

        try
        {
            // Stand up v1 and create alias
            var upload1 = await client.UploadFileAsync(v1, new MemoryStream(v1Bytes), ct: TestContext.Current.CancellationToken);
            await client.LoadDatabaseAsync(v1, upload1.File, ct: TestContext.Current.CancellationToken);
            await client.CreateAliasAsync(alias, v1, TestContext.Current.CancellationToken);

            // Stand up v2 (not yet pointed to by alias)
            var upload2 = await client.UploadFileAsync(v2, new MemoryStream(v2Bytes), ct: TestContext.Current.CancellationToken);
            await client.LoadDatabaseAsync(v2, upload2.File, ct: TestContext.Current.CancellationToken);

            // Fire N queries concurrently, performing the hotswap partway through.
            // Each query records its observed version or error.
            const int totalQueries = 40;
            const int swapAfter = 20;
            var cypher = "MATCH (n:_HotswapMarker) RETURN n.version AS v";
            var results = new (int Index, int? Version, string? Error)[totalQueries];

            // Use a separate client for the query barrage
            using var queryClient = new TraverseHttpClient(HttpFixture.BaseUrl, new TraverseHttpOptions
            {
                ApiKey = _fixture.ApiKey,
                Timeout = TimeSpan.FromSeconds(5),
            });

            // Launch first batch (should all see version 1)
            var preTasks = new Task[swapAfter];
            for (int i = 0; i < swapAfter; i++)
            {
                int idx = i;
                preTasks[i] = Task.Run(async () =>
                {
                    try
                    {
                        var r = await queryClient.QueryAsync(cypher, database: alias);
                        results[idx] = (idx, r.Rows[0][0].GetInt32(), null);
                    }
                    catch (Exception ex)
                    {
                        results[idx] = (idx, null, ex.Message);
                    }
                }, TestContext.Current.CancellationToken);
            }
            await Task.WhenAll(preTasks);

            // -- Hotswap --
            await client.AlterAliasAsync(alias, v2, TestContext.Current.CancellationToken);

            // Launch second batch (should all see version 2)
            var postTasks = new Task[totalQueries - swapAfter];
            for (int i = swapAfter; i < totalQueries; i++)
            {
                int idx = i;
                postTasks[i - swapAfter] = Task.Run(async () =>
                {
                    try
                    {
                        var r = await queryClient.QueryAsync(cypher, database: alias);
                        results[idx] = (idx, r.Rows[0][0].GetInt32(), null);
                    }
                    catch (Exception ex)
                    {
                        results[idx] = (idx, null, ex.Message);
                    }
                }, TestContext.Current.CancellationToken);
            }
            await Task.WhenAll(postTasks);

            // -- Assertions --

            // Zero errors - every query must have succeeded
            var errors = results.Where(r => r.Error is not null).ToList();
            Assert.True(errors.Count == 0,
                $"Expected zero query errors during hotswap, got {errors.Count}: " +
                string.Join("; ", errors.Select(e => e.Error)));

            // Every observed version must be 1 or 2 (no garbage)
            Assert.All(results, r =>
                Assert.True(r.Version == 1 || r.Version == 2,
                    $"Query {r.Index}: unexpected version {r.Version}"));

            // Pre-swap queries must all see version 1
            Assert.All(results.Take(swapAfter), r =>
                Assert.Equal(1, r.Version));

            // Post-swap queries must all see version 2
            Assert.All(results.Skip(swapAfter), r =>
                Assert.Equal(2, r.Version));
        }
        finally
        {
            try { await client.DropAliasAsync(alias, TestContext.Current.CancellationToken); } catch { }
            try { await client.DropDatabaseAsync(v1, ct: TestContext.Current.CancellationToken); } catch { }
            try { await client.DropDatabaseAsync(v2, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }
}
