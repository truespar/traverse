using Traverse.Grpc.Tests.Helpers;
using Traverse.Grpc.Values;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class BatchTests : IClassFixture<ServerFixture>
{
    private readonly ServerFixture _fixture;

    public BatchTests(ServerFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task BatchCreateNodes_CreatesMultiple()
    {
        var client = _fixture.RequireClient();

        var ids = await client.BatchCreateNodesAsync([
            (["_TestBatch"], new Dictionary<string, TvValue> { ["name"] = "A" }),
            (["_TestBatch"], new Dictionary<string, TvValue> { ["name"] = "B" }),
            (["_TestBatch"], new Dictionary<string, TvValue> { ["name"] = "C" }),
        ], cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal(3, ids.Count);

        // Cleanup
        foreach (var id in ids)
            await client.DeleteNodeAsync(id, detach: true, cancellationToken: TestContext.Current.CancellationToken);
    }

    [Fact]
    public async Task BatchCreateEdges_CreatesMultiple()
    {
        var client = _fixture.RequireClient();

        var nodeIds = await client.BatchCreateNodesAsync([
            (["_TestBatchEdge"], null),
            (["_TestBatchEdge"], null),
            (["_TestBatchEdge"], null),
        ], cancellationToken: TestContext.Current.CancellationToken);

        var edgeIds = await client.BatchCreateEdgesAsync([
            (nodeIds[0], nodeIds[1], "LINKED", null),
            (nodeIds[1], nodeIds[2], "LINKED", null),
        ], cancellationToken: TestContext.Current.CancellationToken);

        Assert.Equal(2, edgeIds.Count);

        // Cleanup
        foreach (var id in nodeIds)
            await client.DeleteNodeAsync(id, detach: true, cancellationToken: TestContext.Current.CancellationToken);
    }
}
