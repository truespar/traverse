using System.Buffers;
using System.Buffers.Binary;
using System.Net.Security;
using System.Net.Sockets;
using System.Runtime.InteropServices;

namespace Traverse.Bolt.Internal;

/// <summary>
/// Low-level Bolt connection: TCP socket, handshake, chunk framing, and message I/O.
/// Uses a read-ahead buffer to minimize syscalls - one large socket read serves
/// many chunk headers and chunk bodies, critical for queries returning many records.
/// </summary>
internal sealed class BoltConnection : IAsyncDisposable
{
    private const int MaxChunkSize = 65535;
    private const int SendBufSize = 65536;
    // 256KB read-ahead buffer - large enough to hold many messages from one recv()
    private const int ReadAheadSize = 256 * 1024;
    // Message assembly buffer - grows if needed for very large messages
    private const int InitialMsgBufSize = 256 * 1024;

    private const int IPPROTO_TCP = 6;
    private const int TCP_QUICKACK = 12;
    private static readonly byte[] QuickAckValue = [1, 0, 0, 0];

    private readonly Socket _socket;
    private readonly Stream _stream;
    private readonly byte[] _sendBuf;

    // Read-ahead ring buffer: data arrives from socket into _ioBuf[_ioEnd..],
    // consumed from _ioBuf[_ioPos.._ioEnd]
    private readonly byte[] _ioBuf;
    private int _ioPos;
    private int _ioEnd;

    // De-chunked message assembly buffer (only used for non-RECORD messages)
    private byte[] _msgBuf;

    // Pre-encoded PULL {n: -1} message (never changes)
    private readonly byte[] _pullMsg;
    private readonly int _pullMsgLen;

    // Pre-encoded DISCARD {n: -1} message (never changes)
    private readonly byte[] _discardMsg;
    private readonly int _discardMsgLen;

    // Separate framing buffer for SendRunPullAsync - avoids overlap with _sendBuf
    private readonly byte[] _frameBuf;

    private readonly bool _isLinux;
    private int _disposed;

    public int NegotiatedVersion { get; private set; }

    private BoltConnection(Socket socket, Stream stream)
    {
        _socket = socket;
        _stream = stream;
        _sendBuf = ArrayPool<byte>.Shared.Rent(SendBufSize);
        _ioBuf = ArrayPool<byte>.Shared.Rent(ReadAheadSize);
        _msgBuf = ArrayPool<byte>.Shared.Rent(InitialMsgBufSize);
        _frameBuf = ArrayPool<byte>.Shared.Rent(SendBufSize);
        _ioPos = 0;
        _ioEnd = 0;
        _isLinux = RuntimeInformation.IsOSPlatform(OSPlatform.Linux);

        // Pre-encode PULL {n: -1} - reused every query
        _pullMsg = new byte[32];
        int p = 0;
        PackStreamWriter.WriteStructHeader(_pullMsg, ref p, MsgTag.PULL, 1);
        PackStreamWriter.WriteMapHeader(_pullMsg, ref p, 1);
        PackStreamWriter.WriteString(_pullMsg, ref p, "n");
        PackStreamWriter.WriteInt64(_pullMsg, ref p, -1);
        _pullMsgLen = p;

        // Pre-encode DISCARD {n: -1} - reused every discard query
        _discardMsg = new byte[32];
        p = 0;
        PackStreamWriter.WriteStructHeader(_discardMsg, ref p, MsgTag.DISCARD, 1);
        PackStreamWriter.WriteMapHeader(_discardMsg, ref p, 1);
        PackStreamWriter.WriteString(_discardMsg, ref p, "n");
        PackStreamWriter.WriteInt64(_discardMsg, ref p, -1);
        _discardMsgLen = p;
    }

    /// <summary>Connect to a Bolt server and perform the version handshake.</summary>
    public static async ValueTask<BoltConnection> ConnectAsync(
        string host, int port,
        bool useTls = false, bool allowUntrustedCertificate = false,
        TimeSpan? connectionTimeout = null,
        CancellationToken ct = default)
    {
        var socket = new Socket(AddressFamily.InterNetworkV6, SocketType.Stream, ProtocolType.Tcp)
        {
            DualMode = true,
            NoDelay = true,
        };

        try
        {
            if (connectionTimeout is { } timeout)
            {
                using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
                cts.CancelAfter(timeout);
                await socket.ConnectAsync(host, port, cts.Token);
            }
            else
            {
                await socket.ConnectAsync(host, port, ct);
            }
        }
        catch
        {
            socket.Dispose();
            throw;
        }

        if (RuntimeInformation.IsOSPlatform(OSPlatform.Linux))
        {
            try { socket.SetRawSocketOption(IPPROTO_TCP, TCP_QUICKACK, QuickAckValue); }
            catch { }
        }

        Stream stream;
        try
        {
            var networkStream = new NetworkStream(socket, ownsSocket: false);
            if (useTls)
            {
#pragma warning disable CA5359 // bolt+ssc:// and AllowUntrustedCertificate opt into self-signed servers
                var sslStream = new SslStream(
                    networkStream,
                    leaveInnerStreamOpen: false,
                    allowUntrustedCertificate ? (_, _, _, _) => true : null);
#pragma warning restore CA5359
                await sslStream.AuthenticateAsClientAsync(host);
                stream = sslStream;
            }
            else
            {
                stream = networkStream;
            }
        }
        catch
        {
            socket.Dispose();
            throw;
        }

        var conn = new BoltConnection(socket, stream);
        try
        {
            await conn.HandshakeAsync(ct);
        }
        catch
        {
            await conn.DisposeAsync();
            throw;
        }
        return conn;
    }

