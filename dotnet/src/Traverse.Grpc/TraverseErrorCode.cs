namespace Traverse.Grpc;

/// <summary>
/// Categorized error codes for Traverse operations.
/// </summary>
public enum TraverseErrorCode
{
    Unknown,
    SyntaxError,
    TypeError,
    SemanticError,
    GraphError,
    RuntimeError,
    NotFound,
    Unauthenticated,
    PermissionDenied,
    TransactionError,
    PreconditionFailed,
    Unavailable,
    Timeout,
}
