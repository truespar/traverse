//! `/api/files`: uploads (databases and models), downloads, listing and deletion.

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde_json::{Value, json};

use super::{AppState, DB_LIFECYCLE_BUSY};

// -- PUT /api/files/{name} -------------------------------------------
// Streams a .tvdb file from the request body straight to disk without loading it (POST
// /api/databases/load does that). Rejected if a .tvdb with that name exists, no data directory is
// configured, or the file lacks the TVDB magic bytes.

pub(super) async fn files_upload(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
    body: axum::body::Body,
) -> impl IntoResponse {
    // `?kind=model` uploads a `.tvmodel` instead of a `.tvdb`; one endpoint keeps Studio's Files
    // panel to one upload UI
    let kind = query.get("kind").map(|s| s.as_str()).unwrap_or("database");
    if kind == "model" {
        return files_upload_model(state, name, body).await.into_response();
    }
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "database name is required"})),
        )
            .into_response();
    }

    let has_data_dir = state.registry.data_dir().is_some();

    if has_data_dir {
        let data_dir = state.registry.data_dir().unwrap().to_path_buf();

        // reject if a .tvdb with this name exists on disk
        let final_path = data_dir.join(format!("{}.tvdb", name));
        if final_path.exists() {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({"error": format!("file '{}.tvdb' already exists - drop it first", name)})),
            ).into_response();
        }

        // stream straight to the final path; a failed stream removes the partial file
        tracing::info!("upload starting: '{}.tvdb'", name);
        let result = stream_to_file(&final_path, body).await;

        let total_bytes = match result {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&final_path);
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
            }
        };

        tracing::info!(
            "uploaded '{}.tvdb' ({} bytes) - use POST /api/databases/load to load it",
            name,
            total_bytes
        );

        axum::Json(json!({
            "ok": true,
            "name": name,
            "file": format!("{}.tvdb", name),
            "bytes": total_bytes,
        }))
        .into_response()
    } else {
        // no data directory: stream to a temp file, load into memory, delete it
        if state.registry.get(&name).is_some() {
            return (
                StatusCode::CONFLICT,
                axum::Json(
                    json!({"error": format!("database '{}' already exists - drop it first", name)}),
                ),
            )
                .into_response();
        }

        let permit = match state.db_lifecycle_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::CONFLICT,
                    axum::Json(json!({"error": DB_LIFECYCLE_BUSY})),
                )
                    .into_response();
            }
        };

        let tmp_dir = std::env::temp_dir();
        let tmp_path = tmp_dir.join(format!("traverse_upload_{}.tvdb", name));

        let result = stream_to_file(&tmp_path, body).await;

        let total_bytes = match result {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
            }
        };

        // load from the temp file, then delete it
        let registry = state.registry.clone();
        let db_name = name.clone();
        let tmp_path_clone = tmp_path.clone();
        let load_result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let r = registry.load_database_from_file(&db_name, &tmp_path_clone);
            // always remove the temp file
            let _ = std::fs::remove_file(&tmp_path_clone);
            r
        })
        .await;

        match load_result {
            Ok(Ok(_)) => {
                tracing::info!(
                    "uploaded and loaded '{}.tvdb' ({} bytes, in-memory only)",
                    name,
                    total_bytes
                );
                axum::Json(json!({
                    "ok": true,
                    "name": name,
                    "bytes": total_bytes,
                    "loaded": true,
                }))
                .into_response()
            }
            Ok(Err(e)) => {
                let _ = std::fs::remove_file(&tmp_path);
                (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response()
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": format!("load task failed: {e}")})),
                )
                    .into_response()
            }
        }
    }
}

/// `?kind=model` branch of `files_upload`: saves the body as `<name>.tvmodel` next to the `.tvdb`
/// databases and registers it in `MODEL_STORE`, so the ML panel shows it without a restart. Needs
/// the `ml` feature (503 otherwise).
#[cfg(not(feature = "ml"))]
async fn files_upload_model(
    _state: AppState,
    _name: String,
    _body: axum::body::Body,
) -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({
            "error": "this server was built without the `ml` feature - rebuild with --features ml to enable model uploads"
        })),
    ).into_response()
}

