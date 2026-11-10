//! Manual self-update for the `elo` binary.
//!
//! ELO intentionally has no networking library.  Updates are performed by
//! shelling out to the system `curl`, downloading a release archive and its
//! checksum file, verifying the SHA-256 digest, and atomically replacing the
//! running binary.  No update runs automatically, from hooks, or from the MCP
//! server.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const REPO: &str = "anjoscd/elo-db-ai";
const RELEASE_URL: &str = "https://github.com/anjoscd/elo-db-ai/releases";
const MAX_BINARY_SIZE: u64 = 64 * 1024 * 1024;

/// Validate a version string in the form `^v?\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$`.
/// Returns true when the version matches the allowed semantic-like pattern.
pub fn is_valid_version(version: &str) -> bool {
    let mut chars = version.trim().chars().peekable();

    // Optional leading 'v'.
    if chars.peek() == Some(&'v') || chars.peek() == Some(&'V') {
        chars.next();
    }

    // Consume a dot-separated numeric segment.
    fn consume_numeric(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
        let mut saw_digit = false;
        while let Some(&c) = chars.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            saw_digit = true;
            chars.next();
        }
        saw_digit
    }

    if !consume_numeric(&mut chars) {
        return false;
    }
    if chars.next() != Some('.') {
        return false;
    }
    if !consume_numeric(&mut chars) {
        return false;
    }
    if chars.next() != Some('.') {
        return false;
    }
    if !consume_numeric(&mut chars) {
        return false;
    }

    // Optional pre-release suffix.
    match chars.peek() {
        Some(&'-') => {
            chars.next();
            let mut saw_valid = false;
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '.' {
                    saw_valid = true;
                    chars.next();
                } else {
                    return false;
                }
            }
            if !saw_valid {
                return false;
            }
        }
        None => {}
        _ => return false,
    }

    chars.peek().is_none()
}

/// Options parsed from the CLI.
#[derive(Debug, Clone, Default)]
pub struct UpdateOpts {
    /// Only show current and latest versions; do not download anything.
    pub check: bool,
    /// Pin the version to install (without the leading `v`).
    pub version: Option<String>,
}

/// Abstraction over the actual download step so tests can avoid network calls.
pub trait Fetcher {
    /// Fetch the contents of `url`.  The implementation is responsible for
    /// failing on HTTP error codes.
    fn fetch(&self, url: &str) -> Result<Vec<u8>, Error>;
}

/// Runs `curl` with strict transport options.
pub struct CurlFetcher;

impl Fetcher for CurlFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, Error> {
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("--fail")
            .arg("--location")
            .arg("--proto")
            .arg("=https")
            .arg("--tlsv1.2")
            .arg("--silent")
            .arg("--show-error")
            .arg("-o")
            .arg("-")
            .arg(url);
        if url.contains("api.github.com") {
            cmd.arg("-A").arg("elo-update");
        }
        let out = cmd.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::CurlNotFound
            } else {
                Error::Download(format!("{e}"))
            }
        })?;
        if !out.status.success() {
            return Err(Error::Download(
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
        Ok(out.stdout)
    }
}

/// Errors that can occur while updating.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("curl is not installed or not on PATH")]
    CurlNotFound,
    #[error("download failed: {0}")]
    Download(String),
    #[error("no asset found for target {0}")]
    NoAssetForTarget(String),
    #[error("could not find checksum for {0}")]
    MissingChecksum(String),
    #[error("SHA-256 checksum mismatch")]
    ChecksumMismatch,
    #[error("archive does not contain a single `elo` binary")]
    InvalidArchive,
    #[error("failed to replace binary: {0}")]
    ReplaceBinary(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Returns the Rust target triple for the running binary.
pub fn current_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        // Best-effort fallback; the update will fail cleanly when no asset
        // matches.
        (os, arch) => {
            eprintln!("warning: unsupported platform {os}-{arch} for prebuilt updates");
            "unsupported"
        }
    }
}

/// Archive name for a given version, e.g. `elo-0.2.0-aarch64-apple-darwin.tar.gz`.
pub fn asset_name(version: &str) -> String {
    format!("elo-{}-{}.tar.gz", version, current_target())
}

