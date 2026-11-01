using System.Buffers;
using Traverse.Grpc.Values;

namespace Traverse.Grpc.Internal;

/// <summary>
/// ArrayPool wrapper for reusable TvValue row buffers during streaming.
/// </summary>
internal sealed class RowBufferPool
{
    private readonly ArrayPool<TvValue> _pool = ArrayPool<TvValue>.Shared;

    public TvValue[] Rent(int columnCount) => _pool.Rent(columnCount);

    public void Return(TvValue[] buffer)
    {
        // Clear the buffer to avoid holding references
        Array.Clear(buffer);
        _pool.Return(buffer);
    }
}