#[cfg(feature = "ml")]
async fn files_upload_model(
    state: AppState,
    name: String,
    body: axum::body::Body,
) -> axum::response::Response {
    let raw_name = name.trim().to_string();
    if raw_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "model name is required"})),
        )
            .into_response();
    }

    #[cfg(feature = "ml")]
    if !traverse_cypher::ml::registry::is_safe_model_name(&raw_name) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "model name contains illegal characters (path separators, :, *, ?, <, >, |)"})),
        ).into_response();
    }

    // with disk persistence, stream to a `.tvmodel` file and register from it; without (in-memory
    // server) buffer the body and register from bytes: the model lives in MODEL_STORE until
    // restart, and Export works while the process runs
    if let Some(data_dir) = state.registry.data_dir().map(|d| d.to_path_buf()) {
        let final_path = data_dir.join(format!("{}.tvmodel", raw_name));
        if final_path.exists() {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({"error": format!("file '{}.tvmodel' already exists - delete it first", raw_name)})),
            ).into_response();
        }

        tracing::info!("upload starting: '{}.tvmodel'", raw_name);
        let total_bytes = match stream_to_file_ex(&final_path, body, None).await {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&final_path);
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
            }
        };

        #[cfg(feature = "ml")]
        if let Err(e) =
            traverse_cypher::ml::registry::register_model_from_file(&raw_name, &final_path)
        {
            let _ = std::fs::remove_file(&final_path);
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
        } else {
            tracing::info!(
                "uploaded and registered model '{}.tvmodel' ({} bytes)",
                raw_name,
                total_bytes
            );
        }

        return axum::Json(json!({
            "ok": true,
            "name": raw_name,
            "file": format!("{}.tvmodel", raw_name),
            "bytes": total_bytes,
            "kind": "model",
        }))
        .into_response();
    }

    // no data directory: buffer in memory and register directly; gone on restart
    let collected = match http_body_util::BodyExt::collect(body).await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": format!("upload stream error: {e}")})),
            )
                .into_response();
        }
    };
    let bytes = collected.to_bytes();
    let total_bytes = bytes.len();

    #[cfg(feature = "ml")]
    if let Err(e) = traverse_cypher::ml::registry::register_model_from_bytes(&raw_name, &bytes) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
    }
    tracing::info!(
        "uploaded model '{}.tvmodel' to in-memory store ({} bytes)",
        raw_name,
        total_bytes
    );

    axum::Json(json!({
        "ok": true,
        "name": raw_name,
        "file": format!("{}.tvmodel", raw_name),
        "bytes": total_bytes,
        "kind": "model",
        "in_memory_only": true,
    }))
    .into_response()
}

/// `stream_to_file_ex` with `expected_magic` set to the `TVDB` header, for the database uploads.
/// Other formats call `stream_to_file_ex` directly.
async fn stream_to_file(path: &std::path::Path, body: axum::body::Body) -> Result<u64, String> {
    stream_to_file_ex(path, body, Some(b"TVDB")).await
}