    // Versions we support, expressed as (major * 100 + minor)
    private static readonly int[] SupportedVersions =
    [
        600,
        508, 507, 506, 505, 504, 503, 502, 501, 500,
        404, 403, 402
    ];

    private async ValueTask HandshakeAsync(CancellationToken ct)
    {
        var handshake = new byte[20];
        // Magic: 60 60 B0 17
        handshake[0] = 0x60; handshake[1] = 0x60; handshake[2] = 0xB0; handshake[3] = 0x17;
        // Slot 0: Manifest v1 (major=0xFF, minor=1) - manifest-capable Bolt 6+ servers
        handshake[4] = 0x00; handshake[5] = 0x00; handshake[6] = 0x01; handshake[7] = 0xFF;
        // Slot 1: Bolt 6.0 (classic proposal for servers that don't use manifest)
        handshake[8] = 0x00; handshake[9] = 0x00; handshake[10] = 0x00; handshake[11] = 0x06;
        // Slot 2: Version 5.8 (range 5.0-5.8)
        handshake[12] = 0x00; handshake[13] = 0x08; handshake[14] = 0x08; handshake[15] = 0x05;
        // Slot 3: Version 4.4 (range 4.2-4.4)
        handshake[16] = 0x00; handshake[17] = 0x02; handshake[18] = 0x04; handshake[19] = 0x04;

        await _stream.WriteAsync(handshake, ct);

        // Read 4-byte version response
        await EnsureBufferedAsync(4, ct);
        byte r0 = _ioBuf[_ioPos++], r1 = _ioBuf[_ioPos++], r2 = _ioBuf[_ioPos++], r3 = _ioBuf[_ioPos++];

        if (r0 == 0 && r1 == 0 && r2 == 0 && r3 == 0)
            throw new BoltException("Bolt.Handshake", "Server rejected all proposed Bolt versions");

        if (r3 == 0xFF)
        {
            // Manifest-style handshake (Bolt 6+): server sends available versions + capabilities
            if (r2 != 1)
                throw new BoltException("Bolt.Handshake", $"Unsupported manifest version {r2}");

            await NegotiateManifestAsync(ct);
        }
        else
        {
            NegotiatedVersion = r3 * 100 + r1;
        }
    }

    private async ValueTask NegotiateManifestAsync(CancellationToken ct)
    {
        // Read number of protocol versions (VarInt)
        int versionCount = await ReadVarIntAsync(ct);
        if (versionCount == 0)
            throw new BoltException("Bolt.Handshake", "Server manifest contains no protocol versions");

        // Read version entries (4 bytes each: [padding, range, minor, major] big-endian)
        // and find the highest version we mutually support
        int bestVersion = 0;
        await EnsureBufferedAsync(versionCount * 4, ct);

        for (int i = 0; i < versionCount; i++)
        {
            byte v0 = _ioBuf[_ioPos++], v1 = _ioBuf[_ioPos++], v2 = _ioBuf[_ioPos++], v3 = _ioBuf[_ioPos++];
            int major = v3;
            int minor = v2;
            int range = v1;

            // Expand version range and find best match
            for (int r = 0; r <= range; r++)
            {
                int candidateMinor = minor - r;
                if (candidateMinor < 0) break;
                int candidate = major * 100 + candidateMinor;
                if (candidate > bestVersion && Array.IndexOf(SupportedVersions, candidate) >= 0)
                    bestVersion = candidate;
            }
        }

        // Read capability bitmask (VarInt) - reserved for future use
        _ = await ReadVarIntAsync(ct);

        if (bestVersion == 0)
            throw new BoltException("Bolt.Handshake", "No mutually supported Bolt version found in manifest");

        // Send selected version (4 bytes big-endian) + capabilities VarInt (0x00 = none)
        var response = new byte[5];
        int selectedMajor = bestVersion / 100;
        int selectedMinor = bestVersion % 100;
        // [padding, 0, minor, major] big-endian
        response[0] = 0;
        response[1] = 0;
        response[2] = (byte)selectedMinor;
        response[3] = (byte)selectedMajor;
        response[4] = 0x00; // No capabilities
        await _stream.WriteAsync(response, ct);

        NegotiatedVersion = bestVersion;
    }

    private async ValueTask<int> ReadVarIntAsync(CancellationToken ct)
    {
        long value = 0;
        int shift = 0;
        while (true)
        {
            await EnsureBufferedAsync(1, ct);
            byte b = _ioBuf[_ioPos++];
            value |= (long)(b & 0x7F) << shift;
            if ((b & 0x80) == 0) break;
            shift += 7;
            if (shift > 63)
                throw new BoltException("Bolt.Handshake", "VarInt overflow in manifest");
        }
        return (int)value;
    }

    // -- Buffered read ------------------------------------------------------

    /// <summary>
    /// Ensure at least <paramref name="count"/> bytes are available in the read-ahead buffer.
    /// If not, compact and refill from the socket. This is the only method that does syscalls
    /// on the read path - everything else reads from the buffer.
    /// </summary>
    private async ValueTask EnsureBufferedAsync(int count, CancellationToken ct)
    {
        int available = _ioEnd - _ioPos;
        if (available >= count)
            return;

        // Compact: move unconsumed data to the front
        if (_ioPos > 0)
        {
            if (available > 0)
                Buffer.BlockCopy(_ioBuf, _ioPos, _ioBuf, 0, available);
            _ioPos = 0;
            _ioEnd = available;
        }

        // Read from socket until we have enough
        while (_ioEnd - _ioPos < count)
        {
            RearmQuickAck();
            int read = await _stream.ReadAsync(
                _ioBuf.AsMemory(_ioEnd, _ioBuf.Length - _ioEnd), ct);
            if (read == 0)
                throw new EndOfStreamException("Bolt connection closed unexpectedly");
            _ioEnd += read;
        }
    }

