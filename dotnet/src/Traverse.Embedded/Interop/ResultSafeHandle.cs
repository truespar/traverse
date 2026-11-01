using System.Runtime.InteropServices;

namespace Traverse.Interop;

/// <summary>
/// SafeHandle wrapping a native TvResult pointer.
/// Releases by calling <c>traverse_result_free</c>.
/// </summary>
internal sealed class ResultSafeHandle : SafeHandle
{
    public ResultSafeHandle() : base(IntPtr.Zero, ownsHandle: true) { }

    public override bool IsInvalid => handle == IntPtr.Zero;

    protected override bool ReleaseHandle()
    {
        NativeLibrary.traverse_result_free(handle);
        return true;
    }
}
