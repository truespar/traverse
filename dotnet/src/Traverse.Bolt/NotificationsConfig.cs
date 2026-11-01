namespace Traverse.Bolt;

/// <summary>Minimum severity filter for server notifications.</summary>
public enum NotificationSeverity { Off, Warning, Information }

/// <summary>Notification categories that can be selectively disabled.</summary>
public enum NotificationCategory
{
    Hint, Unrecognized, Unsupported, Performance,
    Deprecation, Security, Topology, Schema, Generic
}

/// <summary>Controls which server notifications are returned with query results.</summary>
public sealed class NotificationsConfig
{
    /// <summary>Minimum severity level. Notifications below this are suppressed.</summary>
    public NotificationSeverity? MinimumSeverity { get; init; }

    /// <summary>Categories to disable entirely.</summary>
    public NotificationCategory[]? DisabledCategories { get; init; }
}
