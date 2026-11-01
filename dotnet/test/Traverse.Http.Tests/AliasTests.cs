using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class AliasTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public AliasTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task CreateAndDrop_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var alias = $"_httptest_alias_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateAliasAsync(alias, _fixture.TestDatabase, TestContext.Current.CancellationToken);
        try
        {
            var aliases = await client.ListAliasesAsync(TestContext.Current.CancellationToken);
            Assert.Contains(aliases, a => a.Alias == alias && a.Database == _fixture.TestDatabase);
        }
        finally
        {
            await client.DropAliasAsync(alias, TestContext.Current.CancellationToken);
        }

        var afterDrop = await client.ListAliasesAsync(TestContext.Current.CancellationToken);
        Assert.DoesNotContain(afterDrop, a => a.Alias == alias);
    }

    [Fact]
    public async Task Alter_Changes_Target()
    {
        var client = _fixture.RequireServer();
        var alias = $"_httptest_alias_{Guid.NewGuid().ToString("N")[..8]}";
        var db2 = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateDatabaseAsync(db2, TestContext.Current.CancellationToken);
        try
        {
            await client.CreateAliasAsync(alias, _fixture.TestDatabase, TestContext.Current.CancellationToken);
            await client.AlterAliasAsync(alias, db2, TestContext.Current.CancellationToken);

            var aliases = await client.ListAliasesAsync(TestContext.Current.CancellationToken);
            var found = Assert.Single(aliases, a => a.Alias == alias);
            Assert.Equal(db2, found.Database);

            await client.DropAliasAsync(alias, TestContext.Current.CancellationToken);
        }
        finally
        {
            try { await client.DropDatabaseAsync(db2, ct: TestContext.Current.CancellationToken); } catch { }
        }
    }
}
