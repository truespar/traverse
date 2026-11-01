using Grpc.Core;

namespace Traverse.Grpc;

/// <summary>
/// Exception thrown by Traverse client operations.
/// Maps gRPC status codes to domain-specific error codes.
/// </summary>
public sealed class TraverseException : Exception
{
    public TraverseErrorCode ErrorCode { get; }
    public StatusCode GrpcStatusCode { get; }

    public TraverseException(string message, TraverseErrorCode errorCode, StatusCode grpcStatusCode)
        : base(message)
    {
        ErrorCode = errorCode;
        GrpcStatusCode = grpcStatusCode;
    }

    public TraverseException(string message, TraverseErrorCode errorCode, StatusCode grpcStatusCode, Exception innerException)
        : base(message, innerException)
    {
        ErrorCode = errorCode;
        GrpcStatusCode = grpcStatusCode;
    }

    internal static TraverseException FromRpcException(RpcException ex)
    {
        var code = ex.StatusCode switch
        {
            StatusCode.Unauthenticated => TraverseErrorCode.Unauthenticated,
            StatusCode.PermissionDenied => TraverseErrorCode.PermissionDenied,
            StatusCode.NotFound => TraverseErrorCode.NotFound,
            StatusCode.FailedPrecondition => TraverseErrorCode.PreconditionFailed,
            StatusCode.Unavailable => TraverseErrorCode.Unavailable,
            StatusCode.DeadlineExceeded => TraverseErrorCode.Timeout,
            StatusCode.Aborted => TraverseErrorCode.TransactionError,
            StatusCode.InvalidArgument => ClassifyInvalidArgument(ex.Status.Detail),
            StatusCode.Internal => ClassifyInternal(ex.Status.Detail),
            _ => TraverseErrorCode.Unknown,
        };

        return new TraverseException(ex.Status.Detail, code, ex.StatusCode, ex);
    }

    private static TraverseErrorCode ClassifyInvalidArgument(string detail)
    {
        if (detail.Contains("syntax", StringComparison.OrdinalIgnoreCase))
            return TraverseErrorCode.SyntaxError;
        if (detail.Contains("type error", StringComparison.OrdinalIgnoreCase))
            return TraverseErrorCode.TypeError;
        return TraverseErrorCode.SemanticError;
    }

    private static TraverseErrorCode ClassifyInternal(string detail)
    {
        if (detail.Contains("runtime error", StringComparison.OrdinalIgnoreCase))
            return TraverseErrorCode.RuntimeError;
        return TraverseErrorCode.GraphError;
    }
}
