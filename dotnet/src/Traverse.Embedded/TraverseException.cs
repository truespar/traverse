namespace Traverse;

/// <summary>
/// Exception thrown when a native Traverse operation fails.
/// </summary>
public sealed class TraverseException : Exception
{
    /// <summary>
    /// The native error code.
    /// </summary>
    public TvErrorCode ErrorCode { get; }

    /// <summary>
    /// The GQLSTATUS (ISO/IEC 39075): five characters naming the condition, such as
    /// <c>22012</c> (division by zero) or <c>42N62</c> (variable not defined). <c>null</c> when
    /// the error has none (an invalid argument).
    /// </summary>
    public string? GqlStatus { get; }

    /// <summary>
    /// Creates a new <see cref="TraverseException"/> with the given error code and message.
    /// </summary>
    public TraverseException(TvErrorCode errorCode, string message)
        : base(message)
    {
        ErrorCode = errorCode;
    }

    /// <summary>
    /// Creates a new <see cref="TraverseException"/> with the given error code, message and
    /// GQLSTATUS.
    /// </summary>
    public TraverseException(TvErrorCode errorCode, string message, string? gqlStatus)
        : base(message)
    {
        ErrorCode = errorCode;
        GqlStatus = gqlStatus;
    }

    /// <summary>
    /// Creates a new <see cref="TraverseException"/> with the given error code, message, and inner exception.
    /// </summary>
    public TraverseException(TvErrorCode errorCode, string message, Exception innerException)
        : base(message, innerException)
    {
        ErrorCode = errorCode;
    }
}
