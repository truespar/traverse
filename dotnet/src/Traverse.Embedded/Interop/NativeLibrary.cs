using System.Reflection;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.Marshalling;

namespace Traverse.Interop;

/// <summary>
/// Hand-crafted P/Invoke declarations for the Traverse native library.
/// Uses <see cref="LibraryImportAttribute"/> (source-generated) for modern interop.
/// </summary>
internal static partial class NativeLibrary
{
    private const string LibName = "traverse_ffi";

    /// <summary>Env var naming the native library file; overrides the bundled one.</summary>
    internal const string LibraryPathVariable = "TRAVERSE_FFI_LIB";

    // runs before the first native call
    static NativeLibrary()
    {
        System.Runtime.InteropServices.NativeLibrary.SetDllImportResolver(
            typeof(NativeLibrary).Assembly, Resolve);
    }

    /// <summary>
    /// <c>TRAVERSE_FFI_LIB</c> when set (a library built from source, a platform the package
    /// doesn't bundle), else the default probing: <c>runtimes/&lt;rid&gt;/native</c>, the app
    /// directory, the system library path.
    /// </summary>
    private static IntPtr Resolve(string name, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (name != LibName)
            return IntPtr.Zero;
        var path = Environment.GetEnvironmentVariable(LibraryPathVariable);
        return string.IsNullOrEmpty(path)
            ? IntPtr.Zero
            : System.Runtime.InteropServices.NativeLibrary.Load(path);
    }

    // -----------------------------------------------------------------------
    // Database
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_open", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr traverse_open(string path);

    [LibraryImport(LibName, EntryPoint = "traverse_flush")]
    internal static partial int traverse_flush(IntPtr db);

    [LibraryImport(LibName, EntryPoint = "traverse_set_busy_timeout")]
    internal static partial int traverse_set_busy_timeout(IntPtr db, long timeoutMs);

    [LibraryImport(LibName, EntryPoint = "traverse_set_statement_timeout")]
    internal static partial int traverse_set_statement_timeout(IntPtr db, long timeoutMs);

    [LibraryImport(LibName, EntryPoint = "traverse_close")]
    internal static partial int traverse_close(IntPtr db);