/// Streams a request body to a file, checking `expected_magic` on the first chunk.
///
/// Synchronous std::fs I/O, not tokio::fs: `tokio::fs::File::write_all()` goes
/// through spawn_blocking, so the handler returns Pending and hyper's dispatcher
/// keeps reading body data into fresh 256 KB buffers faster than they're
/// consumed. Synchronous writes keep the handler Ready, so the dispatcher can't
/// run ahead of the writer.
async fn stream_to_file_ex(
    path: &std::path::Path,
    body: axum::body::Body,
    expected_magic: Option<&[u8]>,
) -> Result<u64, String> {
    use http_body_util::BodyExt as _;
    use std::io::Write as _;

    tracing::info!("stream_to_file: creating {}", path.display());
    let mut file =
        std::fs::File::create(path).map_err(|e| format!("failed to create file: {e}"))?;
    tracing::info!("stream_to_file: file created, awaiting first frame");

    let mut body = body;
    let mut total_bytes: u64 = 0;
    let mut first_chunk = true;
    let mut last_progress_mb: u64 = 0;
    let mut frame_count: u64 = 0;

    loop {
        let frame = match body.frame().await {
            Some(f) => f,
            None => break, // end of body
        };
        frame_count += 1;
        let frame = frame.map_err(|e| format!("stream error: {e}"))?;
        let Some(data) = frame.data_ref() else {
            continue;
        };

        // check the magic header on the first chunk when the caller gave one (`.tvdb` uploads pass
        // `TVDB`; `.tvmodel` uploads pass `None`, since older model files have no header and
        // `Model::from_bytes` validates both layouts at registration)
        if first_chunk {
            if let Some(magic) = expected_magic {
                if data.len() < magic.len() {
                    return Err(format!(
                        "file too small - expected at least {} bytes",
                        magic.len()
                    ));
                }
                if &data[..magic.len()] != magic {
                    return Err(format!(
                        "invalid file - missing expected magic header ({:?})",
                        std::str::from_utf8(magic).unwrap_or("<binary>")
                    ));
                }
            }
            first_chunk = false;
        }

        file.write_all(data)
            .map_err(|e| format!("write error: {e}"))?;
        total_bytes += data.len() as u64;

        // progress every 50 MB, with the phase, to tell body.frame().await stalls from
        // file.write_all() stalls
        let cur_mb = total_bytes / (1024 * 1024);
        if cur_mb / 50 > last_progress_mb / 50 {
            tracing::info!(
                "upload: {} MB written, awaiting next frame (frame #{})",
                cur_mb,
                frame_count
            );
            last_progress_mb = cur_mb;
        }

        // memory pressure check every ~512 KB received
        if total_bytes % (512 * 1024) < data.len() as u64
            && let Some(ceiling) = traverse_core::memory::memory_ceiling()
        {
            let used = traverse_core::memory::allocated_bytes_fresh();
            if used > ceiling {
                return Err(format!(
                    "memory limit exceeded during upload ({} MB used, {} MB limit)",
                    used / (1024 * 1024),
                    ceiling / (1024 * 1024),
                ));
            }
        }
    }

    if first_chunk {
        return Err("empty request body - no data received".into());
    }

    // fsync for durability; the body is fully consumed, so blocking the tokio worker is safe
    // (nothing left to buffer) and the file survives a hard kill
    tracing::info!("upload fsync starting ({} MB)", total_bytes / (1024 * 1024));
    file.sync_all().map_err(|e| format!("fsync error: {e}"))?;
    tracing::info!("upload fsync complete");
    drop(file);

    Ok(total_bytes)
}

// -- GET /api/files/{name} -------------------------------------------
// Streams a .tvdb file from the data directory, loaded or not (read straight from the filesystem).

pub(super) async fn files_download(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let kind = query.get("kind").map(|s| s.as_str()).unwrap_or("database");

    // model downloads can fall back to serializing from MODEL_STORE when there's no file (e.g. a
    // server without TRAVERSE_DATA exporting a trained model); databases are too large to
    // materialize on the fly and always come from disk
    #[cfg(feature = "ml")]
    if kind == "model" {
        let raw_name = name.trim().to_string();
        // prefer the on-disk file when there is one (cheap streaming, clean re-import); otherwise
        // encode the in-memory model, so Export works for in-memory-only deployments
        let on_disk = state
            .registry
            .data_dir()
            .map(|d| d.join(format!("{}.tvmodel", raw_name)))
            .filter(|p| p.is_file());
        if on_disk.is_none()
            && let Some(bytes) = traverse_cypher::ml::registry::encode_model_bytes(&raw_name)
        {
            let len = bytes.len();
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{}.tvmodel\"", raw_name),
                    ),
                    (header::CONTENT_LENGTH, len.to_string()),
                ],
                bytes,
            )
                .into_response();
        }
    }

    let data_dir = match state.registry.data_dir() {
        Some(d) => d.to_path_buf(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "no data directory configured"})),
            )
                .into_response();
        }
    };

    // models keep their casing (filename = model name); databases are lowercased like the rest of
    // the registry expects
    let (name, ext) = if kind == "model" {
        (name.trim().to_string(), "tvmodel")
    } else {
        (name.trim().to_lowercase(), "tvdb")
    };
    let file_path = data_dir.join(format!("{}.{}", name, ext));
    if !file_path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("file '{}.{}' not found", name, ext)})),
        )
            .into_response();
    }

    // a loaded database is saved first, so the file has every commit (model files are written
    // eagerly on train), and the file is taken as of one moment: a user change while it streams
    // doesn't tear it
    let registry = Arc::clone(&state.registry);
    let database = (ext == "tvdb").then(|| name.clone());
    let path = file_path.clone();
    let opened =
        tokio::task::spawn_blocking(move || registry.open_for_download(database.as_deref(), &path))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
    let snapshot = match opened {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": e })),
            )
                .into_response();
        }
    };
    let len = snapshot.len;
    let rest = len - snapshot.head.len() as u64;
    let file = tokio::io::AsyncReadExt::take(tokio::fs::File::from_std(snapshot.file), rest);
    let reader = tokio::io::AsyncReadExt::chain(std::io::Cursor::new(snapshot.head), file);
    let stream = tokio_util::io::ReaderStream::new(reader);
    let body = axum::body::Body::from_stream(stream);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}.{}\"", name, ext),
            ),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        body,
    )
        .into_response()
}

