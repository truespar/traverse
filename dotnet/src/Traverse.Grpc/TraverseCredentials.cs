using System.Text;

namespace Traverse.Grpc;

/// <summary>
/// Username/password credentials for Traverse authentication.
/// The Base64-encoded header value is pre-computed on construction.
/// </summary>
public sealed class TraverseCredentials
{
    public string Username { get; }
    internal string Base64Header { get; }

    public TraverseCredentials(string username, string password)
    {
        Username = username;
        Base64Header = Convert.ToBase64String(Encoding.UTF8.GetBytes($"{username}:{password}"));
    }
}
