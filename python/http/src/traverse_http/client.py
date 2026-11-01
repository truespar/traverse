from __future__ import annotations

import time
from typing import Any, BinaryIO
from urllib.parse import quote

import requests

from .exceptions import TraverseHttpException
from .models import (
    AliasInfo,
    ColumnMapping,
    ConstraintInfo,
    CsvEdgeImportResult,
    CsvImportError,
    CsvImportResult,
    DatabaseInfo,
    EdgeEndpoint,
    HealthDatabase,
    HealthResponse,
    ImportError,
    ImportResult,
    IndexInfo,
    LabelDetail,
    QueryResponse,
    QueryStats,
    SchemaProperty,
    SchemaResponse,
    TaskInfo,
    TvdbFileInfo,
    UploadResult,
    UserInfo,
)


class TraverseHttpClient:
    """HTTP client for the Traverse server management API."""

    def __init__(
        self,
        base_url: str,
        *,
        api_key: str | None = None,
        timeout: float = 100.0,
        transfer_timeout: float = 3600.0,
        allow_untrusted_certificate: bool = False,
    ):
        self._base_url = base_url.rstrip("/") + "/"
        self._timeout = timeout
        self._transfer_timeout = transfer_timeout
        self._session = requests.Session()
        if api_key:
            self._session.headers["Authorization"] = f"Bearer {api_key}"
        if allow_untrusted_certificate:
            self._session.verify = False

    def close(self) -> None:
        self._session.close()

    def __enter__(self) -> TraverseHttpClient:
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()

    # -- Health ----------------------------------------------------------

    def health(self) -> HealthResponse:
        data = self._get("api/health")
        return _parse_health(data)

    def ready(self) -> bool:
        try:
            resp = self._session.get(
                self._url("api/ready"), timeout=self._timeout
            )
            return 200 <= resp.status_code < 300
        except Exception:
            return False

    # -- Databases -------------------------------------------------------

    def list_databases(self) -> list[DatabaseInfo]:
        data = self._get("api/databases")
        return [_parse_database_info(d) for d in data]

    def create_database(self, name: str) -> None:
        self._post("api/databases", {"name": name})

    def drop_database(self, name: str) -> None:
        self._poll_task(self._delete(f"api/databases/{_encode(name)}?async=true"))

    def rename_database(self, old_name: str, new_name: str) -> None:
        self._post(f"api/databases/{_encode(old_name)}/rename", {"name": new_name})

    def unload_database(self, name: str) -> None:
        self._poll_task(
            self._post_raw(
                f"api/databases/{_encode(name)}/unload?async=true",
                timeout=self._transfer_timeout,
            )
        )

    def load_database(self, name: str, file: str) -> None:
        self._poll_task(
            self._post_raw(
                "api/databases/load?async=true",
                json={"name": name, "file": file},
                timeout=self._transfer_timeout,
            )
        )

    # -- Files -----------------------------------------------------------

    def list_files(self) -> list[TvdbFileInfo]:
        data = self._get("api/files")
        # Parse field-by-field: the server may add entry fields (as it
        # did with `kind`) and TvdbFileInfo(**d) would raise on them.
        return [
            TvdbFileInfo(
                name=d.get("name", ""),
                file=d.get("file", ""),
                bytes=d.get("bytes", 0),
                loaded=d.get("loaded", False),
                kind=d.get("kind", "database"),
            )
            for d in data
        ]

    def upload_file(
        self, name: str, data: bytes | BinaryIO, kind: str = "database"
    ) -> UploadResult:
        """Upload a .tvdb database (kind="database", the default) or a
        .tvmodel trained model (kind="model" - requires a server built
        with the ml feature)."""
        if hasattr(data, "read"):
            data = data.read()
        resp = self._session.put(
            self._url(f"api/files/{_encode(name)}") + _kind_query(kind),
            data=data,
            headers={"Content-Type": "application/octet-stream"},
            timeout=self._transfer_timeout,
        )
        _ensure_success(resp)
        return UploadResult(**resp.json())

    def download_file(self, name: str, kind: str = "database") -> bytes:
        resp = self._session.get(
            self._url(f"api/files/{_encode(name)}") + _kind_query(kind),
            timeout=self._transfer_timeout,
        )
        _ensure_success(resp)
        return resp.content

    def delete_file(self, name: str, kind: str = "database") -> None:
        self._delete(f"api/files/{_encode(name)}" + _kind_query(kind))

    # -- Aliases ---------------------------------------------------------

    def list_aliases(self) -> list[AliasInfo]:
        data = self._get("api/aliases")
        return [AliasInfo(**d) for d in data]

    def create_alias(self, alias: str, database: str) -> None:
        self._post("api/aliases", {"alias": alias, "database": database})

    def alter_alias(self, alias: str, database: str) -> None:
        self._put(f"api/aliases/{_encode(alias)}", {"database": database})

    def drop_alias(self, alias: str) -> None:
        self._delete(f"api/aliases/{_encode(alias)}")

    # -- Schema ----------------------------------------------------------

    def get_schema(self, database: str | None = None) -> SchemaResponse:
        path = "api/schema"
        if database is not None:
            path += f"?database={_encode(database)}"
        data = self._get(path)
        return _parse_schema(data)

    # -- Query -----------------------------------------------------------

    def query(
        self,
        cypher: str,
        parameters: dict[str, Any] | None = None,
        database: str | None = None,
        dialect: str | None = None,
    ) -> QueryResponse:
        """Run a query. ``dialect`` selects the query language: None
        (openCypher, the default) or "gql" for ISO/IEC 39075 GQL -
        requires a server built with the gql feature; check
        ``health().features.get("gql")``."""
        body: dict[str, Any] = {"query": cypher}
        if parameters is not None:
            body["parameters"] = parameters
        if database is not None:
            body["database"] = database
        if dialect is not None:
            body["dialect"] = dialect
        data = self._post("api/query", body)
        return _parse_query_response(data)

    # -- Import ----------------------------------------------------------

    def import_cypher(
        self, cypher: str, database: str | None = None
    ) -> ImportResult:
        body: dict[str, str] = {"cypher": cypher}
        if database is not None:
            body["database"] = database
        data = self._post("api/import", body)
        return _parse_import_result(data)

    def import_csv(
        self,
        csv: str,
        label: str,
        columns: list[ColumnMapping],
        database: str | None = None,
        delimiter: str | None = None,
        skip_header: bool | None = None,
    ) -> CsvImportResult:
        body: dict[str, Any] = {
            "csv": csv,
            "label": label,
            "columns": [c.to_dict() for c in columns],
        }
        if database is not None:
            body["database"] = database
        if delimiter is not None:
            body["delimiter"] = delimiter
        if skip_header is not None:
            body["skip_header"] = skip_header
        data = self._post("api/import/csv", body)
        return _parse_csv_import_result(data)

    def import_csv_edges(
        self,
        csv: str,
        edge_type: str,
        source: EdgeEndpoint,
        target: EdgeEndpoint,
        columns: list[ColumnMapping] | None = None,
        database: str | None = None,
        delimiter: str | None = None,
        skip_header: bool | None = None,
    ) -> CsvEdgeImportResult:
        body: dict[str, Any] = {
            "csv": csv,
            "edge_type": edge_type,
            "source": source.to_dict(),
            "target": target.to_dict(),
        }
        if columns is not None:
            body["columns"] = [c.to_dict() for c in columns]
        if database is not None:
            body["database"] = database
        if delimiter is not None:
            body["delimiter"] = delimiter
        if skip_header is not None:
            body["skip_header"] = skip_header
        data = self._post("api/import/csv/edges", body)
        return _parse_csv_edge_import_result(data)

    # -- Users -----------------------------------------------------------

    def list_users(self) -> list[UserInfo]:
        data = self._get("api/users")
        return [UserInfo(**d) for d in data]

    def create_user(self, username: str, password: str, role: str) -> None:
        self._post(
            "api/users",
            {"username": username, "password": password, "role": role},
        )

    def update_user(
        self,
        username: str,
        password: str | None = None,
        role: str | None = None,
    ) -> None:
        body: dict[str, str] = {}
        if password is not None:
            body["password"] = password
        if role is not None:
            body["role"] = role
        self._put(f"api/users/{_encode(username)}", body)

    def delete_user(self, username: str) -> None:
        self._delete(f"api/users/{_encode(username)}")

    # -- Server operations -----------------------------------------------

    def save(self) -> None:
        self._poll_task(
            self._post_raw("api/save?async=true", timeout=self._transfer_timeout)
        )

    # -- Tasks -----------------------------------------------------------

    def get_task(self, task_id: str) -> TaskInfo:
        data = self._get(f"api/tasks/{_encode(task_id)}")
        return _parse_task_info(data)

    # -- Internal HTTP helpers -------------------------------------------

    def _url(self, path: str) -> str:
        return self._base_url + path

    def _get(self, path: str) -> Any:
        resp = self._session.get(self._url(path), timeout=self._timeout)
        _ensure_success(resp)
        return resp.json()

    def _post(self, path: str, body: Any = None) -> Any:
        resp = self._session.post(
            self._url(path), json=body, timeout=self._timeout
        )
        _ensure_success(resp)
        text = resp.text
        if text:
            return resp.json()
        return None

    def _post_raw(
        self,
        path: str,
        json: Any = None,
        timeout: float | None = None,
    ) -> str:
        resp = self._session.post(
            self._url(path), json=json, timeout=timeout or self._timeout
        )
        _ensure_success(resp)
        return resp.text

    def _put(self, path: str, body: Any) -> None:
        resp = self._session.put(
            self._url(path), json=body, timeout=self._timeout
        )
        _ensure_success(resp)

    def _delete(self, path: str) -> str:
        resp = self._session.delete(self._url(path), timeout=self._timeout)
        _ensure_success(resp)
        return resp.text

    def _poll_task(self, response_body: str) -> None:
        if not response_body or not response_body.strip():
            return
        import json as _json

        try:
            data = _json.loads(response_body)
        except ValueError:
            return
        task_id = data.get("taskId")
        if not task_id:
            return
        delay = 0.25
        while True:
            time.sleep(delay)
            task = self.get_task(task_id)
            if task.status == "completed":
                return
            if task.status == "failed":
                raise TraverseHttpException(task.error or "task failed")
            delay = min(delay * 2, 2.0)


