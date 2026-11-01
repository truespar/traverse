using Traverse.Grpc.Internal;
using Traverse.V1;
using Grpc.Core;
using Grpc.Core.Interceptors;
using Grpc.Net.Client;

namespace Traverse.Grpc;

/// <summary>
/// High-performance gRPC client for Traverse. Thread-safe, reusable.
/// </summary>
public sealed partial class TraverseGrpcClient : IAsyncDisposable
{
    private readonly GrpcChannel _channel;
    private readonly TraverseService.TraverseServiceClient _client;
    private readonly TraverseClientOptions _options;
    private readonly RowBufferPool _pool = new();

    private TraverseGrpcClient(GrpcChannel channel, TraverseService.TraverseServiceClient client, TraverseClientOptions options)
    {
        _channel = channel;
        _client = client;
        _options = options;
    }

    /// <summary>
    /// Connect to a Traverse gRPC server and return a ready-to-use client.
    /// </summary>
    /// <param name="address">Server address (e.g. "http://localhost:50051").</param>
    /// <param name="options">Connection options.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    public static async ValueTask<TraverseGrpcClient> ConnectAsync(
        string address,
        TraverseClientOptions? options = null,
        CancellationToken cancellationToken = default)
    {
        options ??= new TraverseClientOptions();
        var channel = ChannelFactory.Create(address, options);

        CallInvoker invoker = channel.CreateCallInvoker();
        if (options.Credentials is not null)
        {
            invoker = invoker.Intercept(new AuthInterceptor(options.Credentials));
        }

        var client = new TraverseService.TraverseServiceClient(invoker);

        // Warmup: issue a lightweight RPC to verify the server is reachable
        await client.GetNodeCountAsync(new GetNodeCountRequest(), cancellationToken: cancellationToken)
            .ResponseAsync.ConfigureAwait(false);

        return new TraverseGrpcClient(channel, client, options);
    }

    /// <summary>
    /// The default database configured for this client.
    /// </summary>
    public string Database => _options.Database;

    private CallOptions CreateCallOptions(CancellationToken cancellationToken = default)
    {
        var callOptions = new CallOptions(cancellationToken: cancellationToken);
        if (_options.EnableCompression)
        {
            callOptions = callOptions.WithWriteOptions(new WriteOptions(WriteFlags.NoCompress));
        }
        return callOptions;
    }

    private static async ValueTask<T> CallAsync<T>(Func<Task<T>> call)
    {
        try
        {
            return await call().ConfigureAwait(false);
        }
        catch (RpcException ex)
        {
            throw TraverseException.FromRpcException(ex);
        }
    }

    /// <summary>
    /// Begin an explicit transaction.
    /// </summary>
    public async ValueTask<TraverseTransaction> BeginTransactionAsync(
        string? database = null,
        IReadOnlyList<string>? bookmarks = null,
        string? accessMode = null,
        TimeSpan? timeout = null,
        CancellationToken cancellationToken = default)
    {
        var request = new BeginTransactionRequest
        {
            Database = database ?? _options.Database,
        };
        if (bookmarks is not null)
            request.Bookmarks.AddRange(bookmarks);
        if (accessMode is not null)
            request.AccessMode = accessMode;
        if (timeout is { } t)
            request.TimeoutMs = (long)t.TotalMilliseconds;

        var response = await CallAsync(() => _client.BeginTransactionAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return new TraverseTransaction(response.TransactionId, _client, this);
    }

    /// <summary>
    /// Open a persistent bidirectional query channel. Eliminates per-RPC stream setup overhead.
    /// </summary>
    public QueryChannel OpenQueryChannel()
    {
        var call = _client.QueryChannel(CreateCallOptions());
        return new QueryChannel(call, _options.Database);
    }

    public ValueTask DisposeAsync()
    {
        _channel.Dispose();
        return ValueTask.CompletedTask;
    }
}
