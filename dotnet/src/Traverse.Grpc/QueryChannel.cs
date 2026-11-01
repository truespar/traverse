using Traverse.Grpc.Values;
using Traverse.V1;
using Grpc.Core;

namespace Traverse.Grpc;

/// <summary>
/// Persistent bidirectional query channel. Eliminates per-RPC HTTP/2 stream setup
/// by keeping one long-lived bidi stream open. Queries are serialized.
/// </summary>
public sealed class QueryChannel : IAsyncDisposable
{
    private readonly AsyncDuplexStreamingCall<QueryChannelRequest, QueryChannelResponse> _call;
    private readonly string _defaultDatabase;
    private ulong _nextRequestId;

    internal QueryChannel(
        AsyncDuplexStreamingCall<QueryChannelRequest, QueryChannelResponse> call,
        string defaultDatabase)
    {
        _call = call;
        _defaultDatabase = defaultDatabase;
    }

    /// <summary>
    /// Execute a query over the channel, consume all rows, and return the summary.
    /// </summary>
    public async ValueTask<QuerySummary> RunAndConsumeAsync(
        string query,
        IReadOnlyDictionary<string, TvValue>? parameters = null,
        CancellationToken ct = default)
    {
        var requestId = _nextRequestId++;

        var req = new QueryChannelRequest
        {
            RequestId = requestId,
            Query = query,
            Database = _defaultDatabase,
        };

        if (parameters is not null)
        {
            foreach (var (k, v) in parameters)
                req.Parameters.Add(k, ValueConverter.ToProto(v));
        }

        await _call.RequestStream.WriteAsync(req, ct).ConfigureAwait(false);

        // Read responses until we get the footer or error for this request_id
        var responseStream = _call.ResponseStream;
        QuerySummary? summary = null;

        while (await responseStream.MoveNext(ct).ConfigureAwait(false))
        {
            var resp = responseStream.Current;
            if (resp.RequestId != requestId)
                continue;

            switch (resp.PayloadCase)
            {
                case QueryChannelResponse.PayloadOneofCase.Header:
                case QueryChannelResponse.PayloadOneofCase.Row:
                    // consume and discard
                    break;

                case QueryChannelResponse.PayloadOneofCase.Footer:
                    summary = QuerySummary.FromFooter(resp.Footer);
                    return summary;

                case QueryChannelResponse.PayloadOneofCase.Error:
                    var err = resp.Error;
                    var errorCode = err.Code switch
                    {
                        3 => ClassifyError(err.Message),
                        5 => TraverseErrorCode.NotFound,
                        7 => TraverseErrorCode.PermissionDenied,
                        8 => TraverseErrorCode.Unavailable,
                        _ => TraverseErrorCode.GraphError,
                    };
                    var grpcCode = err.Code switch
                    {
                        3 => StatusCode.InvalidArgument,
                        5 => StatusCode.NotFound,
                        7 => StatusCode.PermissionDenied,
                        8 => StatusCode.ResourceExhausted,
                        _ => StatusCode.Internal,
                    };
                    throw new TraverseException(err.Message, errorCode, grpcCode);
            }
        }

        throw new TraverseException(
            "channel closed before query completed",
            TraverseErrorCode.Unavailable,
            StatusCode.Unavailable);
    }

    private static TraverseErrorCode ClassifyError(string message)
    {
        if (message.Contains("syntax", StringComparison.OrdinalIgnoreCase))
            return TraverseErrorCode.SyntaxError;
        if (message.Contains("type error", StringComparison.OrdinalIgnoreCase))
            return TraverseErrorCode.TypeError;
        return TraverseErrorCode.SemanticError;
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            await _call.RequestStream.CompleteAsync().ConfigureAwait(false);
        }
        catch
        {
            // stream may already be closed
        }
        _call.Dispose();
    }
}