# -- Helpers ---------------------------------------------------------


def _encode(s: str) -> str:
    return quote(s, safe="")


def _kind_query(kind: str) -> str:
    """Query-string suffix for /api/files kind routing. The default
    "database" is omitted so requests stay byte-identical to
    pre-`kind` clients."""
    return "" if kind == "database" else f"?kind={_encode(kind)}"


def _ensure_success(resp: requests.Response) -> None:
    if 200 <= resp.status_code < 300:
        return
    error_message = None
    try:
        data = resp.json()
        error_message = data.get("error")
    except Exception:
        pass
    raise TraverseHttpException(
        error_message or f"HTTP {resp.status_code}",
        status_code=resp.status_code,
    )


# -- Model parsers ---------------------------------------------------


def _parse_health(d: dict) -> HealthResponse:
    return HealthResponse(
        status=d.get("status", ""),
        version=d.get("version", ""),
        uptime_secs=d.get("uptime_secs", 0),
        memory_bytes=d.get("memory_bytes", 0),
        databases=[HealthDatabase(**db) for db in d.get("databases", [])],
        allow_drop_database=d.get("allow_drop_database", False),
        auth_required=d.get("auth_required", False),
        features={
            k: bool(v) for k, v in (d.get("features") or {}).items()
        },
    )


