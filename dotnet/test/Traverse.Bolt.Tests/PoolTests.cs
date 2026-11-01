using Traverse.Bolt.Tests.Helpers;
using Xunit;

namespace Traverse.Bolt.Tests;

public sealed class PoolTests
{
    [Fact]
    public async Task Pool_AcquireAndRun()
    {
        await using var pool = BoltPool.Create(BoltFixture.PlainUri,
            poolOptions: new BoltPoolOptions { MaxSize = 4 });

        BoltResult result;
        try
        {
            await using var client = await pool.AcquireAsync(TestContext.Current.CancellationToken);
            result = await client.RunAsync("RETURN 1 AS x", ct: TestContext.Current.CancellationToken);
        }
        catch (Exception)
        {
            if (!await IsServerAvailable(BoltFixture.PlainUri))
            {
                Assert.Skip(BoltFixture.SkipPlain);
                return;
            }
            throw;
        }

        Assert.Single(result.Records);
        Assert.Equal(1L, result.Records[0]["x"]);
    }

    [Fact]
    public async Task Pool_ConcurrentQueries()
    {
        await using var pool = BoltPool.Create(BoltFixture.PlainUri,
            poolOptions: new BoltPoolOptions { MaxSize = 4 });

        try
        {
            var tasks = Enumerable.Range(1, 10).Select(async i =>
            {
                await using var client = await pool.AcquireAsync();
                var result = await client.RunAsync($"RETURN {i} AS n");
                return (long)result.Records[0]["n"]!;
            }).ToArray();

            var results = await Task.WhenAll(tasks);
            Assert.Equal(10, results.Length);
            Assert.Equal(55L, results.Sum()); // 1+2+...+10 = 55
        }
        catch (Exception)
        {
            if (!await IsServerAvailable(BoltFixture.PlainUri))
            {
                Assert.Skip(BoltFixture.SkipPlain);
                return;
            }
            throw;
        }
    }

    [Fact]
    public async Task TlsPool_AcquireAndRun()
    {
        await using var pool = BoltPool.Create(BoltFixture.TlsUri,
            poolOptions: new BoltPoolOptions { MaxSize = 4 });

        BoltResult result;
        try
        {
            await using var client = await pool.AcquireAsync(TestContext.Current.CancellationToken);
            result = await client.RunAsync("RETURN 'tls' AS protocol", ct: TestContext.Current.CancellationToken);
        }
        catch (Exception)
        {
            if (!await IsServerAvailable(BoltFixture.TlsUri))
            {
                Assert.Skip(BoltFixture.SkipTls);
                return;
            }
            throw;
        }

        Assert.Single(result.Records);
        Assert.Equal("tls", result.Records[0]["protocol"]);
    }

    private static async Task<bool> IsServerAvailable(string uri)
    {
        try
        {
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(2));
            await using var client = await BoltClient.ConnectAsync(uri, ct: cts.Token);
            return true;
        }
        catch
        {
            return false;
        }
    }
}
