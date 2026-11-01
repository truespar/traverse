using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class ErrorTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public ErrorTests(HttpFixture fixture) => _fixture = fixture;

    // -- Connectivity ------------------------------------------------

    [Fact]
    public async Task Invalid_BaseUrl_Throws()
    {
        using var bad = new TraverseHttpClient("http://localhost:19999", new TraverseHttpOptions
        {
            Timeout = TimeSpan.FromSeconds(2),
        });

        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            await bad.HealthAsync(TestContext.Current.CancellationToken);
        });
    }

    // -- Databases ---------------------------------------------------

    [Fact]
    public async Task Create_Duplicate_Database_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.CreateDatabaseAsync(_fixture.TestDatabase, TestContext.Current.CancellationToken);
        });
        Assert.Contains("already exists", ex.Message);
    }

    [Fact]
    public async Task Drop_Nonexistent_Database_Throws()
    {
        var client = _fixture.RequireServer();

        await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.DropDatabaseAsync("_httptest_does_not_exist_ever", ct: TestContext.Current.CancellationToken);
        });
    }

    [Fact]
    public async Task Rename_To_Existing_Name_Throws()
    {
        var client = _fixture.RequireServer();
        var temp = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateDatabaseAsync(temp, TestContext.Current.CancellationToken);
        try
        {
            var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
            {
                await client.RenameDatabaseAsync(temp, _fixture.TestDatabase, TestContext.Current.CancellationToken);
            });
            Assert.Contains("already exists", ex.Message);
        }
        finally
        {
            try { await client.DropDatabaseAsync(temp, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }

    [Fact]
    public async Task Rename_Nonexistent_Database_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.RenameDatabaseAsync("_httptest_does_not_exist_ever", "whatever", TestContext.Current.CancellationToken);
        });
        Assert.Contains("does not exist", ex.Message);
    }

    // -- Aliases -----------------------------------------------------

    [Fact]
    public async Task Create_Duplicate_Alias_Throws()
    {
        var client = _fixture.RequireServer();
        var alias = $"_httptest_alias_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateAliasAsync(alias, _fixture.TestDatabase, TestContext.Current.CancellationToken);
        try
        {
            var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
            {
                await client.CreateAliasAsync(alias, _fixture.TestDatabase, TestContext.Current.CancellationToken);
            });
            Assert.Contains("already exists", ex.Message);
        }
        finally
        {
            await client.DropAliasAsync(alias, TestContext.Current.CancellationToken);
        }
    }

    [Fact]
    public async Task Create_Alias_To_Nonexistent_Database_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.CreateAliasAsync("_httptest_alias_bad", "_httptest_does_not_exist_ever", TestContext.Current.CancellationToken);
        });
        Assert.Contains("does not exist", ex.Message);
    }

    [Fact]
    public async Task Alter_Nonexistent_Alias_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.AlterAliasAsync("_httptest_alias_does_not_exist", _fixture.TestDatabase, TestContext.Current.CancellationToken);
        });
        Assert.Contains("does not exist", ex.Message);
    }

    [Fact]
    public async Task Drop_Nonexistent_Alias_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.DropAliasAsync("_httptest_alias_does_not_exist", TestContext.Current.CancellationToken);
        });
        Assert.Contains("does not exist", ex.Message);
    }

    // -- Users -------------------------------------------------------

    [Fact]
    public async Task Delete_Nonexistent_User_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.DeleteUserAsync("_httptest_user_does_not_exist", TestContext.Current.CancellationToken);
        });
        Assert.Contains("does not exist", ex.Message);
    }

    // -- Files -------------------------------------------------------

    [Fact]
    public async Task Delete_Loaded_File_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.DeleteFileAsync(_fixture.TestDatabase, ct: TestContext.Current.CancellationToken);
        });
        Assert.Contains("loaded", ex.Message);
    }

    [Fact]
    public async Task Upload_Corrupt_File_Throws()
    {
        var client = _fixture.RequireServer();
        var name = $"_httptest_corrupt_{Guid.NewGuid().ToString("N")[..8]}";

        // Random garbage - not valid TVDB magic bytes
        var garbage = new byte[1024];
        new Random(42).NextBytes(garbage);

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.UploadFileAsync(name, new MemoryStream(garbage), ct: TestContext.Current.CancellationToken);
        });
        Assert.True(
            ex.Message.Contains("TVDB", StringComparison.OrdinalIgnoreCase) ||
            ex.Message.Contains("magic", StringComparison.OrdinalIgnoreCase) ||
            ex.Message.Contains("invalid", StringComparison.OrdinalIgnoreCase),
            $"Expected error about invalid TVDB/magic bytes, got: {ex.Message}");

        // Best-effort cleanup in case it somehow succeeded
        try { await client.DeleteFileAsync(name, ct: TestContext.Current.CancellationToken); } catch { }
    }

    // -- Query -------------------------------------------------------

    [Fact]
    public async Task Query_Nonexistent_Database_Throws()
    {
        var client = _fixture.RequireServer();

        await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.QueryAsync("RETURN 1", database: "_httptest_does_not_exist_ever", ct: TestContext.Current.CancellationToken);
        });
    }
}
