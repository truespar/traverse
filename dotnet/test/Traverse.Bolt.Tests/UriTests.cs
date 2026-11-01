using Xunit;

namespace Traverse.Bolt.Tests;

/// <summary>Unit tests for BoltUri parsing - no server needed.</summary>
public sealed class UriTests
{
    [Fact]
    public void Parse_PlainBolt()
    {
        var (host, port, tls, untrusted) = BoltUri.Parse("bolt://localhost:7690");

        Assert.Equal("localhost", host);
        Assert.Equal(7690, port);
        Assert.False(tls);
        Assert.False(untrusted);
    }

    [Fact]
    public void Parse_BoltTls()
    {
        var (host, port, tls, untrusted) = BoltUri.Parse("bolt+s://example.com:7690");

        Assert.Equal("example.com", host);
        Assert.Equal(7690, port);
        Assert.True(tls);
        Assert.False(untrusted);
    }

    [Fact]
    public void Parse_BoltSelfSigned()
    {
        var (host, port, tls, untrusted) = BoltUri.Parse("bolt+ssc://192.0.2.10:7690");

        Assert.Equal("192.0.2.10", host);
        Assert.Equal(7690, port);
        Assert.True(tls);
        Assert.True(untrusted);
    }

    [Fact]
    public void Parse_DefaultPort()
    {
        var (host, port, tls, _) = BoltUri.Parse("bolt://myhost");

        Assert.Equal("myhost", host);
        Assert.Equal(7687, port);
        Assert.False(tls);
    }

    [Fact]
    public void Parse_InvalidScheme_Throws()
    {
        Assert.ThrowsAny<ArgumentException>(() => BoltUri.Parse("http://localhost:7690"));
    }

    [Fact]
    public void Parse_InvalidUri_Throws()
    {
        Assert.ThrowsAny<ArgumentException>(() => BoltUri.Parse("not a uri at all"));
    }
}