/// Runs an update and returns the installed version string.
pub fn run(
    opts: &UpdateOpts,
    current_version: &str,
    current_exe: &Path,
    fetcher: &dyn Fetcher,
) -> Result<String, Error> {
    // Defence in depth: callers other than the CLI get the same validation.
    if let Some(v) = opts.version.as_deref() {
        if !is_valid_version(v) || v.starts_with(['v', 'V']) {
            return Err(Error::Other(format!("invalid version: {v}")));
        }
    }
    if current_target() == "unsupported" {
        return Err(Error::NoAssetForTarget(
            "unsupported platform for prebuilt updates".into(),
        ));
    }

    if opts.check {
        let latest = latest_version(fetcher)?;
        println!("current version: {current_version}");
        println!("latest version:  {latest}");
        return Ok(latest);
    }

    let version = match &opts.version {
        Some(v) => v.clone(),
        None => latest_version(fetcher)?,
    };

    if version == current_version {
        println!("already at {current_version}");
        return Ok(version);
    }

    let asset = asset_name(&version);
    let asset_url = format!("{RELEASE_URL}/download/v{version}/{asset}",);
    let sums_url = format!("{RELEASE_URL}/download/v{version}/SHA256SUMS");

    let new_binary_path = new_binary_path(current_exe);

    let archive = fetcher.fetch(&asset_url)?;
    let sums = fetcher.fetch(&sums_url)?;
    let sums_text = String::from_utf8(sums)
        .map_err(|e| Error::Other(format!("invalid SHA256SUMS encoding: {e}")))?;

    let expected_hex = extract_checksum(&sums_text, &asset)
        .ok_or_else(|| Error::MissingChecksum(asset.clone()))?;

    if !verify_checksum(&archive, &expected_hex) {
        return Err(Error::ChecksumMismatch);
    }

    let new_binary = extract_binary(&archive)?;
    std::fs::write(&new_binary_path, new_binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&new_binary_path)?;
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&new_binary_path, perms)?;
    }

    replace_binary(&new_binary_path, current_exe)?;

    println!("updated to {version}");
    Ok(version)
}

fn new_binary_path(current_exe: &Path) -> PathBuf {
    let mut new_name = current_exe
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("elo"));
    new_name.push(".new");
    current_exe.with_file_name(new_name)
}

/// Fetches and parses the latest release version from the GitHub API.
fn latest_version(fetcher: &dyn Fetcher) -> Result<String, Error> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = fetcher.fetch(&url)?;
    parse_latest_version(&body)
}

/// Parses a GitHub releases API JSON body and returns the tag name without the
/// leading `v`.
pub fn parse_latest_version(body: &[u8]) -> Result<String, Error> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::Other(format!("failed to parse release JSON: {e}")))?;
    let tag = value["tag_name"]
        .as_str()
        .ok_or_else(|| Error::Other("missing tag_name in release".into()))?;
    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag).to_string();
    if !is_valid_version(&version) {
        return Err(Error::Other(format!(
            "invalid version in release tag: {tag}"
        )));
    }
    Ok(version)
}

/// Locates the checksum line for `file_name` inside a `sha256sum` text file.
/// The line must have the form `<64 hex chars>  [<*]filename`, comments
/// (lines starting with `#`) are ignored, and the file name must match exactly.
pub fn extract_checksum(sums: &str, file_name: &str) -> Option<String> {
    for line in sums.lines().map(|l| l.trim()) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let hex = parts[0];
        let name = parts[1].trim_start_matches('*');
        if name != file_name {
            continue;
        }
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        return Some(hex.to_ascii_lowercase());
    }
    None
}

/// Compares the SHA-256 digest of `data` with the lower/upper-case hex string
/// `expected_hex`.
pub fn verify_checksum(data: &[u8], expected_hex: &str) -> bool {
    let expected = expected_hex.trim();
    let digest = format_sha256(&Sha256::digest(data));
    digest.eq_ignore_ascii_case(expected)
}

fn format_sha256(hash: &sha2::digest::Output<Sha256>) -> String {
    format!("{:x}", hash)
}

