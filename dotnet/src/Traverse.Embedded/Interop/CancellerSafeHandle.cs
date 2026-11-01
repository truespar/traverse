using System.Runtime.InteropServices;

namespace Traverse.Interop;

/// <summary>
/// SafeHandle wrapping a native TvCanceller: stops a transaction's running statement, or one
/// auto-commit statement, from any thread, and stays valid after either ends. Releases with
/// <c>traverse_canceller_free</c>.
/// </summary>
internal sealed class CancellerSafeHandle : SafeHandle
{
    public CancellerSafeHandle() : base(IntPtr.Zero, ownsHandle: true) { }

    /// <summary>A canceller for one auto-commit statement.</summary>
    public static CancellerSafeHandle ForStatement()
    {
        var handle = new CancellerSafeHandle();
        Marshal.InitHandle(handle, NativeLibrary.traverse_canceller_new());
        return handle;
    }

    /// <summary>Stops its statement.</summary>
    public void Cancel()
    {
        bool refAdded = false;
        DangerousAddRef(ref refAdded);
        try
        {
            // fails only for a null handle
            _ = NativeLibrary.traverse_cancel(DangerousGetHandle());
        }
        finally
        {
            if (refAdded) DangerousRelease();
        }
    }

    public override bool IsInvalid => handle == IntPtr.Zero;

    protected override bool ReleaseHandle()
    {
        NativeLibrary.traverse_canceller_free(handle);
        return true;
    }
}
