using Traverse.Bolt.Tests.Helpers;
using Xunit;

namespace Traverse.Bolt.Tests;

public sealed class TransactionTests : IClassFixture<BoltFixture>
{
    private readonly BoltFixture _fixture;

    public TransactionTests(BoltFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task Transaction_CommitCreatesData()
    {
        var client = _fixture.RequirePlain();

        // Clean up first
        await client.RunAndConsumeAsync("MATCH (n:_TxTest) DELETE n", ct: TestContext.Current.CancellationToken);

        await using (var tx = await client.BeginTransactionAsync(ct: TestContext.Current.CancellationToken))
        {
            await tx.RunAndConsumeAsync("CREATE (:_TxTest {val: 1})", ct: TestContext.Current.CancellationToken);
            await tx.CommitAsync(TestContext.Current.CancellationToken);
        }

        var result = await client.RunAsync("MATCH (n:_TxTest) RETURN n.val AS val", ct: TestContext.Current.CancellationToken);
        Assert.Single(result.Records);
        Assert.Equal(1L, result.Records[0]["val"]);

        // Clean up
        await client.RunAndConsumeAsync("MATCH (n:_TxTest) DELETE n", ct: TestContext.Current.CancellationToken);
    }

    [Fact]
    public async Task Transaction_RollbackDiscardsData()
    {
        var client = _fixture.RequirePlain();

        // Clean up first
        await client.RunAndConsumeAsync("MATCH (n:_TxRollback) DELETE n", ct: TestContext.Current.CancellationToken);

        await using (var tx = await client.BeginTransactionAsync(ct: TestContext.Current.CancellationToken))
        {
            await tx.RunAndConsumeAsync("CREATE (:_TxRollback {val: 1})", ct: TestContext.Current.CancellationToken);
            await tx.RollbackAsync(TestContext.Current.CancellationToken);
        }

        var result = await client.RunAsync("MATCH (n:_TxRollback) RETURN count(n) AS c", ct: TestContext.Current.CancellationToken);
        Assert.Single(result.Records);
        Assert.Equal(0L, result.Records[0]["c"]);
    }

    [Fact]
    public async Task Transaction_DisposeAutoRollback()
    {
        var client = _fixture.RequirePlain();

        await client.RunAndConsumeAsync("MATCH (n:_TxDispose) DELETE n", ct: TestContext.Current.CancellationToken);

        // Begin TX, create data, then dispose without committing
        await using (var tx = await client.BeginTransactionAsync(ct: TestContext.Current.CancellationToken))
        {
            await tx.RunAndConsumeAsync("CREATE (:_TxDispose {val: 1})", ct: TestContext.Current.CancellationToken);
            // No commit - dispose should auto-rollback
        }

        var result = await client.RunAsync("MATCH (n:_TxDispose) RETURN count(n) AS c", ct: TestContext.Current.CancellationToken);
        Assert.Single(result.Records);
        Assert.Equal(0L, result.Records[0]["c"]);
    }

    [Fact]
    public async Task Transaction_MultipleQueries()
    {
        var client = _fixture.RequirePlain();

        await client.RunAndConsumeAsync("MATCH (n:_TxMulti) DELETE n", ct: TestContext.Current.CancellationToken);

        await using (var tx = await client.BeginTransactionAsync(ct: TestContext.Current.CancellationToken))
        {
            await tx.RunAndConsumeAsync("CREATE (:_TxMulti {val: 1})", ct: TestContext.Current.CancellationToken);
            await tx.RunAndConsumeAsync("CREATE (:_TxMulti {val: 2})", ct: TestContext.Current.CancellationToken);

            var result = await tx.RunAsync("MATCH (n:_TxMulti) RETURN n.val AS val ORDER BY val", ct: TestContext.Current.CancellationToken);
            Assert.Equal(2, result.Records.Count);
            Assert.Equal(1L, result.Records[0]["val"]);
            Assert.Equal(2L, result.Records[1]["val"]);

            await tx.CommitAsync(TestContext.Current.CancellationToken);
        }

        // Clean up
        await client.RunAndConsumeAsync("MATCH (n:_TxMulti) DELETE n", ct: TestContext.Current.CancellationToken);
    }

    [Fact]
    public async Task Transaction_CommittedTx_ThrowsOnReuse()
    {
        var client = _fixture.RequirePlain();

        var tx = await client.BeginTransactionAsync(ct: TestContext.Current.CancellationToken);
        await tx.CommitAsync(TestContext.Current.CancellationToken);

        await Assert.ThrowsAsync<InvalidOperationException>(async () =>
        {
            await tx.RunAsync("RETURN 1", ct: TestContext.Current.CancellationToken);
        });

        await tx.DisposeAsync();
    }

    // -- TLS transaction ---------------------------------------------

    [Fact]
    public async Task Tls_Transaction_CommitAndQuery()
    {
        var client = _fixture.RequireTls();

        await client.RunAndConsumeAsync("MATCH (n:_TxTls) DELETE n", ct: TestContext.Current.CancellationToken);

        await using (var tx = await client.BeginTransactionAsync(ct: TestContext.Current.CancellationToken))
        {
            await tx.RunAndConsumeAsync("CREATE (:_TxTls {val: 42})", ct: TestContext.Current.CancellationToken);
            await tx.CommitAsync(TestContext.Current.CancellationToken);
        }

        var result = await client.RunAsync("MATCH (n:_TxTls) RETURN n.val AS val", ct: TestContext.Current.CancellationToken);
        Assert.Single(result.Records);
        Assert.Equal(42L, result.Records[0]["val"]);

        await client.RunAndConsumeAsync("MATCH (n:_TxTls) DELETE n", ct: TestContext.Current.CancellationToken);
    }
}
