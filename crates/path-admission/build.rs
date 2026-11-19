//! Downloads the OpenShell protobuf definitions for the version pinned in
//! `<repo>/openshell.version` (bare version such as `0.1.9`; the git tag is
//! `v<version>`), then compiles them with tonic.
//!
//! Starting from `gateway_interceptor.proto` and `extension.proto`, every
//! non-`google/` import is fetched recursively from
//! `https://raw.githubusercontent.com/NVIDIA/OpenShell/<tag>/proto/`.
//! Files are cached per tag in `<target dir>/openshell-proto/<tag>/`.
//!
//! Environment overrides:
//! * `OPENSHELL_PROTO_DIR`: use a local `proto/` directory, no download
//!   (offline builds or a checkout of the OpenShell repo).
//! * `OPENSHELL_PROTO_BASE_URL`: mirror base URL; the tag and
//!   `/proto/<file>` are appended.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ROOT_PROTOS: &[&str] = &["gateway_interceptor.proto", "extension.proto"];
const DEFAULT_BASE_URL: &str = "https://raw.githubusercontent.com/NVIDIA/OpenShell";

type BoxError = Box<dyn std::error::Error>;

fn main() -> Result<(), BoxError> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let version_file = manifest_dir.join("../../openshell.version");
    println!("cargo:rerun-if-changed={}", version_file.display());
    println!("cargo:rerun-if-env-changed=OPENSHELL_PROTO_DIR");
    println!("cargo:rerun-if-env-changed=OPENSHELL_PROTO_BASE_URL");

    let proto_dir = match std::env::var_os("OPENSHELL_PROTO_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            println!("cargo:rerun-if-changed={}", dir.display());
            dir
        }
        None => {
            let tag = read_tag(&version_file)?;
            let base = std::env::var("OPENSHELL_PROTO_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
            let out = cache_root()?.join("openshell-proto").join(sanitize(&tag));
            download_protos(&base, &tag, &out)?;
            out
        }
    };

    // protox is a pure-Rust protobuf compiler with the google/protobuf
    // well-known types built in, so no system protoc is needed.
    let roots: Vec<PathBuf> = ROOT_PROTOS.iter().map(|f| proto_dir.join(f)).collect();
    let descriptors = protox::compile(&roots, [&proto_dir])?;
    tonic_prost_build::configure()
        .build_server(true)
        // The client is only used by tests.
        .build_client(true)
        .compile_fds(descriptors)?;
    Ok(())
}

/// Reads the OpenShell version (e.g. `0.1.9`, same form as the gateway image
/// tag) and returns the matching git tag (`v0.1.9`).
fn read_tag(version_file: &Path) -> Result<String, BoxError> {
    let raw = fs::read_to_string(version_file)
        .map_err(|e| format!("reading {}: {e}", version_file.display()))?;
    // First non-empty, non-comment line.
    let version = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| format!("{} does not contain a version", version_file.display()))?;
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!(
            "{} must contain a bare version such as 0.1.9 (no leading \"v\"), got {version:?}",
            version_file.display()
        )
        .into());
    }
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
    {
        return Err(format!(
            "invalid OpenShell version {version:?} in {}",
            version_file.display()
        )
        .into());
    }
    Ok(format!("v{version}"))
}

/// Shared download cache: the profile directory (parent of `build/`), so
/// `cargo build`, `cargo test` and `cargo clippy`, which each get their own
/// OUT_DIR, download a tag only once. OUT_DIR is
/// `<target>[/<triple>]/<profile>/build/<pkg>…/out`; the depth below `build`
/// varies between cargo versions. Falls back to OUT_DIR.
fn cache_root() -> Result<PathBuf, BoxError> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    Ok(out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(out_dir))
}

fn sanitize(tag: &str) -> String {
    tag.replace(['/', '\\'], "_")
}

/// Fetches the root protos and their transitive imports into `out`.
fn download_protos(base: &str, tag: &str, out: &Path) -> Result<(), BoxError> {
    fs::create_dir_all(out)?;
    let mut pending: Vec<String> = ROOT_PROTOS.iter().map(|s| s.to_string()).collect();
    let mut seen = BTreeSet::new();
    while let Some(file) = pending.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let dest = out.join(&file);
        let contents = if dest.is_file() {
            fs::read_to_string(&dest)?
        } else {
            let url = format!("{}/{tag}/proto/{file}", base.trim_end_matches('/'));
            let body = ureq::get(&url)
                .call()
                .map_err(|e| format!("downloading {url}: {e}"))?
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("reading {url}: {e}"))?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            // Write atomically so an interrupted build never leaves a
            // truncated file in the cache.
            let tmp = dest.with_extension("proto.part");
            fs::write(&tmp, &body)?;
            fs::rename(&tmp, &dest)?;
            body
        };
        for import in imports(&contents) {
            // Well-known types are built into protox.
            if !import.starts_with("google/") {
                pending.push(import);
            }
        }
    }
    Ok(())
}

/// Extracts `import "x.proto";` / `import public "x.proto";` targets.
fn imports(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("import")?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            let rest = rest.trim_start();
            let rest = rest
                .strip_prefix("public")
                .or_else(|| rest.strip_prefix("weak"))
                .unwrap_or(rest)
                .trim_start();
            let rest = rest.strip_prefix('"')?;
            let end = rest.find('"')?;
            let path = &rest[..end];
            // Never let an import escape the cache directory.
            if path.split('/').any(|c| c == ".." || c.is_empty()) || path.starts_with('/') {
                return None;
            }
            Some(path.to_string())
        })
        .collect()
}
