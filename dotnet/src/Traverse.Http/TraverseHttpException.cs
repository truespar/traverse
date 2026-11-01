using System.Net;

namespace Traverse.Http;

/// <summary>
/// Exception thrown when a Traverse HTTP API call fails.
/// </summary>
public sealed class TraverseHttpException : Exception
{
    /// <summary>HTTP status code, if the error came from an HTTP response.</summary>
    public HttpStatusCode? StatusCode { get; }

    public TraverseHttpException(string message, HttpStatusCode? statusCode = null)
        : base(message)
    {
        StatusCode = statusCode;
    }
}
