using System.Collections.Concurrent;
using System.Diagnostics;

namespace Traverse.Bolt;

/// <summary>
/// Connection pool for BoltClient instances. Lazily creates connections on demand
/// and limits concurrency with a semaphore. Supports connection lifetime rotation,
/// idle eviction, liveness probing, and background maintenance.
/// </summary>
public sealed class BoltPool : IAsyncDisposable
{
    private readonly string _host;
    private readonly int _port;
    private readonly BoltClientOptions? _options;
    private readonly BoltPoolOptions _poolOptions;
    private readonly SemaphoreSlim _semaphore;
    private readonly ConcurrentQueue<PooledEntry> _idle = new();
    private readonly Timer? _maintenanceTimer;
    private volatile bool _disposed;

    // Metrics
    private int _inUse;
    private long _created;

    /// <summary>Number of connections currently checked out from the pool.</summary>
    public int InUse => Volatile.Read(ref _inUse);

    /// <summary>Number of idle connections available in the pool.</summary>
    public int Idle => _idle.Count;

    /// <summary>Total number of connections created since pool creation.</summary>
    public long Created => Volatile.Read(ref _created);

    private record PooledEntry(BoltClient Client, long CreatedTicks, long IdleSinceTicks);

    private BoltPool(string host, int port, BoltClientOptions? options, BoltPoolOptions poolOptions)
    {
        _host = host;
        _port = port;
        _options = options;
        _poolOptions = poolOptions;
        _semaphore = new SemaphoreSlim(poolOptions.MaxSize, poolOptions.MaxSize);

        // Start background maintenance if idle timeout is configured
        if (poolOptions.IdleMaintenanceInterval is { } interval)
        {
            _maintenanceTimer = new Timer(
                static state => _ = ((BoltPool)state!).RunMaintenanceAsync(),
                this,
                interval,
                interval);
        }
    }

    /// <summary>Create a new connection pool.</summary>
    public static BoltPool Create(string host, int port, BoltClientOptions? options = null,
        BoltPoolOptions? poolOptions = null)
    {
        poolOptions ??= new BoltPoolOptions();
        if (poolOptions.MaxSize < 1) throw new ArgumentOutOfRangeException(nameof(poolOptions));
        return new BoltPool(host, port, options, poolOptions);
    }

    /// <summary>Create a new connection pool with a simple max size.</summary>
    public static BoltPool Create(string host, int port, BoltClientOptions? options, int maxSize)
        => Create(host, port, options, new BoltPoolOptions { MaxSize = maxSize });

    /// <summary>Create a pool from a Bolt URI: bolt://, bolt+s://, bolt+ssc://</summary>
    public static BoltPool Create(string uri, BoltClientOptions? options = null,
        BoltPoolOptions? poolOptions = null)
    {
        var (host, port, useTls, allowUntrusted) = BoltUri.Parse(uri);
        if (useTls || allowUntrusted)
        {
            options = new BoltClientOptions
            {
                Username = options?.Username,
                Password = options?.Password,
                Database = options?.Database,
                UseTls = useTls || (options?.UseTls ?? false),
                AllowUntrustedCertificate = allowUntrusted || (options?.AllowUntrustedCertificate ?? false),
                Notifications = options?.Notifications,
                FetchSize = options?.FetchSize ?? -1,
                ConnectionTimeout = options?.ConnectionTimeout ?? TimeSpan.FromSeconds(30),
            };
        }
        return Create(host, port, options, poolOptions);
    }

    /// <summary>Create a pool from a Bolt URI with a simple max size.</summary>
    public static BoltPool Create(string uri, BoltClientOptions? options, int maxSize)
        => Create(uri, options, new BoltPoolOptions { MaxSize = maxSize });

