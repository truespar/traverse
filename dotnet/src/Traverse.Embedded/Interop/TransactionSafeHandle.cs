using System.Runtime.InteropServices;

namespace Traverse.Interop;

/// <summary>
/// SafeHandle wrapping a native TvTransaction pointer.
/// Releases by calling <c>traverse_abort</c> (auto-abort on dispose).
/// </summary>
internal sealed class TransactionSafeHandle : SafeHandle
{
    public TransactionSafeHandle() : base(IntPtr.Zero, ownsHandle: true) { }

    public override bool IsInvalid => handle == IntPtr.Zero;

    protected override bool ReleaseHandle()
    {
        // fails only for a null handle
        _ = NativeLibrary.traverse_abort(handle);
        return true;
    }
}
