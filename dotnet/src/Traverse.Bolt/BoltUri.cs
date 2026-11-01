namespace Traverse.Bolt;

/// <summary>Parses Bolt connection URIs: bolt://, bolt+s://, bolt+ssc://</summary>
internal static class BoltUri
{
    private const int DefaultPort = 7687;

    /// <summary>
    /// Parse a Bolt URI and return (host, port, useTls, allowUntrustedCertificate).
    /// Supported schemes: bolt (plain), bolt+s (TLS), bolt+ssc (TLS + self-signed).
    /// </summary>
    public static (string host, int port, bool useTls, bool allowUntrustedCertificate) Parse(string uri)
    {
        if (!Uri.TryCreate(uri, UriKind.Absolute, out var parsed))
            throw new ArgumentException($"Invalid Bolt URI: {uri}", nameof(uri));

        bool useTls;
        bool allowUntrusted;

        switch (parsed.Scheme.ToLowerInvariant())
        {
            case "bolt":
                useTls = false;
                allowUntrusted = false;
                break;
            case "bolt+s":
                useTls = true;
                allowUntrusted = false;
                break;
            case "bolt+ssc":
                useTls = true;
                allowUntrusted = true;
                break;
            default:
                throw new ArgumentException($"Unsupported Bolt URI scheme: {parsed.Scheme}", nameof(uri));
        }

        string host = parsed.Host;
        int port = parsed.Port > 0 ? parsed.Port : DefaultPort;

        if (string.IsNullOrEmpty(host))
            throw new ArgumentException("Bolt URI must include a host", nameof(uri));

        return (host, port, useTls, allowUntrusted);
    }
}
