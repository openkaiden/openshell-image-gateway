//! Windows host path → container-machine path conversion.
//!
//! On Windows, Docker Desktop and Podman run their engine inside a Linux VM.
//! A bind-mount source such as `C:\Users\me\proj` has to be expressed as the
//! path under which the VM sees the Windows drive:
//!
//! * Docker Desktop: `/run/desktop/mnt/host/c/Users/me/proj`
//! * Podman machine: `/mnt/c/Users/me/proj`

use std::fmt;

/// Error for a value that looks like a Windows path but can't be mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    pub path: String,
    pub reason: &'static str,
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot convert Windows path {:?}: {}",
            self.path, self.reason
        )
    }
}

impl std::error::Error for ConvertError {}

/// Returns `Ok(None)` when `source` is not a Windows path (leave it alone),
/// `Ok(Some(path))` with the machine path, or an error when it is a Windows
/// path that cannot be mapped (UNC shares, `..` components, ...).
pub fn convert_windows_path(source: &str, prefix: &str) -> Result<Option<String>, ConvertError> {
    let err = |reason| ConvertError {
        path: source.to_string(),
        reason,
    };

    let mut rest = source;
    // Win32 file namespace / device namespace prefixes: \\?\C:\ and \\.\C:\
    for ns in [r"\\?\", r"\\.\", "//?/", "//./"] {
        if let Some(stripped) = rest.strip_prefix(ns) {
            if stripped.len() >= 4 && stripped[..4].eq_ignore_ascii_case(r"UNC\") {
                return Err(err("UNC network paths are not supported"));
            }
            rest = stripped;
            break;
        }
    }

    let bytes = rest.as_bytes();
    let has_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if !has_drive {
        // `\\server\share`. (`//x` is left alone: it is a valid POSIX path.)
        if rest.starts_with(r"\\") {
            return Err(err("UNC network paths are not supported"));
        }
        // A plain POSIX path (or a volume name): not ours to touch.
        return Ok(None);
    }
    // `C:` must be followed by a separator or end the string; `C:foo` is a
    // drive-relative path whose meaning depends on the client's cwd.
    let tail = &rest[2..];
    if !(tail.is_empty() || tail.starts_with('\\') || tail.starts_with('/')) {
        return Err(err(
            "drive-relative paths are not supported; use an absolute path like C:\\dir",
        ));
    }

    let drive = (bytes[0] as char).to_ascii_lowercase();
    let mut components = Vec::new();
    for part in tail.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => return Err(err("'..' components are not allowed")),
            p => components.push(p),
        }
    }

    let prefix = prefix.trim_end_matches('/');
    let mut out = String::with_capacity(prefix.len() + tail.len() + 3);
    out.push_str(prefix);
    out.push('/');
    out.push(drive);
    for c in components {
        out.push('/');
        out.push_str(c);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCKER: &str = "/run/desktop/mnt/host";
    const PODMAN: &str = "/mnt";

    fn ok(src: &str, prefix: &str) -> Option<String> {
        convert_windows_path(src, prefix).unwrap()
    }

    #[test]
    fn converts_backslash_paths() {
        assert_eq!(
            ok(r"C:\Users\me\proj", DOCKER).as_deref(),
            Some("/run/desktop/mnt/host/c/Users/me/proj")
        );
        assert_eq!(
            ok(r"C:\Users\me\proj", PODMAN).as_deref(),
            Some("/mnt/c/Users/me/proj")
        );
    }

    #[test]
    fn converts_forward_slash_and_mixed_paths() {
        assert_eq!(
            ok("D:/data/set", PODMAN).as_deref(),
            Some("/mnt/d/data/set")
        );
        assert_eq!(ok(r"e:\a/b\\c\", PODMAN).as_deref(), Some("/mnt/e/a/b/c"));
        assert_eq!(ok(r"C:\a\.\b", PODMAN).as_deref(), Some("/mnt/c/a/b"));
    }

    #[test]
    fn converts_drive_roots() {
        assert_eq!(ok(r"C:\", PODMAN).as_deref(), Some("/mnt/c"));
        assert_eq!(ok("C:", DOCKER).as_deref(), Some("/run/desktop/mnt/host/c"));
    }

    #[test]
    fn keeps_spaces_and_unicode() {
        assert_eq!(
            ok(r"C:\Users\Jérôme M\My Project", PODMAN).as_deref(),
            Some("/mnt/c/Users/Jérôme M/My Project")
        );
    }

    #[test]
    fn strips_win32_namespace_prefix() {
        assert_eq!(
            ok(r"\\?\C:\long\path", PODMAN).as_deref(),
            Some("/mnt/c/long/path")
        );
        assert_eq!(ok(r"\\.\C:\dev", PODMAN).as_deref(), Some("/mnt/c/dev"));
    }

    #[test]
    fn prefix_trailing_slash_is_ignored() {
        assert_eq!(ok(r"C:\x", "/mnt/").as_deref(), Some("/mnt/c/x"));
    }

    #[test]
    fn leaves_non_windows_values_alone() {
        assert_eq!(ok("/home/me/proj", PODMAN), None);
        assert_eq!(ok("/mnt/c/Users", PODMAN), None);
        assert_eq!(ok("my-volume", PODMAN), None);
        assert_eq!(ok("relative/dir", PODMAN), None);
        assert_eq!(ok("", PODMAN), None);
    }

    #[test]
    fn rejects_unmappable_windows_paths() {
        assert!(convert_windows_path(r"\\server\share\dir", PODMAN).is_err());
        assert!(convert_windows_path(r"\\?\UNC\server\share", PODMAN).is_err());
        assert!(convert_windows_path(r"C:\a\..\b", PODMAN).is_err());
        assert!(convert_windows_path("C:relative", PODMAN).is_err());
    }
}
