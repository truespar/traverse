namespace Traverse.Bolt.Internal;

/// <summary>PackStream marker bytes and Bolt message structure tags.</summary>
internal static class Markers
{
    // PackStream types
    public const byte NULL = 0xC0;
    public const byte TRUE = 0xC3;
    public const byte FALSE = 0xC2;
    public const byte INT8 = 0xC8;
    public const byte INT16 = 0xC9;
    public const byte INT32 = 0xCA;
    public const byte INT64 = 0xCB;
    public const byte FLOAT64 = 0xC1;

    // Bytes
    public const byte BYTES8 = 0xCC;
    public const byte BYTES16 = 0xCD;
    public const byte BYTES32 = 0xCE;

    // String
    public const byte TINY_STRING = 0x80; // 0x80..0x8F (+ low nibble = length)
    public const byte STRING8 = 0xD0;
    public const byte STRING16 = 0xD1;
    public const byte STRING32 = 0xD2;

    // List
    public const byte TINY_LIST = 0x90; // 0x90..0x9F
    public const byte LIST8 = 0xD4;
    public const byte LIST16 = 0xD5;
    public const byte LIST32 = 0xD6;

    // Map
    public const byte TINY_MAP = 0xA0; // 0xA0..0xAF
    public const byte MAP8 = 0xD8;
    public const byte MAP16 = 0xD9;
    public const byte MAP32 = 0xDA;

    // Struct
    public const byte TINY_STRUCT = 0xB0; // 0xB0..0xBF
}

/// <summary>Bolt message tags (first byte inside a struct).</summary>
internal static class MsgTag
{
    // Client messages
    public const byte HELLO = 0x01;
    public const byte LOGON = 0x6A;
    public const byte LOGOFF = 0x6B;
    public const byte RUN = 0x10;
    public const byte PULL = 0x3F;
    public const byte DISCARD = 0x2F;
    public const byte RESET = 0x0F;
    public const byte GOODBYE = 0x02;
    public const byte BEGIN = 0x11;
    public const byte COMMIT = 0x12;
    public const byte ROLLBACK = 0x13;
    public const byte ROUTE = 0x66;
    public const byte TELEMETRY = 0x54;

    // Server messages
    public const byte SUCCESS = 0x70;
    public const byte RECORD = 0x71;
    public const byte FAILURE = 0x7F;
    public const byte IGNORED = 0x7E;
}

/// <summary>Graph type structure tags.</summary>
internal static class GraphTag
{
    public const byte NODE = 0x4E;
    public const byte RELATIONSHIP = 0x52;
    public const byte UNBOUND_REL = 0x72;
    public const byte PATH = 0x50;
}

/// <summary>Temporal and spatial structure tags.</summary>
internal static class TypeTag
{
    public const byte DATE = 0x44;
    public const byte LOCAL_TIME = 0x74;
    public const byte TIME = 0x54;
    public const byte LOCAL_DT = 0x64;
    public const byte DT_OFFSET = 0x49;
    public const byte DT_ZONE = 0x69;
    public const byte DURATION = 0x45;
    public const byte POINT2D = 0x58;
    public const byte POINT3D = 0x59;
}