def _parse_database_info(d: dict) -> DatabaseInfo:
    return DatabaseInfo(
        name=d.get("name", ""),
        default=d.get("default", False),
        read_only=d.get("readOnly", False),
        aliases=d.get("aliases", []),
    )


def _parse_schema(d: dict) -> SchemaResponse:
    return SchemaResponse(
        labels=d.get("labels", []),
        relationship_types=d.get("relationship_types", []),
        property_keys=[SchemaProperty(**p) for p in d.get("property_keys", [])],
        indexes=[IndexInfo(**i) for i in d.get("indexes", [])],
        constraints=[ConstraintInfo(**c) for c in d.get("constraints", [])],
        labels_detail=[_parse_label_detail(ld) for ld in d.get("labels_detail", [])],
        relationship_types_detail=[
            _parse_label_detail(ld) for ld in d.get("relationship_types_detail", [])
        ],
    )


def _parse_label_detail(d: dict) -> LabelDetail:
    return LabelDetail(
        name=d.get("name", ""),
        properties=[SchemaProperty(**p) for p in d.get("properties", [])],
    )


def _parse_query_response(d: dict) -> QueryResponse:
    return QueryResponse(
        columns=d.get("columns", []),
        rows=d.get("rows", []),
        total_rows=d.get("total_rows", 0),
        truncated=d.get("truncated", False),
        query_type=d.get("query_type", ""),
        stats=QueryStats(**d["stats"]) if "stats" in d else QueryStats(),
        time_ms=d.get("time_ms", 0.0),
        parsing_time_ms=d.get("parsing_time_ms", 0.0),
        planning_time_ms=d.get("planning_time_ms", 0.0),
        execution_time_ms=d.get("execution_time_ms", 0.0),
    )


def _parse_import_result(d: dict) -> ImportResult:
    return ImportResult(
        total_statements=d.get("total_statements", 0),
        executed=d.get("executed", 0),
        errors=[ImportError(**e) for e in d.get("errors", [])],
        stats=QueryStats(**d["stats"]) if "stats" in d else QueryStats(),
        time_ms=d.get("time_ms", 0.0),
    )


def _parse_csv_import_result(d: dict) -> CsvImportResult:
    return CsvImportResult(
        nodes_created=d.get("nodes_created", 0),
        properties_set=d.get("properties_set", 0),
        labels_added=d.get("labels_added", 0),
        errors=[CsvImportError(**e) for e in d.get("errors", [])],
        time_ms=d.get("time_ms", 0.0),
    )


def _parse_csv_edge_import_result(d: dict) -> CsvEdgeImportResult:
    return CsvEdgeImportResult(
        edges_created=d.get("edges_created", 0),
        properties_set=d.get("properties_set", 0),
        errors=[CsvImportError(**e) for e in d.get("errors", [])],
        time_ms=d.get("time_ms", 0.0),
    )


def _parse_task_info(d: dict) -> TaskInfo:
    return TaskInfo(
        id=d.get("id", ""),
        operation=d.get("operation", ""),
        status=d.get("status", ""),
        result=d.get("result"),
        error=d.get("error"),
        created_at=d.get("createdAt", ""),
        finished_at=d.get("finishedAt"),
    )
