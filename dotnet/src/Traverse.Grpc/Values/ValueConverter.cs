using Traverse.V1;

namespace Traverse.Grpc.Values;

/// <summary>
/// Converts between protobuf <see cref="Value"/> and <see cref="TvValue"/>.
/// </summary>
internal static class ValueConverter
{
    public static TvValue FromProto(Value proto)
    {
        return proto.KindCase switch
        {
            Value.KindOneofCase.NullValue => TvValue.Null,
            Value.KindOneofCase.BoolValue => TvValue.FromBool(proto.BoolValue),
            Value.KindOneofCase.IntValue => TvValue.FromInt64(proto.IntValue),
            Value.KindOneofCase.DoubleValue => TvValue.FromDouble(proto.DoubleValue),
            Value.KindOneofCase.StringValue => TvValue.FromString(proto.StringValue),
            Value.KindOneofCase.BytesValue => TvValue.FromBytes(proto.BytesValue.ToByteArray()),
            Value.KindOneofCase.TemporalValue => TvValue.FromTemporal(proto.TemporalValue),
            Value.KindOneofCase.ListValue => TvValue.FromList(ConvertList(proto.ListValue)),
            Value.KindOneofCase.MapValue => TvValue.FromMap(ConvertMap(proto.MapValue)),
            Value.KindOneofCase.NodeValue => TvValue.FromNode(ConvertNode(proto.NodeValue)),
            Value.KindOneofCase.EdgeValue => TvValue.FromEdge(ConvertEdge(proto.EdgeValue)),
            Value.KindOneofCase.PathValue => TvValue.FromPath(ConvertPath(proto.PathValue)),
            Value.KindOneofCase.None => TvValue.Null,
            _ => TvValue.Null,
        };
    }

    public static Value ToProto(TvValue value)
    {
        var proto = new Value();
        switch (value.Kind)
        {
            case TvValueKind.Null:
                proto.NullValue = true;
                break;
            case TvValueKind.Bool:
                proto.BoolValue = value.AsBool();
                break;
            case TvValueKind.Int64:
                proto.IntValue = value.AsInt64();
                break;
            case TvValueKind.Double:
                proto.DoubleValue = value.AsDouble();
                break;
            case TvValueKind.String:
                proto.StringValue = value.AsString();
                break;
            case TvValueKind.Bytes:
                proto.BytesValue = Google.Protobuf.ByteString.CopyFrom(value.AsBytes());
                break;
            case TvValueKind.Temporal:
                proto.TemporalValue = value.AsTemporal();
                break;
            case TvValueKind.List:
                var list = new ListValue();
                foreach (var item in value.AsList())
                    list.Values.Add(ToProto(item));
                proto.ListValue = list;
                break;
            case TvValueKind.Map:
                var map = new MapValue();
                foreach (var (k, v) in value.AsMap())
                    map.Entries.Add(k, ToProto(v));
                proto.MapValue = map;
                break;
            case TvValueKind.Node:
                proto.NodeValue = NodeToProto(value.AsNode());
                break;
            case TvValueKind.Edge:
                proto.EdgeValue = EdgeToProto(value.AsEdge());
                break;
            case TvValueKind.Path:
                var path = value.AsPath();
                var pv = new PathValue();
                foreach (var n in path.Nodes) pv.Nodes.Add(NodeToProto(n));
                foreach (var e in path.Edges) pv.Edges.Add(EdgeToProto(e));
                proto.PathValue = pv;
                break;
        }
        return proto;
    }

    internal static TvNode ConvertNode(NodeValue nv)
    {
        var props = new Dictionary<string, TvValue>(nv.Properties.Count);
        foreach (var (k, v) in nv.Properties)
            props[k] = FromProto(v);
        return new TvNode(nv.Id, nv.Labels.ToList().AsReadOnly(), props);
    }

    internal static TvEdge ConvertEdge(EdgeValue ev)
    {
        var props = new Dictionary<string, TvValue>(ev.Properties.Count);
        foreach (var (k, v) in ev.Properties)
            props[k] = FromProto(v);
        return new TvEdge(ev.Id, ev.EdgeType, ev.Source, ev.Target, props);
    }

    private static TvPath ConvertPath(PathValue pv)
    {
        var nodes = new List<TvNode>(pv.Nodes.Count);
        foreach (var n in pv.Nodes) nodes.Add(ConvertNode(n));
        var edges = new List<TvEdge>(pv.Edges.Count);
        foreach (var e in pv.Edges) edges.Add(ConvertEdge(e));
        return new TvPath(nodes.AsReadOnly(), edges.AsReadOnly());
    }

    private static TvValue[] ConvertList(ListValue lv)
    {
        var list = new TvValue[lv.Values.Count];
        for (int i = 0; i < lv.Values.Count; i++)
            list[i] = FromProto(lv.Values[i]);
        return list;
    }

    private static Dictionary<string, TvValue> ConvertMap(MapValue mv)
    {
        var dict = new Dictionary<string, TvValue>(mv.Entries.Count);
        foreach (var (k, v) in mv.Entries)
            dict[k] = FromProto(v);
        return dict;
    }

    internal static NodeValue NodeToProto(TvNode node)
    {
        var nv = new NodeValue { Id = node.Id };
        nv.Labels.AddRange(node.Labels);
        foreach (var (k, v) in node.Properties)
            nv.Properties.Add(k, ToProto(v));
        return nv;
    }

    internal static EdgeValue EdgeToProto(TvEdge edge)
    {
        var ev = new EdgeValue
        {
            Id = edge.Id,
            EdgeType = edge.Type,
            Source = edge.Source,
            Target = edge.Target,
        };
        foreach (var (k, v) in edge.Properties)
            ev.Properties.Add(k, ToProto(v));
        return ev;
    }

    internal static Google.Protobuf.Collections.MapField<string, Value> ToProtoMap(
        IReadOnlyDictionary<string, TvValue>? properties)
    {
        var map = new Google.Protobuf.Collections.MapField<string, Value>();
        if (properties is not null)
        {
            foreach (var (k, v) in properties)
                map.Add(k, ToProto(v));
        }
        return map;
    }
}
