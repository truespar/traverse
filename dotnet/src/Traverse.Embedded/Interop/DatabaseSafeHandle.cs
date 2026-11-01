using System.Runtime.InteropServices;

namespace Traverse.Interop;

/// <summary>
/// SafeHandle wrapping a native TvDatabase pointer.
/// Releases by calling <c>traverse_close</c>.
/// </summary>
internal sealed class DatabaseSafeHandle : SafeHandle
{
    public DatabaseSafeHandle() : base(IntPtr.Zero, ownsHandle: true) { }

    public override bool IsInvalid => handle == IntPtr.Zero;

    /// <summary>Why the close failed (its save), read by <see cref="Database.Dispose"/>.</summary>
    internal TraverseException? CloseError { get; private set; }

    protected override bool ReleaseHandle()
    {
        if (NativeLibrary.traverse_close(handle) != 0)
            CloseError = NativeLibrary.LastError();
        return true;
    }
}