    /// <summary>Get the message assembly buffer for PackStream decoding.</summary>
    public byte[] RecvBuffer => _msgBuf;

    // -- Receive: chunk de-framing from buffered reader ---------------------

    /// <summary>
    /// Receive a chunked Bolt message into the message assembly buffer.
    /// Reads chunk headers + data from the read-ahead buffer (minimal syscalls).
    /// Returns the total de-chunked message length.
    /// </summary>
    public async ValueTask<int> ReceiveMessageAsync(CancellationToken ct = default)
    {
        int msgLen = 0;

        while (true)
        {
            // Read 2-byte chunk header from buffer
            await EnsureBufferedAsync(2, ct);
            int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));
            _ioPos += 2;

            if (chunkSize == 0)
                break; // End of message

            // Ensure message assembly buffer has room
            EnsureMsgCapacity(msgLen + chunkSize);

            // Copy chunk data from read-ahead buffer to message buffer
            int remaining = chunkSize;
            while (remaining > 0)
            {
                await EnsureBufferedAsync(Math.Min(remaining, _ioBuf.Length), ct);
                int available = _ioEnd - _ioPos;
                int toCopy = Math.Min(remaining, available);
                Buffer.BlockCopy(_ioBuf, _ioPos, _msgBuf, msgLen, toCopy);
                _ioPos += toCopy;
                msgLen += toCopy;
                remaining -= toCopy;
            }
        }

        return msgLen;
    }

    /// <summary>
    /// Skip all remaining chunks of a message until the 0x0000 terminator.
    /// </summary>
    private async ValueTask SkipRemainingChunksAsync(CancellationToken ct)
    {
        while (true)
        {
            await EnsureBufferedAsync(2, ct);
            int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));
            _ioPos += 2;

            if (chunkSize == 0)
                break;

            // Skip chunk data - advance through buffer, refilling as needed
            int remaining = chunkSize;
            while (remaining > 0)
            {
                await EnsureBufferedAsync(Math.Min(remaining, _ioBuf.Length), ct);
                int available = _ioEnd - _ioPos;
                int skip = Math.Min(remaining, available);
                _ioPos += skip;
                remaining -= skip;
            }
        }
    }

    /// <summary>
    /// Skip an entire chunked message (first chunk + any remaining + terminator).
    /// </summary>
    private async ValueTask SkipFullMessageAsync(int firstChunkSize, CancellationToken ct)
    {
        // Skip first chunk data
        int remaining = firstChunkSize;
        while (remaining > 0)
        {
            await EnsureBufferedAsync(Math.Min(remaining, _ioBuf.Length), ct);
            int available = _ioEnd - _ioPos;
            int skip = Math.Min(remaining, available);
            _ioPos += skip;
            remaining -= skip;
        }
        // Skip any additional chunks + terminator
        await SkipRemainingChunksAsync(ct);
    }

    private void EnsureMsgCapacity(int needed)
    {
        if (needed <= _msgBuf.Length)
            return;
        int newSize = Math.Max(_msgBuf.Length * 2, needed);
        byte[] newBuf = ArrayPool<byte>.Shared.Rent(newSize);
        Buffer.BlockCopy(_msgBuf, 0, newBuf, 0, _msgBuf.Length);
        ArrayPool<byte>.Shared.Return(_msgBuf);
        _msgBuf = newBuf;
    }

    /// <summary>Read a complete Bolt message and return the struct tag and body offsets.</summary>
    public async ValueTask<(byte tag, int bodyOffset, int bodyLength)> ReadMessageAsync(CancellationToken ct = default)
    {
        int length = await ReceiveMessageAsync(ct);
        if (length < 2)
            throw new InvalidDataException($"Bolt message too short: {length} bytes");

        int pos = 0;
        var (tag, _) = PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
        return (tag, pos, length - pos);
    }

    /// <summary>Read a SUCCESS/FAILURE message's metadata map from the message buffer.</summary>
    public Dictionary<string, object?> ReadSuccessMetadata(int bodyOffset)
    {
        int pos = bodyOffset;
        return PackStreamReader.ReadMap(_msgBuf, ref pos);
    }

    /// <summary>
    /// Drain all PULL responses: skip RECORD messages at the chunk level (zero copy),
    /// assemble only the final SUCCESS to return summary metadata.
    /// </summary>
    public async ValueTask<Dictionary<string, object?>> DrainPullResponsesAsync(CancellationToken ct = default)
    {
        while (true)
        {
            // Peek at the first chunk to determine message tag without assembling.
            // We need: 2 bytes chunk header + at least 2 bytes (struct marker + tag)
            await EnsureBufferedAsync(4, ct);
            int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));

            if (chunkSize < 2)
                throw new InvalidDataException($"Bolt message chunk too small: {chunkSize}");

            // Peek at tag byte (struct marker at _ioPos+2, tag at _ioPos+3)
            byte tag = _ioBuf[_ioPos + 3];

            // Consume the chunk header we already peeked
            _ioPos += 2;

            if (tag == MsgTag.RECORD)
            {
                // Skip entire message - no copy, no parse
                await SkipFullMessageAsync(chunkSize, ct);
                continue;
            }

            if (tag == MsgTag.SUCCESS)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                return PackStreamReader.ReadMap(_msgBuf, ref pos);
            }

            if (tag == MsgTag.FAILURE)
            {
                // Assemble to extract error details, then RESET to recover connection
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                var metadata = PackStreamReader.ReadMap(_msgBuf, ref pos);
                await ResetAsync(ct);
                throw BoltException.FromMetadata(metadata, NegotiatedVersion);
            }

            throw new BoltException("Bolt.Protocol", $"Unexpected message tag 0x{tag:X2} during PULL");
        }
    }

    /// <summary>
    /// Handle a pipelined RUN FAILURE: read the FAILURE metadata, consume the
    /// IGNORED response for the pipelined PULL, then RESET to recover.
    /// Returns the BoltException to throw.
    /// </summary>
    public async ValueTask<BoltException> HandleRunFailureAsync(int bodyOffset, CancellationToken ct)
    {
        var metadata = ReadSuccessMetadata(bodyOffset);

        // Consume the IGNORED response for the pipelined PULL
        await EnsureBufferedAsync(4, ct);
        int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));
        _ioPos += 2;
        await SkipFullMessageAsync(chunkSize, ct);

        // RESET to recover to Ready state
        await ResetAsync(ct);

        return BoltException.FromMetadata(metadata, NegotiatedVersion);
    }

    /// <summary>
    /// Assemble the rest of a message whose first chunk size is already known
    /// (chunk header already consumed). Copies first chunk + remaining chunks into _msgBuf.
    /// </summary>
    private async ValueTask<int> AssembleCurrentMessageAsync(int firstChunkSize, CancellationToken ct)
    {
        int msgLen = 0;
        EnsureMsgCapacity(firstChunkSize);

        // Copy first chunk
        int remaining = firstChunkSize;
        while (remaining > 0)
        {
            await EnsureBufferedAsync(Math.Min(remaining, _ioBuf.Length), ct);
            int available = _ioEnd - _ioPos;
            int toCopy = Math.Min(remaining, available);
            Buffer.BlockCopy(_ioBuf, _ioPos, _msgBuf, msgLen, toCopy);
            _ioPos += toCopy;
            msgLen += toCopy;
            remaining -= toCopy;
        }

        // Copy remaining chunks
        while (true)
        {
            await EnsureBufferedAsync(2, ct);
            int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));
            _ioPos += 2;
            if (chunkSize == 0) break;

            EnsureMsgCapacity(msgLen + chunkSize);
            remaining = chunkSize;
            while (remaining > 0)
            {
                await EnsureBufferedAsync(Math.Min(remaining, _ioBuf.Length), ct);
                int available = _ioEnd - _ioPos;
                int toCopy = Math.Min(remaining, available);
                Buffer.BlockCopy(_ioBuf, _ioPos, _msgBuf, msgLen, toCopy);
                _ioPos += toCopy;
                msgLen += toCopy;
                remaining -= toCopy;
            }
        }

        return msgLen;
    }

    /// <summary>
    /// Send RESET and consume the SUCCESS response to return the connection to Ready state.
    /// Called after FAILURE to recover the connection for reuse.
    /// </summary>
    public async ValueTask ResetAsync(CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.RESET, 0);
        await SendMessageAsync(_sendBuf, pos, ct);

        // Read RESET response - should be SUCCESS
        var (tag, _, _) = await ReadMessageAsync(ct);
        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RESET, got 0x{tag:X2}");
    }

    // -- Send ---------------------------------------------------------------

    /// <summary>
    /// Send a message: chunk-frame and send in a single socket write.
    /// </summary>
    public async ValueTask SendMessageAsync(byte[] buffer, int length, CancellationToken ct = default)
    {
        int maxFramedSize = length + ((length / MaxChunkSize) + 1) * 2 + 2;
        byte[] framedBuf = maxFramedSize <= SendBufSize
            ? ArrayPool<byte>.Shared.Rent(maxFramedSize)
            : new byte[maxFramedSize];

        try
        {
            int fPos = 0;
            int offset = 0;
            while (offset < length)
            {
                int chunkSize = Math.Min(length - offset, MaxChunkSize);
                BinaryPrimitives.WriteUInt16BigEndian(framedBuf.AsSpan(fPos), (ushort)chunkSize);
                fPos += 2;
                Buffer.BlockCopy(buffer, offset, framedBuf, fPos, chunkSize);
                fPos += chunkSize;
                offset += chunkSize;
            }
            framedBuf[fPos++] = 0;
            framedBuf[fPos++] = 0;

            await _stream.WriteAsync(framedBuf.AsMemory(0, fPos), ct);
        }
        finally
        {
            if (maxFramedSize <= SendBufSize)
                ArrayPool<byte>.Shared.Return(framedBuf);
        }
    }

    /// <summary>Encode a RUN message into _sendBuf and return the encoded length.</summary>
    private int EncodeRunMessage(string cypher, Dictionary<string, object>? parameters, string? database,
        IReadOnlyList<string>? bookmarks = null, QueryOptions? queryOptions = null,
        NotificationsConfig? notifications = null)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.RUN, 3);
        PackStreamWriter.WriteString(_sendBuf, ref pos, cypher);

        if (parameters is { Count: > 0 })
        {
            PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, parameters.Count);
            foreach (var (key, value) in parameters)
            {
                PackStreamWriter.WriteString(_sendBuf, ref pos, key);
                PackStreamWriter.WriteValue(_sendBuf, ref pos, value);
            }
        }
        else
        {
            PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, 0);
        }

        WriteExtrasMap(_sendBuf, ref pos, database, bookmarks, queryOptions, notifications);

        return pos;
    }

    /// <summary>Write the extras/metadata map shared by RUN and BEGIN.</summary>
    private void WriteExtrasMap(byte[] buf, ref int pos, string? database,
        IReadOnlyList<string>? bookmarks, QueryOptions? opts, NotificationsConfig? notif)
    {
        int ver = NegotiatedVersion;
        bool isOff = notif?.MinimumSeverity == NotificationSeverity.Off;
        // When severity is Off, all notifications are disabled - skip categories/classifications
        bool useClassifications = !isOff && ver >= 505 && notif?.DisabledCategories is { Length: > 0 };
        bool useCategories = !isOff && ver >= 502 && ver < 505 && notif?.DisabledCategories is { Length: > 0 };
        bool useSeverity = ver >= 502 && notif?.MinimumSeverity is not null;

        int count = 0;
        if (database is not null) count++;
        if (bookmarks is { Count: > 0 }) count++;
        if (opts?.TxTimeout is not null) count++;
        if (opts?.TxMetadata is { Count: > 0 }) count++;
        if (opts?.Mode == AccessMode.Read) count++;
        if (opts?.ImpersonatedUser is not null) count++;
        if (useSeverity) count++;
        if (useCategories) count++;
        if (useClassifications) count++;
        PackStreamWriter.WriteMapHeader(buf, ref pos, count);

        if (database is not null)
        {
            PackStreamWriter.WriteString(buf, ref pos, "db");
            PackStreamWriter.WriteString(buf, ref pos, database);
        }
        if (bookmarks is { Count: > 0 })
        {
            PackStreamWriter.WriteString(buf, ref pos, "bookmarks");
            PackStreamWriter.WriteListHeader(buf, ref pos, bookmarks.Count);
            for (int i = 0; i < bookmarks.Count; i++)
                PackStreamWriter.WriteString(buf, ref pos, bookmarks[i]);
        }
        if (opts?.TxTimeout is { } timeout)
        {
            PackStreamWriter.WriteString(buf, ref pos, "tx_timeout");
            PackStreamWriter.WriteInt64(buf, ref pos, Math.Max(0L, (long)timeout.TotalMilliseconds));
        }
        if (opts?.TxMetadata is { Count: > 0 } meta)
        {
            PackStreamWriter.WriteString(buf, ref pos, "tx_metadata");
            PackStreamWriter.WriteMapHeader(buf, ref pos, meta.Count);
            foreach (var (key, value) in meta)
            {
                PackStreamWriter.WriteString(buf, ref pos, key);
                PackStreamWriter.WriteValue(buf, ref pos, value);
            }
        }
        if (opts?.Mode == AccessMode.Read)
        {
            PackStreamWriter.WriteString(buf, ref pos, "mode");
            PackStreamWriter.WriteString(buf, ref pos, "r");
        }
        if (opts?.ImpersonatedUser is { } impUser)
        {
            PackStreamWriter.WriteString(buf, ref pos, "imp_user");
            PackStreamWriter.WriteString(buf, ref pos, impUser);
        }
        if (useSeverity)
        {
            PackStreamWriter.WriteString(buf, ref pos, "notifications_minimum_severity");
            PackStreamWriter.WriteString(buf, ref pos, notif!.MinimumSeverity!.Value switch
            {
                NotificationSeverity.Off => "OFF",
                NotificationSeverity.Warning => "WARNING",
                NotificationSeverity.Information => "INFORMATION",
                _ => "OFF"
            });
        }
        if (useCategories)
        {
            var cats = notif!.DisabledCategories!;
            PackStreamWriter.WriteString(buf, ref pos, "notifications_disabled_categories");
            PackStreamWriter.WriteListHeader(buf, ref pos, cats.Length);
            for (int i = 0; i < cats.Length; i++)
                PackStreamWriter.WriteString(buf, ref pos, cats[i].ToString().ToUpperInvariant());
        }
        if (useClassifications)
        {
            var cats = notif!.DisabledCategories!;
            PackStreamWriter.WriteString(buf, ref pos, "notifications_disabled_classifications");
            PackStreamWriter.WriteListHeader(buf, ref pos, cats.Length);
            for (int i = 0; i < cats.Length; i++)
                PackStreamWriter.WriteString(buf, ref pos, cats[i].ToString().ToUpperInvariant());
        }
    }

    /// <summary>Frame RUN + a completion message into _frameBuf and send.</summary>
    private async ValueTask SendRunWithCompletionAsync(
        int runLen, byte[] completionMsg, int completionMsgLen, CancellationToken ct)
    {
        int fPos = 0;

        // Frame RUN: chunk header + data + terminator
        BinaryPrimitives.WriteUInt16BigEndian(_frameBuf.AsSpan(fPos), (ushort)runLen);
        fPos += 2;
        Buffer.BlockCopy(_sendBuf, 0, _frameBuf, fPos, runLen);
        fPos += runLen;
        _frameBuf[fPos++] = 0;
        _frameBuf[fPos++] = 0;

        // Frame completion: chunk header + pre-encoded data + terminator
        BinaryPrimitives.WriteUInt16BigEndian(_frameBuf.AsSpan(fPos), (ushort)completionMsgLen);
        fPos += 2;
        Buffer.BlockCopy(completionMsg, 0, _frameBuf, fPos, completionMsgLen);
        fPos += completionMsgLen;
        _frameBuf[fPos++] = 0;
        _frameBuf[fPos++] = 0;

        await _stream.WriteAsync(_frameBuf.AsMemory(0, fPos), ct);
    }

    /// <summary>
    /// Encode and send RUN + PULL pipelined in a single socket write.
    /// PULL is pre-encoded; both messages are framed into _frameBuf to avoid overlap
    /// with the RUN payload in _sendBuf.
    /// </summary>
    public async ValueTask SendRunPullAsync(
        string cypher, Dictionary<string, object>? parameters, string? database,
        IReadOnlyList<string>? bookmarks = null, QueryOptions? queryOptions = null,
        NotificationsConfig? notifications = null, CancellationToken ct = default)
    {
        int runLen = EncodeRunMessage(cypher, parameters, database, bookmarks, queryOptions, notifications);
        await SendRunWithCompletionAsync(runLen, _pullMsg, _pullMsgLen, ct);
    }

    /// <summary>
    /// Encode and send RUN + DISCARD pipelined in a single socket write.
    /// </summary>
    public async ValueTask SendRunDiscardAsync(
        string cypher, Dictionary<string, object>? parameters, string? database,
        IReadOnlyList<string>? bookmarks = null, QueryOptions? queryOptions = null,
        NotificationsConfig? notifications = null, CancellationToken ct = default)
    {
        int runLen = EncodeRunMessage(cypher, parameters, database, bookmarks, queryOptions, notifications);
        await SendRunWithCompletionAsync(runLen, _discardMsg, _discardMsgLen, ct);
    }

    /// <summary>
    /// Encode and send only a RUN message (no pipelined PULL/DISCARD).
    /// Used by streaming cursors that manage PULL separately.
    /// </summary>
    public async ValueTask SendRunOnlyAsync(
        string cypher, Dictionary<string, object>? parameters, string? database,
        IReadOnlyList<string>? bookmarks = null, QueryOptions? queryOptions = null,
        NotificationsConfig? notifications = null, CancellationToken ct = default)
    {
        int runLen = EncodeRunMessage(cypher, parameters, database, bookmarks, queryOptions, notifications);
        await SendMessageAsync(_sendBuf, runLen, ct);
    }

    /// <summary>Send a PULL message with the given fetch size (and optional qid).</summary>
    public async ValueTask SendPullAsync(long fetchSize, long qid = -1, CancellationToken ct = default)
    {
        if (fetchSize == -1 && qid == -1)
        {
            // Use pre-encoded PULL {n: -1}
            await SendMessageAsync(_pullMsg, _pullMsgLen, ct);
            return;
        }

        int mapCount = 1;
        if (qid != -1) mapCount++;

        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.PULL, 1);
        PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, mapCount);
        PackStreamWriter.WriteString(_sendBuf, ref pos, "n");
        PackStreamWriter.WriteInt64(_sendBuf, ref pos, fetchSize);
        if (qid != -1)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "qid");
            PackStreamWriter.WriteInt64(_sendBuf, ref pos, qid);
        }
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    /// <summary>
    /// Read a single PULL batch: materializes RECORD messages until SUCCESS.
    /// Returns records and whether more batches are available (has_more).
    /// </summary>
    public async ValueTask<(List<object?[]> records, bool hasMore, Dictionary<string, object?> summary)>
        ReadPullBatchAsync(CancellationToken ct = default)
    {
        var records = new List<object?[]>();

        while (true)
        {
            await EnsureBufferedAsync(4, ct);
            int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));

            if (chunkSize < 2)
                throw new InvalidDataException($"Bolt message chunk too small: {chunkSize}");

            byte tag = _ioBuf[_ioPos + 3];
            _ioPos += 2;

            if (tag == MsgTag.RECORD)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                int fieldCount = PackStreamReader.ReadListHeader(_msgBuf, ref pos);
                var values = new object?[fieldCount];
                for (int i = 0; i < fieldCount; i++)
                    values[i] = PackStreamReader.ReadValue(_msgBuf, ref pos);
                records.Add(values);
                continue;
            }

            if (tag == MsgTag.SUCCESS)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                var metadata = PackStreamReader.ReadMap(_msgBuf, ref pos);
                bool hasMore = metadata.TryGetValue("has_more", out var hm) && hm is true;
                return (records, hasMore, metadata);
            }

            if (tag == MsgTag.FAILURE)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                var metadata = PackStreamReader.ReadMap(_msgBuf, ref pos);
                await ResetAsync(ct);
                throw BoltException.FromMetadata(metadata, NegotiatedVersion);
            }

            throw new BoltException("Bolt.Protocol", $"Unexpected message tag 0x{tag:X2} during PULL batch");
        }
    }

    // -- Result reading ------------------------------------------------------

    /// <summary>
    /// Read all PULL responses, assembling RECORD messages into value arrays.
    /// Same chunk-level peeking as DrainPullResponsesAsync but materializes records.
    /// </summary>
    public async ValueTask<(List<object?[]> records, Dictionary<string, object?> summary)> ReadPullResponsesAsync(
        CancellationToken ct = default)
    {
        var records = new List<object?[]>();

        while (true)
        {
            await EnsureBufferedAsync(4, ct);
            int chunkSize = (int)BinaryPrimitives.ReadUInt16BigEndian(_ioBuf.AsSpan(_ioPos));

            if (chunkSize < 2)
                throw new InvalidDataException($"Bolt message chunk too small: {chunkSize}");

            byte tag = _ioBuf[_ioPos + 3];
            _ioPos += 2;

            if (tag == MsgTag.RECORD)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                int fieldCount = PackStreamReader.ReadListHeader(_msgBuf, ref pos);
                var values = new object?[fieldCount];
                for (int i = 0; i < fieldCount; i++)
                    values[i] = PackStreamReader.ReadValue(_msgBuf, ref pos);
                records.Add(values);
                continue;
            }

            if (tag == MsgTag.SUCCESS)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                var metadata = PackStreamReader.ReadMap(_msgBuf, ref pos);
                return (records, metadata);
            }

            if (tag == MsgTag.FAILURE)
            {
                int length = await AssembleCurrentMessageAsync(chunkSize, ct);
                int pos = 0;
                PackStreamReader.ReadStructHeader(_msgBuf, ref pos);
                var metadata = PackStreamReader.ReadMap(_msgBuf, ref pos);
                await ResetAsync(ct);
                throw BoltException.FromMetadata(metadata, NegotiatedVersion);
            }

            throw new BoltException("Bolt.Protocol", $"Unexpected message tag 0x{tag:X2} during PULL");
        }
    }

    /// <summary>
    /// Read the DISCARD response - a single SUCCESS or FAILURE (no RECORD messages).
    /// Returns summary metadata on SUCCESS, throws on FAILURE after RESET.
    /// </summary>
    public async ValueTask<Dictionary<string, object?>> ReadDiscardResponseAsync(CancellationToken ct = default)
    {
        var (tag, bodyOffset, _) = await ReadMessageAsync(ct);

        if (tag == MsgTag.SUCCESS)
            return ReadSuccessMetadata(bodyOffset);

        if (tag == MsgTag.FAILURE)
        {
            var metadata = ReadSuccessMetadata(bodyOffset);
            await ResetAsync(ct);
            throw BoltException.FromMetadata(metadata, NegotiatedVersion);
        }

        throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after DISCARD, got 0x{tag:X2}");
    }

    // -- Transaction message senders -----------------------------------------

    /// <summary>Send BEGIN with optional database, bookmarks, and query options in the extras map.</summary>
    public async ValueTask SendBeginAsync(string? database, IReadOnlyList<string>? bookmarks = null,
        QueryOptions? queryOptions = null, NotificationsConfig? notifications = null,
        CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.BEGIN, 1);
        WriteExtrasMap(_sendBuf, ref pos, database, bookmarks, queryOptions, notifications);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    /// <summary>Send COMMIT (0 fields).</summary>
    public async ValueTask SendCommitAsync(CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.COMMIT, 0);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    /// <summary>Send ROLLBACK (0 fields).</summary>
    public async ValueTask SendRollbackAsync(CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.ROLLBACK, 0);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    // -- High-level message senders -----------------------------------------

    public async ValueTask SendHelloAsync(string userAgent, NotificationsConfig? notifications = null,
        Dictionary<string, string>? routingContext = null, CancellationToken ct = default)
    {
        int ver = NegotiatedVersion;
        bool hasBoltAgent = ver >= 503;
        bool hasRouting = ver >= 401;
        bool hasPatchBolt = ver >= 403 && ver < 500;
        bool isOff = notifications?.MinimumSeverity == NotificationSeverity.Off;
        bool hasNotifSeverity = ver >= 502 && notifications?.MinimumSeverity is not null;
        // When severity is Off, all notifications are disabled - skip categories/classifications
        bool hasNotifCategories = !isOff && ver >= 502 && ver < 505 && notifications is not null;
        bool hasNotifClassifications = !isOff && ver >= 505 && notifications?.DisabledCategories is { Length: > 0 };

        int mapCount = 1; // user_agent always
        if (hasBoltAgent) mapCount++;
        if (hasRouting) mapCount++;
        if (hasPatchBolt) mapCount++;
        if (hasNotifSeverity) mapCount++;
        if (hasNotifCategories && notifications?.DisabledCategories is { Length: > 0 }) mapCount++;
        if (hasNotifClassifications) mapCount++;

        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.HELLO, 1);
        PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, mapCount);

        PackStreamWriter.WriteString(_sendBuf, ref pos, "user_agent");
        PackStreamWriter.WriteString(_sendBuf, ref pos, userAgent);

        // routing - cluster routing context (Bolt 4.1+)
        if (hasRouting)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "routing");
            if (routingContext is { Count: > 0 })
            {
                PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, routingContext.Count);
                foreach (var (key, value) in routingContext)
                {
                    PackStreamWriter.WriteString(_sendBuf, ref pos, key);
                    PackStreamWriter.WriteString(_sendBuf, ref pos, value);
                }
            }
            else
            {
                // Null routing context - server interprets as direct connection
                _sendBuf[pos++] = Markers.NULL;
            }
        }

        // patch_bolt - UTC temporal fix (Bolt 4.3-4.4 only)
        if (hasPatchBolt)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "patch_bolt");
            PackStreamWriter.WriteListHeader(_sendBuf, ref pos, 1);
            PackStreamWriter.WriteString(_sendBuf, ref pos, "utc");
        }

        // bolt_agent - rich client identification (Bolt 5.3+)
        if (hasBoltAgent)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "bolt_agent");
            PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, 3);
            PackStreamWriter.WriteString(_sendBuf, ref pos, "product");
            PackStreamWriter.WriteString(_sendBuf, ref pos, userAgent);
            PackStreamWriter.WriteString(_sendBuf, ref pos, "platform");
            PackStreamWriter.WriteString(_sendBuf, ref pos,
                $"{RuntimeInformation.OSDescription};{RuntimeInformation.OSArchitecture}");
            PackStreamWriter.WriteString(_sendBuf, ref pos, "language_details");
            PackStreamWriter.WriteString(_sendBuf, ref pos, RuntimeInformation.FrameworkDescription);
        }

        // notifications_minimum_severity (Bolt 5.2+)
        if (hasNotifSeverity)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "notifications_minimum_severity");
            PackStreamWriter.WriteString(_sendBuf, ref pos, notifications!.MinimumSeverity!.Value switch
            {
                NotificationSeverity.Off => "OFF",
                NotificationSeverity.Warning => "WARNING",
                NotificationSeverity.Information => "INFORMATION",
                _ => "OFF"
            });
        }

        // Bolt 5.2-5.4: notifications_disabled_categories
        if (hasNotifCategories && notifications?.DisabledCategories is { Length: > 0 } cats52)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "notifications_disabled_categories");
            PackStreamWriter.WriteListHeader(_sendBuf, ref pos, cats52.Length);
            for (int i = 0; i < cats52.Length; i++)
                PackStreamWriter.WriteString(_sendBuf, ref pos, cats52[i].ToString().ToUpperInvariant());
        }

        // Bolt 5.5+: notifications_disabled_classifications (renamed key)
        if (hasNotifClassifications)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "notifications_disabled_classifications");
            var cats55 = notifications!.DisabledCategories!;
            PackStreamWriter.WriteListHeader(_sendBuf, ref pos, cats55.Length);
            for (int i = 0; i < cats55.Length; i++)
                PackStreamWriter.WriteString(_sendBuf, ref pos, cats55[i].ToString().ToUpperInvariant());
        }

        await SendMessageAsync(_sendBuf, pos, ct);
    }

    public async ValueTask SendLogonNoneAsync(CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.LOGON, 1);
        PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, 1);
        PackStreamWriter.WriteString(_sendBuf, ref pos, "scheme");
        PackStreamWriter.WriteString(_sendBuf, ref pos, "none");
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    public async ValueTask SendLogonAsync(string username, string password, CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.LOGON, 1);
        PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, 3);
        PackStreamWriter.WriteString(_sendBuf, ref pos, "scheme");
        PackStreamWriter.WriteString(_sendBuf, ref pos, "basic");
        PackStreamWriter.WriteString(_sendBuf, ref pos, "principal");
        PackStreamWriter.WriteString(_sendBuf, ref pos, username);
        PackStreamWriter.WriteString(_sendBuf, ref pos, "credentials");
        PackStreamWriter.WriteString(_sendBuf, ref pos, password);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    public async ValueTask SendLogoffAsync(CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.LOGOFF, 0);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    public async ValueTask SendLogonAsync(BoltAuthToken token, CancellationToken ct = default)
    {
        int mapCount = 1; // scheme always present
        if (token.Principal is not null) mapCount++;
        if (token.Credentials is not null) mapCount++;
        if (token.Realm is not null) mapCount++;
        if (token.Parameters is { Count: > 0 }) mapCount += token.Parameters.Count;

        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.LOGON, 1);
        PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, mapCount);
        PackStreamWriter.WriteString(_sendBuf, ref pos, "scheme");
        PackStreamWriter.WriteString(_sendBuf, ref pos, token.Scheme);

        if (token.Principal is { } principal)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "principal");
            PackStreamWriter.WriteString(_sendBuf, ref pos, principal);
        }
        if (token.Credentials is { } credentials)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "credentials");
            PackStreamWriter.WriteString(_sendBuf, ref pos, credentials);
        }
        if (token.Realm is { } realm)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "realm");
            PackStreamWriter.WriteString(_sendBuf, ref pos, realm);
        }
        if (token.Parameters is { Count: > 0 } parameters)
        {
            foreach (var (key, value) in parameters)
            {
                PackStreamWriter.WriteString(_sendBuf, ref pos, key);
                PackStreamWriter.WriteValue(_sendBuf, ref pos, value);
            }
        }

        await SendMessageAsync(_sendBuf, pos, ct);
    }

    /// <summary>Send ROUTE message (0x66) for cluster routing table discovery.</summary>
    public async ValueTask SendRouteAsync(
        Dictionary<string, string>? routing,
        IReadOnlyList<string>? bookmarks,
        string? database,
        string? impersonatedUser,
        CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.ROUTE, 3);

        // routing context (map of string->string)
        if (routing is { Count: > 0 })
        {
            PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, routing.Count);
            foreach (var (key, value) in routing)
            {
                PackStreamWriter.WriteString(_sendBuf, ref pos, key);
                PackStreamWriter.WriteString(_sendBuf, ref pos, value);
            }
        }
        else
        {
            PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, 0);
        }

        // bookmarks list
        if (bookmarks is { Count: > 0 })
        {
            PackStreamWriter.WriteListHeader(_sendBuf, ref pos, bookmarks.Count);
            for (int i = 0; i < bookmarks.Count; i++)
                PackStreamWriter.WriteString(_sendBuf, ref pos, bookmarks[i]);
        }
        else
        {
            PackStreamWriter.WriteListHeader(_sendBuf, ref pos, 0);
        }

        // extra dict: {db, imp_user}
        int extraCount = 0;
        if (database is not null) extraCount++;
        if (impersonatedUser is not null) extraCount++;
        PackStreamWriter.WriteMapHeader(_sendBuf, ref pos, extraCount);
        if (database is not null)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "db");
            PackStreamWriter.WriteString(_sendBuf, ref pos, database);
        }
        if (impersonatedUser is not null)
        {
            PackStreamWriter.WriteString(_sendBuf, ref pos, "imp_user");
            PackStreamWriter.WriteString(_sendBuf, ref pos, impersonatedUser);
        }

        await SendMessageAsync(_sendBuf, pos, ct);
    }

    /// <summary>Send TELEMETRY message (0x54) with the query API type.</summary>
    public async ValueTask SendTelemetryAsync(int apiType, CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.TELEMETRY, 1);
        PackStreamWriter.WriteInt64(_sendBuf, ref pos, apiType);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    public async ValueTask SendGoodbyeAsync(CancellationToken ct = default)
    {
        int pos = 0;
        PackStreamWriter.WriteStructHeader(_sendBuf, ref pos, MsgTag.GOODBYE, 0);
        await SendMessageAsync(_sendBuf, pos, ct);
    }

    // -- Helpers -------------------------------------------------------------

    private void RearmQuickAck()
    {
        if (_isLinux)
        {
            try { _socket.SetRawSocketOption(IPPROTO_TCP, TCP_QUICKACK, QuickAckValue); }
            catch { }
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
            return;

        try { _stream.Dispose(); } catch { }
        try { _socket.Dispose(); } catch { }
        ArrayPool<byte>.Shared.Return(_sendBuf);
        ArrayPool<byte>.Shared.Return(_ioBuf);
        ArrayPool<byte>.Shared.Return(_msgBuf);
        ArrayPool<byte>.Shared.Return(_frameBuf);
    }
}