// -- GET /api/files --------------------------------------------------
// Every persistable artifact in the data directory: `.tvdb` databases plus, with `--features ml`,
// `.tvmodel` trained models. Each entry has a `kind` so clients needn't inspect the extension.

pub(super) async fn files_list(State(state): State<AppState>) -> impl IntoResponse {
    let data_dir = match state.registry.data_dir() {
        Some(d) => d.to_path_buf(),
        None => {
            // no data directory: empty list (uploads go straight to memory)
            return axum::Json(json!([])).into_response();
        }
    };

    let entries = match std::fs::read_dir(&data_dir) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": format!("failed to read data directory: {e}")})),
            )
                .into_response();
        }
    };

    let mut files: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match ext {
            "tvdb" => {
                let loaded = state.registry.get(&stem).is_some();
                files.push(json!({
                    "name": stem,
                    "file": format!("{}.tvdb", stem),
                    "bytes": size,
                    "loaded": loaded,
                    "kind": "database",
                }));
            }
            "tvmodel" => {
                // models are tracked separately with the ml feature; without it the file is still
                // listed, with `loaded: false`
                #[cfg(feature = "ml")]
                let loaded = traverse_cypher::ml::registry::lookup_model(&stem).is_some();
                #[cfg(not(feature = "ml"))]
                let loaded = false;
                files.push(json!({
                    "name": stem,
                    "file": format!("{}.tvmodel", stem),
                    "bytes": size,
                    "loaded": loaded,
                    "kind": "model",
                }));
            }
            _ => {}
        }
    }

    axum::Json(json!(files)).into_response()
}

// -- DELETE /api/files/{name} ----------------------------------------
// Deletes a .tvdb file from the data directory; rejected while the database is loaded (drop it
// first).

pub(super) async fn files_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let kind = query.get("kind").map(|s| s.as_str()).unwrap_or("database");
    let data_dir = match state.registry.data_dir() {
        Some(d) => d.to_path_buf(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "no data directory configured"})),
            )
                .into_response();
        }
    };

    // model files skip the "currently loaded?" check; they're dropped from the ML registry first so
    // MODEL_STORE matches disk
    if kind == "model" {
        let raw_name = name.trim().to_string();
        let file_path = data_dir.join(format!("{}.tvmodel", raw_name));
        if !file_path.is_file() {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": format!("file '{}.tvmodel' not found", raw_name)})),
            )
                .into_response();
        }
        // a registered model is dropped with its file; a file that never loaded (or a build without
        // `ml`) is just removed below
        #[cfg(feature = "ml")]
        if let Err(e) = traverse_cypher::ml::registry::drop_model(&raw_name) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
        if file_path.exists()
            && let Err(e) = tokio::fs::remove_file(&file_path).await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": format!("failed to delete model file: {e}")})),
            )
                .into_response();
        }
        tracing::info!("deleted model file '{}.tvmodel'", raw_name);
        return axum::Json(json!({"ok": true})).into_response();
    }

    let name = name.trim().to_lowercase();
    let file_path = data_dir.join(format!("{}.tvdb", name));

    if !file_path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("file '{}.tvdb' not found", name)})),
        )
            .into_response();
    }

    // reject while the database is loaded
    if state.registry.get(&name).is_some() {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({"error": format!("database '{}' is currently loaded - drop it first", name)})),
        ).into_response();
    }

    match tokio::fs::remove_file(&file_path).await {
        Ok(()) => {
            tracing::info!("deleted file '{}.tvdb'", name);
            axum::Json(json!({"ok": true})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("failed to delete file: {e}")})),
        )
            .into_response(),
    }
}
