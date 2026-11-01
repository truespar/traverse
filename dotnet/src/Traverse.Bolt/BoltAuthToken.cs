namespace Traverse.Bolt;

/// <summary>
/// Authentication token for Bolt LOGON. Supports basic, bearer, kerberos, and custom schemes.
/// </summary>
public sealed class BoltAuthToken
{
    public string Scheme { get; init; } = "none";
    public string? Principal { get; init; }
    public string? Credentials { get; init; }
    public string? Realm { get; init; }
    public Dictionary<string, object>? Parameters { get; init; }

    public static BoltAuthToken None() => new() { Scheme = "none" };

    public static BoltAuthToken Basic(string username, string password, string? realm = null)
        => new() { Scheme = "basic", Principal = username, Credentials = password, Realm = realm };

    public static BoltAuthToken Bearer(string token)
        => new() { Scheme = "bearer", Credentials = token };

    public static BoltAuthToken Kerberos(string ticket)
        => new() { Scheme = "kerberos", Credentials = ticket };

    public static BoltAuthToken Custom(string scheme, string? principal, string? credentials,
        string? realm, Dictionary<string, object>? parameters)
        => new() { Scheme = scheme, Principal = principal, Credentials = credentials,
                   Realm = realm, Parameters = parameters };
}