    /// <summary>
    /// Acquire a pooled connection. Blocks if the pool is exhausted until a connection is returned
    /// or the acquisition timeout expires.
    /// </summary>
    public async ValueTask<PooledBoltClient> AcquireAsync(CancellationToken ct = default)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);

        using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        cts.CancelAfter(_poolOptions.ConnectionAcquisitionTimeout);

        try
        {
            await _semaphore.WaitAsync(cts.Token);
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            throw new TimeoutException(
                $"Timed out waiting for a connection from the pool after {_poolOptions.ConnectionAcquisitionTimeout}.");
        }

        try
        {
            var now = Stopwatch.GetTimestamp();

            while (_idle.TryDequeue(out var entry))
            {
                var age = Stopwatch.GetElapsedTime(entry.CreatedTicks, now);
                if (age > _poolOptions.MaxConnectionLifetime)
                {
                    Discard(entry.Client);
                    continue;
                }

                var idle = Stopwatch.GetElapsedTime(entry.IdleSinceTicks, now);
                if (_poolOptions.ConnectionIdleTimeout is { } idleTimeout && idle > idleTimeout)
                {
                    Discard(entry.Client);
                    continue;
                }

                if (_poolOptions.ConnectionLivenessThreshold is { } threshold && idle > threshold)
                {
                    try
                    {
                        await entry.Client.ResetAsync(ct);
                    }
                    catch
                    {
                        Discard(entry.Client);
                        continue;
                    }
                }

                Interlocked.Increment(ref _inUse);
                return new PooledBoltClient(entry.Client, this);
            }

            var client = await BoltClient.ConnectAsync(_host, _port, _options, ct);
            Interlocked.Increment(ref _created);
            Interlocked.Increment(ref _inUse);
            return new PooledBoltClient(client, this);
        }
        catch
        {
            _semaphore.Release();
            throw;
        }
    }

    /// <summary>Return a connection to the pool or discard if broken.</summary>
    internal void Return(BoltClient client, bool isBroken)
    {
        Interlocked.Decrement(ref _inUse);

        if (isBroken || _disposed)
            Discard(client);
        else
            _idle.Enqueue(new PooledEntry(client, client.CreatedTicks, Stopwatch.GetTimestamp()));
        _semaphore.Release();
    }

    /// <summary>Closes <paramref name="client"/> in the background; a failed goodbye doesn't matter.</summary>
    private static void Discard(BoltClient client) => _ = DiscardAsync(client);

    private static async Task DiscardAsync(BoltClient client)
    {
        try { await client.DisposeAsync().ConfigureAwait(false); }
        catch { /* the connection is going away anyway */ }
    }

    /// <summary>Background maintenance: evict expired and idle connections.</summary>
    private async Task RunMaintenanceAsync()
    {
        if (_disposed) return;

        try
        {
            var now = Stopwatch.GetTimestamp();
            int count = _idle.Count;

            for (int i = 0; i < count; i++)
            {
                if (!_idle.TryDequeue(out var entry)) break;

                var age = Stopwatch.GetElapsedTime(entry.CreatedTicks, now);
                // an idle connection holds no permit (Return released it), so evicting one
                // releases nothing
                if (age > _poolOptions.MaxConnectionLifetime)
                {
                    await DiscardAsync(entry.Client);
                    continue;
                }

                var idle = Stopwatch.GetElapsedTime(entry.IdleSinceTicks, now);
                if (_poolOptions.ConnectionIdleTimeout is { } idleTimeout && idle > idleTimeout)
                {
                    await DiscardAsync(entry.Client);
                    continue;
                }

                // Still valid - re-enqueue
                _idle.Enqueue(entry);
            }
        }
        catch
        {
            // Best-effort maintenance
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (_disposed) return;
        _disposed = true;

        if (_maintenanceTimer is not null)
            await _maintenanceTimer.DisposeAsync();

        while (_idle.TryDequeue(out var entry))
            await DiscardAsync(entry.Client);

        // the semaphore stays: clients still checked out release it when they come back (it
        // holds no OS handle unless AvailableWaitHandle is used)
    }
}