/// Extracts the `elo` binary from a `.tar.gz` archive.
/// Only regular files named exactly `elo` (at the root or inside a single
/// directory level) are considered; if there is not exactly one such entry the
/// archive is rejected. The binary is limited to 64 MiB.
pub fn extract_binary(tar_gz: &[u8]) -> Result<Vec<u8>, Error> {
    let decoder = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(decoder);
    let mut binary: Option<Vec<u8>> = None;
    for entry in archive.entries().map_err(|e| Error::Other(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Other(e.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| Error::Other(e.to_string()))?;
        let components: Vec<_> = path.components().collect();
        if components.len() > 2 {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) != Some("elo") {
            continue;
        }
        let mut data = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut entry, MAX_BINARY_SIZE + 1),
            &mut data,
        )
        .map_err(|e| Error::Other(e.to_string()))?;
        if data.len() > MAX_BINARY_SIZE as usize {
            return Err(Error::Other("extracted binary exceeds 64 MiB".into()));
        }
        if binary.is_some() {
            return Err(Error::InvalidArchive);
        }
        binary = Some(data);
    }
    binary.ok_or(Error::InvalidArchive)
}

/// Atomically replaces `current_exe` with the file at `new_binary_path`, keeping
/// the old binary as `<current_exe>.old`.
/// The new binary is written next to the current executable (`<exe>.new`),
/// then the current executable is copied to the backup path and the new binary
/// is renamed over it in a single atomic step. If the rename fails the `.new`
/// file is removed and the current executable is left untouched.
pub fn replace_binary(new_binary_path: &Path, current_exe: &Path) -> Result<(), Error> {
    let backup = backup_path(current_exe);
    if let Err(e) = std::fs::copy(current_exe, &backup) {
        // No backup, no swap: the staged binary must not be left behind.
        let _ = std::fs::remove_file(new_binary_path);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(new_binary_path, current_exe) {
        let _ = std::fs::remove_file(new_binary_path);
        return Err(Error::ReplaceBinary(e));
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".old");
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct MockFetcher {
        responses: std::collections::HashMap<String, Vec<u8>>,
    }

    impl Fetcher for MockFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>, Error> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| Error::Download(format!("unexpected url {url}")))
        }
    }

    #[test]
    fn parse_latest_version_strips_v_prefix() {
        let body = br#"{"tag_name":"v0.2.0"}"#;
        assert_eq!(parse_latest_version(body).unwrap(), "0.2.0");
    }

    #[test]
    fn parse_latest_version_accepts_plain_tag() {
        let body = br#"{"tag_name":"0.2.0"}"#;
        assert_eq!(parse_latest_version(body).unwrap(), "0.2.0");
    }

    #[test]
    fn parse_latest_version_rejects_invalid_tag() {
        let body = br#"{"tag_name":"bad-version"}"#;
        assert!(parse_latest_version(body).is_err());
    }

    #[test]
    fn is_valid_version_accepts_valid() {
        assert!(is_valid_version("0.2.0"));
        assert!(is_valid_version("v0.2.0"));
        assert!(is_valid_version("1.2.3-alpha.1"));
        assert!(is_valid_version("v1.2.3-beta.2"));
    }

    #[test]
    fn is_valid_version_rejects_invalid() {
        assert!(!is_valid_version("../x"));
        assert!(!is_valid_version(""));
        assert!(!is_valid_version("1.2"));
        assert!(!is_valid_version("1.2.3.4"));
        assert!(!is_valid_version("a.b.c"));
        assert!(!is_valid_version("1.2.3-"));
        assert!(!is_valid_version("1.2.3 /etc/passwd"));
    }

    #[test]
    fn extract_checksum_finds_file() {
        let sums = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  elo-0.2.0-aarch64-apple-darwin.tar.gz\n\n".to_string()
            + "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  elo-0.2.0-x86_64-apple-darwin.tar.gz";
        assert_eq!(
            extract_checksum(&sums, "elo-0.2.0-aarch64-apple-darwin.tar.gz"),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        );
    }

    #[test]
    fn extract_checksum_skips_comments_and_requires_exact_name() {
        let sums = "# comment line\n".to_string()
            + "ccccccccccccccccbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  elo-0.2.0-x86_64-apple-darwin.tar.gz\n"
            + "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd  *elo-0.2.0-aarch64-apple-darwin.tar.gz\n"
            + "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee  elo-0.2.0-aarch64-apple-darwin.tar.gz-extra";
        assert_eq!(
            extract_checksum(&sums, "elo-0.2.0-aarch64-apple-darwin.tar.gz"),
            Some("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string())
        );
    }

    #[test]
    fn extract_checksum_requires_64_hex_chars() {
        let hex64 = "a".repeat(64);
        let sums = "not64hex  elo-0.2.0-aarch64-apple-darwin.tar.gz\n".to_string()
            + &format!("{hex64}  elo-0.2.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            extract_checksum(&sums, "elo-0.2.0-aarch64-apple-darwin.tar.gz"),
            Some(hex64)
        );
    }

    #[test]
    fn verify_checksum_ok_and_bad() {
        let data = b"hello world";
        let good = format_sha256(&Sha256::digest(data));
        assert!(verify_checksum(data, &good));
        assert!(!verify_checksum(
            data,
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut ar = tar::Builder::new(Vec::new());
        for (name, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            ar.append_data(&mut header, *name, *content).unwrap();
        }
        let uncompressed = ar.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&uncompressed).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extract_binary_finds_elo_entry() {
        let tar_gz = tar_gz(&[("elo", b"12345")]);
        assert_eq!(extract_binary(&tar_gz).unwrap(), b"12345");
    }

    #[test]
    fn extract_binary_accepts_one_level_directory() {
        let tar_gz = tar_gz(&[("release/elo", b"12345")]);
        assert_eq!(extract_binary(&tar_gz).unwrap(), b"12345");
    }

    #[test]
    fn extract_binary_rejects_directory() {
        let mut ar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_cksum();
        ar.append_data(&mut header, "elo/", &b""[..]).unwrap();
        let uncompressed = ar.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&uncompressed).unwrap();
        let tar_gz = encoder.finish().unwrap();
        assert!(extract_binary(&tar_gz).is_err());
    }

    #[test]
    fn extract_binary_rejects_multiple_elo() {
        let tar_gz = tar_gz(&[("elo", b"first"), ("elo", b"second")]);
        assert!(extract_binary(&tar_gz).is_err());
    }

    #[test]
    fn extract_binary_rejects_oversized() {
        let content = vec![0u8; MAX_BINARY_SIZE as usize + 1];
        let tar_gz = tar_gz(&[("elo", &content)]);
        assert!(extract_binary(&tar_gz).is_err());
    }

    #[test]
    fn replace_binary_swaps_and_keeps_old() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("elo");
        let new_bin = tmp.path().join("elo.new");
        std::fs::write(&current, "old").unwrap();
        std::fs::write(&new_bin, "new").unwrap();
        replace_binary(&new_bin, &current).unwrap();
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(current.with_extension("old")).unwrap(),
            "old"
        );
    }

    #[test]
    fn replace_binary_keeps_current_when_rename_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("elo");
        // Non-existent path simulates a failed rename after copy.
        let new_bin = tmp.path().join("does-not-exist.new");
        std::fs::write(&current, "old").unwrap();
        assert!(replace_binary(&new_bin, &current).is_err());
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "old");
        assert!(!new_bin.exists());
    }

    #[test]
    fn current_target_maps_known_platforms() {
        let target = current_target();
        assert!([
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "unsupported",
        ]
        .contains(&target));
    }

    #[test]
    fn check_only_prints_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("elo");
        std::fs::write(&current, "old").unwrap();
        let fetcher = MockFetcher {
            responses: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "https://api.github.com/repos/anjoscd/elo-db-ai/releases/latest".to_string(),
                    br#"{"tag_name":"v0.2.0"}"#.to_vec(),
                );
                m
            },
        };
        let opts = UpdateOpts {
            check: true,
            version: None,
        };
        let result = run(&opts, "0.1.0", &current, &fetcher);
        assert_eq!(result.unwrap(), "0.2.0");
    }

    #[test]
    fn uppercase_v_tag_is_normalized() {
        let json = r#"{"tag_name":"V1.2.3"}"#;
        assert_eq!(parse_latest_version(json.as_bytes()).unwrap(), "1.2.3");
    }
}
