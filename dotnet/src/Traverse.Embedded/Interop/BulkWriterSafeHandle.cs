using System.Runtime.InteropServices;

namespace Traverse.Interop;

/// <summary>
/// SafeHandle wrapping a native TvBulkWriter pointer.
/// Releases by calling <c>traverse_bulk_abort</c> (auto-abort on dispose).
/// </summary>
internal sealed class BulkWriterSafeHandle : SafeHandle
{
    public BulkWriterSafeHandle() : base(IntPtr.Zero, ownsHandle: true) { }

    public override bool IsInvalid => handle == IntPtr.Zero;

    protected override bool ReleaseHandle()
    {
        // fails only for a null handle
        _ = NativeLibrary.traverse_bulk_abort(handle);
        return true;
    }
}
