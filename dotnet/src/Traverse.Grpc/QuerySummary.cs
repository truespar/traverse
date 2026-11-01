using Traverse.Grpc.Values;
using Traverse.V1;

namespace Traverse.Grpc;

/// <summary>
/// Summary metadata from a completed query execution.
/// </summary>
public sealed class QuerySummary
{
    public long NodesCreated { get; init; }
    public long NodesDeleted { get; init; }
    public long RelationshipsCreated { get; init; }
    public long RelationshipsDeleted { get; init; }
    public long PropertiesSet { get; init; }
    public long LabelsAdded { get; init; }
    public long LabelsRemoved { get; init; }
    public long IndexesAdded { get; init; }
    public long IndexesRemoved { get; init; }
    public long ConstraintsAdded { get; init; }
    public long ConstraintsRemoved { get; init; }
    public string QueryType { get; init; } = "";
    public string? Bookmark { get; init; }
    public string? Database { get; init; }
    public double TimeToFirstMs { get; init; }
    public double TimeToLastMs { get; init; }
    public bool ContainsUpdates { get; init; }
    public IReadOnlyList<QueryNotification> Notifications { get; init; } = [];
    public TvValue? Plan { get; init; }

    internal static QuerySummary FromFooter(QueryFooter footer)
    {
        var stats = footer.Stats;
        var notifications = new List<QueryNotification>(footer.Notifications.Count);
        foreach (var n in footer.Notifications)
        {
            notifications.Add(new QueryNotification
            {
                Code = n.Code,
                Title = n.Title,
                Description = n.Description,
                Severity = n.Severity,
            });
        }

        TvValue? plan = footer.Plan is not null ? ValueConverter.FromProto(footer.Plan) : default(TvValue?);

        return new QuerySummary
        {
            NodesCreated = stats?.NodesCreated ?? 0,
            NodesDeleted = stats?.NodesDeleted ?? 0,
            RelationshipsCreated = stats?.RelationshipsCreated ?? 0,
            RelationshipsDeleted = stats?.RelationshipsDeleted ?? 0,
            PropertiesSet = stats?.PropertiesSet ?? 0,
            LabelsAdded = stats?.LabelsAdded ?? 0,
            LabelsRemoved = stats?.LabelsRemoved ?? 0,
            IndexesAdded = stats?.IndexesAdded ?? 0,
            IndexesRemoved = stats?.IndexesRemoved ?? 0,
            ConstraintsAdded = stats?.ConstraintsAdded ?? 0,
            ConstraintsRemoved = stats?.ConstraintsRemoved ?? 0,
            QueryType = footer.QueryType,
            Bookmark = string.IsNullOrEmpty(footer.Bookmark) ? null : footer.Bookmark,
            Database = string.IsNullOrEmpty(footer.Database) ? null : footer.Database,
            TimeToFirstMs = footer.TFirstMs,
            TimeToLastMs = footer.TLastMs,
            ContainsUpdates = footer.ContainsUpdates,
            Notifications = notifications,
            Plan = plan,
        };
    }
}

/// <summary>
/// A diagnostic notification from query execution.
/// </summary>
public sealed class QueryNotification
{
    public string Code { get; init; } = "";
    public string Title { get; init; } = "";
    public string Description { get; init; } = "";
    public string Severity { get; init; } = "";
}
