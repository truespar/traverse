//! Dataset download support for traverse-bench.
//!
//! Downloads pokec benchmark datasets from truespar.com, decompresses them,
//! and caches them locally.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Display a path with forward slashes (consistent cross-platform output).
fn display_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

const BASE_URL: &str = "https://releases.truespar.com/benchmark";

/// Returns the download URL for a given variant and format.
fn dataset_url(variant: &str, format: &str) -> String {
    format!("{BASE_URL}/pokec_{variant}_{format}")
}

/// Download the pokec dataset for the given variant.
///
/// Downloads the gzipped cypher file, decompresses it, and caches the result.
/// Returns the path to the uncompressed `.cypher` file.
pub fn download_dataset(
    variant: &str,
    cache_dir: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let filename = format!("pokec_{variant}_import.cypher");
    let cache_path = PathBuf::from(cache_dir).join(&filename);

    // Check cache first
    if cache_path.exists() {
        let size_mb = fs::metadata(&cache_path)?.len() as f64 / (1024.0 * 1024.0);
        eprintln!(
            "  Cached:  {} ({:.1} MB)",
            display_path(&cache_path),
            size_mb
        );
        return Ok(display_path(&cache_path));
    }

    fs::create_dir_all(cache_dir)?;

    let url = dataset_url(variant, "cypher.gz");
    eprintln!("  Source:  {url}");

    let response = ureq::get(&url)
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    let content_length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    // Download to a temp file first
    let tmp_path = cache_path.with_extension("cypher.tmp");
    download_and_decompress(
        response.into_body().into_reader(),
        &tmp_path,
        content_length,
    )?;

    // Atomically move into place
    fs::rename(&tmp_path, &cache_path)?;

    let size_mb = fs::metadata(&cache_path)?.len() as f64 / (1024.0 * 1024.0);
    eprintln!(
        "  Saved:   {} ({:.1} MB)",
        display_path(&cache_path),
        size_mb
    );

    Ok(display_path(&cache_path))
}

/// Download the pokec CSV files (nodes + edges) for Neo4j native import.
///
/// Downloads the tar.gz archive, extracts it, and caches the CSV files.
/// Returns the path to the directory containing the CSV files.
pub fn download_csv(variant: &str, cache_dir: &str) -> Result<String, Box<dyn std::error::Error>> {
    let nodes_file = format!("pokec_{variant}_nodes.csv");
    let edges_file = format!("pokec_{variant}_edges.csv");
    let nodes_path = PathBuf::from(cache_dir).join(&nodes_file);
    let edges_path = PathBuf::from(cache_dir).join(&edges_file);

    // Check cache first
    if nodes_path.exists() && edges_path.exists() {
        let n_mb = fs::metadata(&nodes_path)?.len() as f64 / (1024.0 * 1024.0);
        let e_mb = fs::metadata(&edges_path)?.len() as f64 / (1024.0 * 1024.0);
        eprintln!("  Cached:  {} ({:.1} MB)", display_path(&nodes_path), n_mb);
        eprintln!("           {} ({:.1} MB)", display_path(&edges_path), e_mb);
        return Ok(cache_dir.to_string());
    }

    fs::create_dir_all(cache_dir)?;

    let url = dataset_url(variant, "csv.tar.gz");
    eprintln!("  Source:  {url}");

    let response = ureq::get(&url)
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    let content_length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    // Download with progress, then extract tar.gz
    let mut downloaded: u64 = 0;
    let mut last_report: u64 = 0;
    let progress_reader = ProgressReader {
        inner: response.into_body().into_reader(),
        downloaded: &mut downloaded,
        last_report: &mut last_report,
        total: content_length,
    };

    let gz = flate2::read::GzDecoder::new(progress_reader);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        if file_name.ends_with("_nodes.csv") || file_name.ends_with("_edges.csv") {
            let out_path = PathBuf::from(cache_dir).join(&file_name);
            let mut out_file = fs::File::create(&out_path)?;
            io::copy(&mut entry, &mut out_file)?;
        }
    }

    eprintln!();

    let n_mb = fs::metadata(&nodes_path)?.len() as f64 / (1024.0 * 1024.0);
    let e_mb = fs::metadata(&edges_path)?.len() as f64 / (1024.0 * 1024.0);
    eprintln!("  Saved:   {} ({:.1} MB)", display_path(&nodes_path), n_mb);
    eprintln!("           {} ({:.1} MB)", display_path(&edges_path), e_mb);

    Ok(cache_dir.to_string())
}

/// Download a gzipped stream, decompress it, and write to the output path.
fn download_and_decompress(
    reader: impl Read,
    output_path: &Path,
    total_bytes: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    use flate2::read::GzDecoder;

    let mut downloaded: u64 = 0;
    let mut last_report: u64 = 0;

    // Wrap reader to track download progress
    let progress_reader = ProgressReader {
        inner: reader,
        downloaded: &mut downloaded,
        last_report: &mut last_report,
        total: total_bytes,
    };

    let decoder = GzDecoder::new(progress_reader);
    let mut buf_reader = io::BufReader::new(decoder);
    let mut out_file = fs::File::create(output_path)?;
    let mut buf = vec![0u8; 256 * 1024];

    loop {
        let n = buf_reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out_file.write_all(&buf[..n])?;
    }

    eprintln!();
    Ok(())
}

/// A reader wrapper that prints download progress.
struct ProgressReader<'a, R> {
    inner: R,
    downloaded: &'a mut u64,
    last_report: &'a mut u64,
    total: Option<u64>,
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        *self.downloaded += n as u64;

        // Report every ~1 MB
        if *self.downloaded - *self.last_report > 1_000_000 {
            *self.last_report = *self.downloaded;
            let dl_mb = *self.downloaded as f64 / (1024.0 * 1024.0);
            if let Some(total) = self.total {
                let total_mb = total as f64 / (1024.0 * 1024.0);
                let pct = (*self.downloaded as f64 / total as f64 * 100.0).min(100.0);
                eprint!("\r  {dl_mb:.1} / {total_mb:.1} MB ({pct:.0}%)    ");
            } else {
                eprint!("\r  {dl_mb:.1} MB    ");
            }
        }

        Ok(n)
    }
}
