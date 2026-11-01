using System.Net.Sockets;
using System.Runtime.InteropServices;
using Grpc.Core;
using Grpc.Net.Client;
using Grpc.Net.Compression;

namespace Traverse.Grpc.Internal;

/// <summary>
/// Creates a configured <see cref="GrpcChannel"/> with TCP NoDelay, keepalive, and compression.
/// </summary>
internal static class ChannelFactory
{
    public static GrpcChannel Create(string address, TraverseClientOptions options)
    {
        var handler = new SocketsHttpHandler
        {
            EnableMultipleHttp2Connections = true,
            KeepAlivePingDelay = options.KeepAliveInterval,
            KeepAlivePingTimeout = options.KeepAliveTimeout,
            KeepAlivePingPolicy = HttpKeepAlivePingPolicy.Always,
            ConnectCallback = async (context, ct) =>
            {
                var socket = new Socket(AddressFamily.InterNetworkV6, SocketType.Stream, ProtocolType.Tcp)
                {
                    NoDelay = true,
                    DualMode = true,
                };
                await socket.ConnectAsync(context.DnsEndPoint, ct).ConfigureAwait(false);
                if (RuntimeInformation.IsOSPlatform(OSPlatform.Linux))
                    return new QuickAckStream(socket);
                return new NetworkStream(socket, ownsSocket: true);
            },
        };

        var channelOptions = new GrpcChannelOptions
        {
            HttpHandler = handler,
            MaxSendMessageSize = options.MaxSendMessageSize,
            MaxReceiveMessageSize = options.MaxReceiveMessageSize,
            DisposeHttpClient = true,
        };

        if (options.EnableCompression)
        {
            channelOptions.CompressionProviders = [new GzipCompressionProvider(System.IO.Compression.CompressionLevel.Fastest)];
        }

        return GrpcChannel.ForAddress(address, channelOptions);
    }
}

/// <summary>
/// A NetworkStream wrapper that re-arms TCP_QUICKACK before every read on Linux.
/// TCP_QUICKACK is not persistent - the kernel resets it after each delayed ACK decision.
/// Without this, gRPC server-streaming responses stall for 40ms (the Linux delayed-ACK timer)
/// because the server's h2 event loop only flushes buffered DATA frames when woken by I/O,
/// and the client's delayed ACK is the only I/O event.
/// </summary>
internal sealed class QuickAckStream : NetworkStream
{
    private const int IPPROTO_TCP = 6;
    private const int TCP_QUICKACK = 12;
    private static readonly byte[] s_enabled = BitConverter.GetBytes(1);

    private readonly Socket _socket;

    public QuickAckStream(Socket socket) : base(socket, ownsSocket: true)
    {
        _socket = socket;
        ArmQuickAck();
    }

    private void ArmQuickAck()
    {
        try { _socket.SetRawSocketOption(IPPROTO_TCP, TCP_QUICKACK, s_enabled); }
        catch { /* best-effort */ }
    }

    public override int Read(byte[] buffer, int offset, int count)
    {
        ArmQuickAck();
        return base.Read(buffer, offset, count);
    }

    public override int Read(Span<byte> buffer)
    {
        ArmQuickAck();
        return base.Read(buffer);
    }

    public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken)
    {
        ArmQuickAck();
        return base.ReadAsync(buffer, offset, count, cancellationToken);
    }

    public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
    {
        ArmQuickAck();
        return base.ReadAsync(buffer, cancellationToken);
    }

    public override int ReadByte()
    {
        ArmQuickAck();
        return base.ReadByte();
    }
}
