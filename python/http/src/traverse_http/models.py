from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


# -- Health ----------------------------------------------------------

@dataclass
class HealthDatabase:
    name: str = ""
    default: bool = False
    nodes: int = 0
    edges: int = 0
    memory_bytes: int = 0


@dataclass
class HealthResponse:
    status: str = ""
    version: str = ""
    uptime_secs: int = 0
    memory_bytes: int = 0
    databases: list[HealthDatabase] = field(default_factory=list)
    allow_drop_database: bool = False
    auth_required: bool = False
    #: Compile-time server capabilities, e.g. {"gql": True, "ml": False}.
    #: Check features.get("gql") before passing dialect="gql" to query().
    features: dict[str, bool] = field(default_factory=dict)


# -- Databases -------------------------------------------------------

@dataclass
class DatabaseInfo:
    name: str = ""
    default: bool = False
    read_only: bool = False
    aliases: list[str] = field(default_factory=list)


# -- Files -----------------------------------------------------------

@dataclass
class TvdbFileInfo:
    name: str = ""
    file: str = ""
    bytes: int = 0
    loaded: bool = False
    #: "database" for .tvdb files, "model" for .tvmodel trained models.
    kind: str = "database"


@dataclass
class UploadResult:
    ok: bool = False
    name: str = ""
    file: str = ""
    bytes: int = 0


# -- Aliases ---------------------------------------------------------

@dataclass
class AliasInfo:
    alias: str = ""
    database: str = ""


# -- Schema ----------------------------------------------------------

@dataclass
class SchemaProperty:
    name: str = ""
    type: str = ""


@dataclass
class IndexInfo:
    name: str = ""
    label: str = ""
    property: str = ""
    entries: int = 0


@dataclass
class ConstraintInfo:
    name: str = ""
    type: str = ""
    label: str = ""
    property: str = ""


@dataclass
class LabelDetail:
    name: str = ""
    properties: list[SchemaProperty] = field(default_factory=list)


@dataclass
class SchemaResponse:
    labels: list[str] = field(default_factory=list)
    relationship_types: list[str] = field(default_factory=list)
    property_keys: list[SchemaProperty] = field(default_factory=list)
    indexes: list[IndexInfo] = field(default_factory=list)
    constraints: list[ConstraintInfo] = field(default_factory=list)
    labels_detail: list[LabelDetail] = field(default_factory=list)
    relationship_types_detail: list[LabelDetail] = field(default_factory=list)


# -- Query -----------------------------------------------------------

@dataclass
class QueryStats:
    nodes_created: int = 0
    nodes_deleted: int = 0
    relationships_created: int = 0
    relationships_deleted: int = 0
    properties_set: int = 0
    labels_added: int = 0
    labels_removed: int = 0


@dataclass
class QueryResponse:
    columns: list[str] = field(default_factory=list)
    rows: list[list[Any]] = field(default_factory=list)
    total_rows: int = 0
    truncated: bool = False
    query_type: str = ""
    stats: QueryStats = field(default_factory=QueryStats)
    time_ms: float = 0.0
    parsing_time_ms: float = 0.0
    planning_time_ms: float = 0.0
    execution_time_ms: float = 0.0


# -- Import ----------------------------------------------------------

@dataclass
class ImportError:
    statement: str = ""
    error: str = ""


@dataclass
class ImportResult:
    total_statements: int = 0
    executed: int = 0
    errors: list[ImportError] = field(default_factory=list)
    stats: QueryStats = field(default_factory=QueryStats)
    time_ms: float = 0.0


@dataclass
class CsvImportError:
    row: int = 0
    error: str = ""


@dataclass
class CsvImportResult:
    nodes_created: int = 0
    properties_set: int = 0
    labels_added: int = 0
    errors: list[CsvImportError] = field(default_factory=list)
    time_ms: float = 0.0


@dataclass
class CsvEdgeImportResult:
    edges_created: int = 0
    properties_set: int = 0
    errors: list[CsvImportError] = field(default_factory=list)
    time_ms: float = 0.0


@dataclass
class ColumnMapping:
    name: str
    property: str | None = None
    type: str | None = None

    def to_dict(self) -> dict[str, str]:
        d: dict[str, str] = {"name": self.name}
        if self.property is not None:
            d["property"] = self.property
        if self.type is not None:
            d["type"] = self.type
        return d


@dataclass
class EdgeEndpoint:
    label: str
    column: str
    property: str

    def to_dict(self) -> dict[str, str]:
        return {"label": self.label, "column": self.column, "property": self.property}


# -- Tasks -----------------------------------------------------------

@dataclass
class TaskInfo:
    id: str = ""
    operation: str = ""
    status: str = ""
    result: Any = None
    error: str | None = None
    created_at: str = ""
    finished_at: str | None = None


# -- Users -----------------------------------------------------------

@dataclass
class UserInfo:
    username: str = ""
    role: str = ""