    // -----------------------------------------------------------------------
    // Bulk writer
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_begin")]
    internal static partial IntPtr traverse_bulk_begin(IntPtr db);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_create_node")]
    internal static partial long traverse_bulk_create_node(
        IntPtr writer,
        [MarshalAs(UnmanagedType.LPArray, ArraySubType = UnmanagedType.LPUTF8Str)] string[] labels,
        int labelCount,
        [MarshalAs(UnmanagedType.LPArray, ArraySubType = UnmanagedType.LPUTF8Str)] string[] propKeys,
        [MarshalAs(UnmanagedType.LPArray, ArraySubType = UnmanagedType.LPUTF8Str)] string[] propValues,
        int propCount);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_create_edge")]
    internal static partial long traverse_bulk_create_edge(
        IntPtr writer,
        long source, long target,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string edgeType,
        [MarshalAs(UnmanagedType.LPArray, ArraySubType = UnmanagedType.LPUTF8Str)] string[] propKeys,
        [MarshalAs(UnmanagedType.LPArray, ArraySubType = UnmanagedType.LPUTF8Str)] string[] propValues,
        int propCount);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_create_index", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int traverse_bulk_create_index(
        IntPtr writer,
        string name,
        string label,
        string property);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_create_edge_index", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int traverse_bulk_create_edge_index(
        IntPtr writer,
        string name,
        string edgeType,
        string? property);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_drop_edge_index", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int traverse_bulk_drop_edge_index(
        IntPtr writer,
        string name);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_commit")]
    internal static partial int traverse_bulk_commit(IntPtr writer);

    [LibraryImport(LibName, EntryPoint = "traverse_bulk_abort")]
    internal static partial int traverse_bulk_abort(IntPtr writer);

    // -----------------------------------------------------------------------
    // Transaction
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_begin")]
    internal static partial IntPtr traverse_begin(IntPtr db);

    [LibraryImport(LibName, EntryPoint = "traverse_begin_read")]
    internal static partial IntPtr traverse_begin_read(IntPtr db);

    [LibraryImport(LibName, EntryPoint = "traverse_commit")]
    internal static partial int traverse_commit(IntPtr tx);

    [LibraryImport(LibName, EntryPoint = "traverse_abort")]
    internal static partial int traverse_abort(IntPtr tx);

    [LibraryImport(LibName, EntryPoint = "traverse_canceller")]
    internal static partial IntPtr traverse_canceller(IntPtr tx);

    [LibraryImport(LibName, EntryPoint = "traverse_canceller_new")]
    internal static partial IntPtr traverse_canceller_new();

    [LibraryImport(LibName, EntryPoint = "traverse_cancel")]
    internal static partial int traverse_cancel(IntPtr canceller);

    [LibraryImport(LibName, EntryPoint = "traverse_canceller_free")]
    internal static partial void traverse_canceller_free(IntPtr canceller);

    // -----------------------------------------------------------------------
    // Query
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_query", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr traverse_query(IntPtr tx, string query);

    [LibraryImport(LibName, EntryPoint = "traverse_query_gql", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr traverse_query_gql(IntPtr tx, string query);

    [LibraryImport(LibName, EntryPoint = "traverse_execute", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr traverse_execute(IntPtr db, string query, IntPtr canceller);

    [LibraryImport(LibName, EntryPoint = "traverse_execute_gql", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr traverse_execute_gql(IntPtr db, string query, IntPtr canceller);

    // -----------------------------------------------------------------------
    // Result - metadata
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_result_column_count")]
    internal static partial int traverse_result_column_count(IntPtr result);

    [LibraryImport(LibName, EntryPoint = "traverse_result_column_name")]
    internal static partial IntPtr traverse_result_column_name(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_row_count")]
    internal static partial long traverse_result_row_count(IntPtr result);

    [LibraryImport(LibName, EntryPoint = "traverse_result_next")]
    [return: MarshalAs(UnmanagedType.U1)]
    internal static partial bool traverse_result_next(IntPtr result);

    // -----------------------------------------------------------------------
    // Result - scalar value getters
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_string")]
    internal static partial IntPtr traverse_result_get_string(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_int")]
    internal static partial long traverse_result_get_int(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_double")]
    internal static partial double traverse_result_get_double(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_bool")]
    [return: MarshalAs(UnmanagedType.U1)]
    internal static partial bool traverse_result_get_bool(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_is_null")]
    [return: MarshalAs(UnmanagedType.U1)]
    internal static partial bool traverse_result_is_null(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_type")]
    internal static partial IntPtr traverse_result_get_type(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_free")]
    internal static partial void traverse_result_free(IntPtr result);

    // -----------------------------------------------------------------------
    // Result - graph accessors (node)
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_node_id")]
    internal static partial long traverse_result_get_node_id(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_node_label_count")]
    internal static partial int traverse_result_get_node_label_count(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_node_label")]
    internal static partial IntPtr traverse_result_get_node_label(IntPtr result, int col, int labelIdx);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_node_prop_count")]
    internal static partial int traverse_result_get_node_prop_count(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_node_prop_key")]
    internal static partial IntPtr traverse_result_get_node_prop_key(IntPtr result, int col, int propIdx);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_node_prop_value")]
    internal static partial IntPtr traverse_result_get_node_prop_value(IntPtr result, int col, int propIdx);

    // -----------------------------------------------------------------------
    // Result - graph accessors (edge)
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_edge_id")]
    internal static partial long traverse_result_get_edge_id(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_edge_type_name")]
    internal static partial IntPtr traverse_result_get_edge_type_name(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_edge_source")]
    internal static partial long traverse_result_get_edge_source(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_edge_target")]
    internal static partial long traverse_result_get_edge_target(IntPtr result, int col);

    // -----------------------------------------------------------------------
    // Result - graph accessors (path)
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_path_length")]
    internal static partial int traverse_result_get_path_length(IntPtr result, int col);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_path_node_id")]
    internal static partial long traverse_result_get_path_node_id(IntPtr result, int col, int idx);

    [LibraryImport(LibName, EntryPoint = "traverse_result_get_path_edge_id")]
    internal static partial long traverse_result_get_path_edge_id(IntPtr result, int col, int idx);

    // -----------------------------------------------------------------------
    // Error
    // -----------------------------------------------------------------------

    [LibraryImport(LibName, EntryPoint = "traverse_last_error")]
    internal static partial IntPtr traverse_last_error();

    [LibraryImport(LibName, EntryPoint = "traverse_last_error_code")]
    internal static partial int traverse_last_error_code();

    [LibraryImport(LibName, EntryPoint = "traverse_last_error_gql_status")]
    internal static partial IntPtr traverse_last_error_gql_status();

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// <summary>
    /// The native error state (this thread's last error) as a <see cref="TraverseException"/>.
    /// </summary>
    internal static TraverseException LastError()
    {
        var code = (TvErrorCode)traverse_last_error_code();
        var msgPtr = traverse_last_error();
        var message = msgPtr != IntPtr.Zero
            ? Marshal.PtrToStringUTF8(msgPtr) ?? "unknown error"
            : "unknown error";
        var statusPtr = traverse_last_error_gql_status();
        var gqlStatus = statusPtr != IntPtr.Zero ? Marshal.PtrToStringUTF8(statusPtr) : null;
        return new TraverseException(code, message, gqlStatus);
    }

    /// <summary>
    /// Read the native error state and throw a <see cref="TraverseException"/>
    /// with the error code and message.
    /// </summary>
    internal static void ThrowLastError() => throw LastError();

    /// <summary>
    /// Check the native error state and throw if an error is set.
    /// </summary>
    internal static void CheckError()
    {
        var code = (TvErrorCode)traverse_last_error_code();
        if (code != TvErrorCode.Ok)
            ThrowLastError();
    }

    /// <summary>
    /// Check a return code and throw if it indicates an error.
    /// </summary>
    internal static void CheckReturnCode(int rc)
    {
        if (rc != 0)
            ThrowLastError();
    }
}
