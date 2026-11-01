using System.Diagnostics;
using System.Net.Sockets;

namespace Traverse.Bolt;

/// <summary>Retry options for transient error handling.</summary>
public sealed class RetryOptions
{
    public static readonly RetryOptions Default = new();

    /// <summary>Initial delay before the first retry in milliseconds. Default: 1000.</summary>
    public double InitialDelayMs { get; init; } = 1000;

    /// <summary>Maximum total retry time in milliseconds. Default: 30000.</summary>
    public long MaxRetryTimeMs { get; init; } = 30_000;
}

/// <summary>
/// Retry helper with exponential backoff and jitter for transient Bolt errors.
/// Acquires a fresh connection from the pool on each attempt.
/// Retries on: transient server errors, transport failures, and timeouts.
/// </summary>
public static class BoltRetry
{
    public static async ValueTask<T> ExecuteAsync<T>(
        BoltPool pool,
        Func<PooledBoltClient, ValueTask<T>> work,
        RetryOptions? options = null,
        CancellationToken ct = default)
    {
        var opts = options ?? RetryOptions.Default;
        var sw = Stopwatch.StartNew();
        int retries = 0;
        double delayMs = opts.InitialDelayMs;
        List<Exception>? previousExceptions = null;

        while (true)
        {
            await using var client = await pool.AcquireAsync(ct);
            try
            {
                return await work(client);
            }
            catch (Exception ex) when (IsRetriable(ex) &&
                (retries < 2 || sw.ElapsedMilliseconds < opts.MaxRetryTimeMs))
            {
                (previousExceptions ??= []).Add(ex);
                retries++;
                var jitter = delayMs * 0.2;
                var actualDelay = delayMs - jitter + Random.Shared.NextDouble() * 2 * jitter;
                await Task.Delay((int)actualDelay, ct);
                delayMs *= 2.0;
            }
            catch (Exception ex) when (previousExceptions is { Count: > 0 })
            {
                previousExceptions.Add(ex);
                throw new AggregateException(
                    $"Failed after {retries + 1} attempts", previousExceptions);
            }
        }
    }

    public static async ValueTask ExecuteAsync(
        BoltPool pool,
        Func<PooledBoltClient, ValueTask> work,
        RetryOptions? options = null,
        CancellationToken ct = default)
    {
        await ExecuteAsync(pool, async client =>
        {
            await work(client);
            return 0;
        }, options, ct);
    }

    /// <summary>
    /// Determines if an exception is retriable. Matches Neo4j driver behavior:
    /// transient server errors, transport failures, and authorization expiry.
    /// </summary>
    private static bool IsRetriable(Exception ex) => ex switch
    {
        BoltException bolt => bolt.IsTransient
            || bolt.Code == "Neo.ClientError.Security.AuthorizationExpired",
        SocketException => true,
        IOException => true, // includes EndOfStreamException
        _ => false,
    };
}
