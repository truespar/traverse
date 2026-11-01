namespace Traverse.Bolt;

/// <summary>A 2D point with SRID. Represents Bolt Point2D type (0x58).</summary>
public sealed record BoltPoint2D(int Srid, double X, double Y);

/// <summary>A 3D point with SRID. Represents Bolt Point3D type (0x59).</summary>
public sealed record BoltPoint3D(int Srid, double X, double Y, double Z);
