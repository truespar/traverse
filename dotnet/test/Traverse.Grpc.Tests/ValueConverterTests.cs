using Traverse.Grpc.Values;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class ValueConverterTests
{
    [Fact]
    public void TvValue_Bool_Roundtrip()
    {
        TvValue v = true;
        Assert.Equal(TvValueKind.Bool, v.Kind);
        Assert.True(v.AsBool());
    }

    [Fact]
    public void TvValue_Int64_Roundtrip()
    {
        TvValue v = 42L;
        Assert.Equal(TvValueKind.Int64, v.Kind);
        Assert.Equal(42L, v.AsInt64());
    }

    [Fact]
    public void TvValue_Double_Roundtrip()
    {
        TvValue v = 3.14;
        Assert.Equal(TvValueKind.Double, v.Kind);
        Assert.Equal(3.14, v.AsDouble(), precision: 10);
    }

    [Fact]
    public void TvValue_String_Roundtrip()
    {
        TvValue v = "hello";
        Assert.Equal(TvValueKind.String, v.Kind);
        Assert.Equal("hello", v.AsString());
    }

    [Fact]
    public void TvValue_Null_IsNull()
    {
        var v = TvValue.Null;
        Assert.Equal(TvValueKind.Null, v.Kind);
        Assert.True(v.IsNull);
    }

    [Fact]
    public void TvValue_InvalidCast_Throws()
    {
        TvValue v = 42L;
        Assert.Throws<InvalidCastException>(() => v.AsBool());
        Assert.Throws<InvalidCastException>(() => v.AsString());
        Assert.Throws<InvalidCastException>(() => v.AsDouble());
    }

    [Fact]
    public void TvValue_List_Roundtrip()
    {
        var list = new TvValue[] { 1L, 2L, 3L };
        var v = TvValue.FromList(list);
        Assert.Equal(TvValueKind.List, v.Kind);
        var result = v.AsList();
        Assert.Equal(3, result.Count);
        Assert.Equal(2L, result[1].AsInt64());
    }

    [Fact]
    public void TvValue_Map_Roundtrip()
    {
        var map = new Dictionary<string, TvValue> { ["a"] = 1L, ["b"] = "two" };
        var v = TvValue.FromMap(map);
        Assert.Equal(TvValueKind.Map, v.Kind);
        var result = v.AsMap();
        Assert.Equal(1L, result["a"].AsInt64());
        Assert.Equal("two", result["b"].AsString());
    }

    [Fact]
    public void TvValue_Node_Roundtrip()
    {
        var node = new TvNode(1, ["Person"], new Dictionary<string, TvValue> { ["name"] = "Alice" });
        var v = TvValue.FromNode(node);
        Assert.Equal(TvValueKind.Node, v.Kind);
        var result = v.AsNode();
        Assert.Equal(1UL, result.Id);
        Assert.Equal("Person", result.Labels[0]);
    }

    [Fact]
    public void TvValue_Edge_Roundtrip()
    {
        var edge = new TvEdge(5, "KNOWS", 1, 2, new Dictionary<string, TvValue>());
        var v = TvValue.FromEdge(edge);
        Assert.Equal(TvValueKind.Edge, v.Kind);
        var result = v.AsEdge();
        Assert.Equal("KNOWS", result.Type);
    }

    [Fact]
    public void TvValue_AsObject_ReturnsCorrectTypes()
    {
        Assert.Null(TvValue.Null.AsObject());
        Assert.Equal(true, TvValue.FromBool(true).AsObject());
        Assert.Equal(42L, TvValue.FromInt64(42).AsObject());
        Assert.Equal(3.14, TvValue.FromDouble(3.14).AsObject());
        Assert.Equal("hello", TvValue.FromString("hello").AsObject());
    }

    [Fact]
    public void TvValue_ToString_FormatsCorrectly()
    {
        Assert.Equal("null", TvValue.Null.ToString());
        Assert.Equal("true", TvValue.FromBool(true).ToString());
        Assert.Equal("42", TvValue.FromInt64(42).ToString());
        Assert.Equal("hello", TvValue.FromString("hello").ToString());
    }
}
