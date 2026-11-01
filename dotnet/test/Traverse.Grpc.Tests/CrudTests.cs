using Traverse.Grpc.Tests.Helpers;
using Traverse.Grpc.Values;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class CrudTests : IClassFixture<ServerFixture>
{
    private readonly ServerFixture _fixture;

    public CrudTests(ServerFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task CreateNode_GetNode_DeleteNode_Roundtrip()
    {
        var client = _fixture.RequireClient();

        var node = await client.CreateNodeAsync(["_TestCrud"], new Dictionary<string, TvValue> { ["name"] = "Alice" }, cancellationToken: TestContext.Current.CancellationToken);

        Assert.Contains("_TestCrud", node.Labels);
        Assert.Equal("Alice", node.Properties["name"].AsString());

        var fetched = await client.GetNodeAsync(node.Id, cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(node.Id, fetched.Id);
        Assert.Equal("Alice", fetched.Properties["name"].AsString());

        await client.DeleteNodeAsync(node.Id, detach: true, cancellationToken: TestContext.Current.CancellationToken);

        await Assert.ThrowsAsync<TraverseException>(async () =>
            await client.GetNodeAsync(node.Id, cancellationToken: TestContext.Current.CancellationToken));
    }

    [Fact]
    public async Task CreateEdge_GetEdge_DeleteEdge_Roundtrip()
    {
        var client = _fixture.RequireClient();

        var n1 = await client.CreateNodeAsync(["_TestCrudEdge"], cancellationToken: TestContext.Current.CancellationToken);
        var n2 = await client.CreateNodeAsync(["_TestCrudEdge"], cancellationToken: TestContext.Current.CancellationToken);

        var edge = await client.CreateEdgeAsync(n1.Id, n2.Id, "KNOWS", new Dictionary<string, TvValue> { ["since"] = 2024L }, cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal("KNOWS", edge.Type);
        Assert.Equal(n1.Id, edge.Source);
        Assert.Equal(n2.Id, edge.Target);

        var fetched = await client.GetEdgeAsync(edge.Id, cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(edge.Id, fetched.Id);

        await client.DeleteEdgeAsync(edge.Id, cancellationToken: TestContext.Current.CancellationToken);

        // Cleanup nodes
        await client.DeleteNodeAsync(n1.Id, detach: true, cancellationToken: TestContext.Current.CancellationToken);
        await client.DeleteNodeAsync(n2.Id, detach: true, cancellationToken: TestContext.Current.CancellationToken);
    }

    [Fact]
    public async Task UpdateNodeProperties_UpdatesValues()
    {
        var client = _fixture.RequireClient();

        var node = await client.CreateNodeAsync(["_TestUpdate"], new Dictionary<string, TvValue> { ["v"] = 1L }, cancellationToken: TestContext.Current.CancellationToken);

        await client.UpdateNodePropertiesAsync(node.Id, new Dictionary<string, TvValue> { ["v"] = 2L }, cancellationToken: TestContext.Current.CancellationToken);

        var updated = await client.GetNodeAsync(node.Id, cancellationToken: TestContext.Current.CancellationToken);
        Assert.Equal(2L, updated.Properties["v"].AsInt64());

        await client.DeleteNodeAsync(node.Id, detach: true, cancellationToken: TestContext.Current.CancellationToken);
    }
}
