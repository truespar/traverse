using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class SaveTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public SaveTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task Save_Succeeds()
    {
        var client = _fixture.RequireServer();
        await client.SaveAsync(ct: TestContext.Current.CancellationToken);
    }
}
