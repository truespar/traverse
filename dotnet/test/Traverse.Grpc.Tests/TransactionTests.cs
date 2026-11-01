using Traverse.Grpc.Tests.Helpers;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class TransactionTests : IClassFixture<ServerFixture>
{
    private readonly ServerFixture _fixture;

    public TransactionTests(ServerFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task Transaction_CommitAsync_Succeeds()
    {
        var client = _fixture.RequireClient();

        await using var tx = await client.BeginTransactionAsync(cancellationToken: TestContext.Current.CancellationToken);
        await (await tx.QueryAsync("CREATE (n:_TestTxCommit {v: 1})", cancellationToken: TestContext.Current.CancellationToken)).ConsumeAsync(TestContext.Current.CancellationToken);
        var bookmark = await tx.CommitAsync(TestContext.Current.CancellationToken);

        // Cleanup
        await (await client.QueryAsync("MATCH (n:_TestTxCommit) DELETE n", cancellationToken: TestContext.Current.CancellationToken)).ConsumeAsync(TestContext.Current.CancellationToken);
    }

    [Fact]
    public async Task Transaction_Rollback_DiscardsChanges()
    {
        var client = _fixture.RequireClient();

        await using var tx = await client.BeginTransactionAsync(cancellationToken: TestContext.Current.CancellationToken);
        await (await tx.QueryAsync("CREATE (n:_TestTxRollback {v: 1})", cancellationToken: TestContext.Current.CancellationToken)).ConsumeAsync(TestContext.Current.CancellationToken);
        await tx.RollbackAsync(TestContext.Current.CancellationToken);

        // Verify the node was not persisted
        await using var result = await client.QueryAsync("MATCH (n:_TestTxRollback) RETURN count(n) AS c", cancellationToken: TestContext.Current.CancellationToken);
        var (rows, _) = await result.ToListAsync(TestContext.Current.CancellationToken);
        Assert.Equal(0L, rows[0].GetInt64("c"));
    }

    [Fact]
    public async Task Transaction_AutoRollbackOnDispose()
    {
        var client = _fixture.RequireClient();

        // Use block scope to trigger DisposeAsync without commit
        {
            await using var tx = await client.BeginTransactionAsync(cancellationToken: TestContext.Current.CancellationToken);
            await (await tx.QueryAsync("CREATE (n:_TestTxAutoRollback {v: 1})", cancellationToken: TestContext.Current.CancellationToken)).ConsumeAsync(TestContext.Current.CancellationToken);
            // no commit - dispose triggers rollback
        }

        await using var result = await client.QueryAsync("MATCH (n:_TestTxAutoRollback) RETURN count(n) AS c", cancellationToken: TestContext.Current.CancellationToken);
        var (rows, _) = await result.ToListAsync(TestContext.Current.CancellationToken);
        Assert.Equal(0L, rows[0].GetInt64("c"));
    }
}
