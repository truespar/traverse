using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class UserTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public UserTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task CreateListDelete_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var username = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateUserAsync(username, "testpass123", "reader", TestContext.Current.CancellationToken);
        try
        {
            var users = await client.ListUsersAsync(TestContext.Current.CancellationToken);
            Assert.Contains(users, u => u.Username == username);
        }
        finally
        {
            await client.DeleteUserAsync(username, TestContext.Current.CancellationToken);
        }

        var afterDelete = await client.ListUsersAsync(TestContext.Current.CancellationToken);
        Assert.DoesNotContain(afterDelete, u => u.Username == username);
    }

    [Fact]
    public async Task UpdateUser_Changes_Role()
    {
        var client = _fixture.RequireServer();
        var username = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        await client.CreateUserAsync(username, "testpass123", "reader", TestContext.Current.CancellationToken);
        try
        {
            await client.UpdateUserAsync(username, role: "admin", ct: TestContext.Current.CancellationToken);

            var users = await client.ListUsersAsync(TestContext.Current.CancellationToken);
            var user = Assert.Single(users, u => u.Username == username);
            Assert.Equal("admin", user.Role);
        }
        finally
        {
            await client.DeleteUserAsync(username, TestContext.Current.CancellationToken);
        }
    }
}
