using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class TaskTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public TaskTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task AsyncLoadDatabase_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var name = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        using var factory = new TvdbFactory();
        var bytes = TvdbFactory.ReadBytes(
            factory.Create("task_test", "CREATE (:_TaskMarker {v: 1})"));

        try
        {
            // Upload the file
            var upload = await client.UploadFileAsync(name, new MemoryStream(bytes), ct: TestContext.Current.CancellationToken);

            // Load via async task mode (transparent to caller)
            await client.LoadDatabaseAsync(name, upload.File, ct: TestContext.Current.CancellationToken);

            // Verify it's queryable
            var result = await client.QueryAsync("MATCH (n:_TaskMarker) RETURN n.v AS v", database: name, ct: TestContext.Current.CancellationToken);
            Assert.Single(result.Rows);
            Assert.Equal(1, result.Rows[0][0].GetInt32());
        }
        finally
        {
            try { await client.DropDatabaseAsync(name, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }

    [Fact]
    public async Task AsyncUnloadAndReload_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var name = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateDatabaseAsync(name, TestContext.Current.CancellationToken);
        try
        {
            // Insert data
            await client.QueryAsync("CREATE (:_TaskMarker {v: 42})", database: name, ct: TestContext.Current.CancellationToken);

            // Unload (async mode)
            await client.UnloadDatabaseAsync(name, ct: TestContext.Current.CancellationToken);

            // Verify it's gone from loaded databases
            var dbs = await client.ListDatabasesAsync(TestContext.Current.CancellationToken);
            Assert.DoesNotContain(dbs, db => db.Name == name);

            // The file should still exist on disk
            var files = await client.ListFilesAsync(TestContext.Current.CancellationToken);
            Assert.Contains(files, f => f.Name == name);

            // Reload (async mode)
            var file = files.First(f => f.Name == name).File;
            await client.LoadDatabaseAsync(name, file, ct: TestContext.Current.CancellationToken);

            // Verify data survived the round-trip
            var result = await client.QueryAsync("MATCH (n:_TaskMarker) RETURN n.v AS v", database: name, ct: TestContext.Current.CancellationToken);
            Assert.Single(result.Rows);
            Assert.Equal(42, result.Rows[0][0].GetInt32());
        }
        finally
        {
            try { await client.DropDatabaseAsync(name, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }

    [Fact]
    public async Task AsyncSave_Succeeds()
    {
        var client = _fixture.RequireServer();
        // Save uses async task mode transparently
        await client.SaveAsync(ct: TestContext.Current.CancellationToken);
    }

    [Fact]
    public async Task GetTask_NotFound_Throws()
    {
        var client = _fixture.RequireServer();
        var ex = await Assert.ThrowsAsync<TraverseHttpException>(
            () => client.GetTaskAsync("tsk_0000000000000000", TestContext.Current.CancellationToken));
        Assert.Equal(System.Net.HttpStatusCode.NotFound, ex.StatusCode);
    }
}
