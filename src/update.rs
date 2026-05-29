//! Self-update: `flat-cyborg update [--check]`.
//!
//! Mirrors agentis's `update` command in spirit, but keeps flat-cyborg's
//! dependency tree minimal: instead of bundling an HTTP/TLS stack and a hashing
//! crate, it shells out to the same tools the install script uses — `curl` or
//! `wget` for downloads and `sha256sum`/`shasum` for verification.
//!
//! It resolves the latest GitHub release, compares it to the running version,
//! downloads the matching platform asset, verifies its SHA256 checksum
//! (fail-closed unless `FLAT_CYBORG_INSECURE=1`), and atomically replaces the
//! running executable — handling the Linux `ETXTBSY` case (a running ELF cannot
//! be renamed over) by unlinking first, and falling back to `sudo` when the
//! install directory is not writable.

use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

const REPO: &str = "Replikanti/flat-cyborg";

/// Entry point for the `update` subcommand.
pub fn cmd_update(args: &[String]) -> ExitCode {
    let check_only = args.iter().any(|a| a == "--check");
    if let Some(bad) = args.iter().find(|a| *a != "--check") {
        eprintln!("flat-cyborg update: unknown argument: {bad}");
        eprintln!("Usage: flat-cyborg update [--check]");
        return ExitCode::from(2);
    }
    match run_update(check_only) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flat-cyborg update: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_update(check_only: bool) -> Result<ExitCode, String> {
    let current = flat_cyborg::VERSION;
    let tag = latest_tag()?;
    let latest = tag.trim_start_matches('v');

    if !is_newer(&tag, current) {
        println!("flat-cyborg {current} is already up to date (latest: {latest}).");
        return Ok(ExitCode::SUCCESS);
    }
    println!("Update available: {current} -> {latest}");
    if check_only {
        println!("Run `flat-cyborg update` to install it.");
        return Ok(ExitCode::SUCCESS);
    }

    let asset = asset_name()?;
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let url = format!("{base}/{asset}");

    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;

    // Prefer staging next to the running binary: that directory is on the same
    // filesystem (so the final install is an atomic rename) and, for a system
    // install like /usr/local/bin, is not world-writable (so no /tmp symlink
    // race). If it is not writable, fall back to a private 0700 temp dir and
    // install via sudo.
    let in_dir = current_exe.with_file_name(format!(".flat-cyborg-update-{}", std::process::id()));
    let (staged, privileged) = if fetch_to_file(&url, &in_dir).is_ok() {
        (in_dir, false)
    } else {
        let dir = make_private_dir()?;
        let staged = dir.join(&asset);
        if let Err(e) = fetch_to_file(&url, &staged) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        (staged, true)
    };

    let cleanup = || {
        let _ = std::fs::remove_file(&staged);
        if privileged {
            if let Some(parent) = staged.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    };

    if let Err(e) = verify_checksum(&base, &asset, &staged) {
        cleanup();
        return Err(e);
    }
    if let Err(e) = set_executable(&staged) {
        cleanup();
        return Err(e);
    }

    let result = if privileged {
        sudo_replace(&staged, &current_exe)
    } else {
        replace_executable(&staged, &current_exe)
    };
    if let Err(e) = result {
        cleanup();
        return Err(e);
    }
    cleanup();

    println!("Updated flat-cyborg to {latest}.");
    Ok(ExitCode::SUCCESS)
}

/// Creates an exclusively-owned `0700` directory under the system temp dir for
/// staging when the install directory is not writable.
fn make_private_dir() -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("flat-cyborg-update-{}", std::process::id()));
    // Remove a stale dir left by a previous run with the same pid, then create
    // exclusively (create_dir fails on an existing path or a symlink).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).map_err(|e| format!("cannot create staging dir: {e}"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot secure staging dir: {e}"))?;
    Ok(dir)
}

/// Verifies `tmp` against the release `.sha256`. Fails closed: a missing
/// checksum file or no sha256 tool aborts unless `FLAT_CYBORG_INSECURE=1`.
fn verify_checksum(base: &str, asset: &str, tmp: &Path) -> Result<(), String> {
    let insecure = std::env::var("FLAT_CYBORG_INSECURE").as_deref() == Ok("1");
    let skip = |reason: &str| -> Result<(), String> {
        if insecure {
            eprintln!(
                "Warning: {reason}; installing WITHOUT verification (FLAT_CYBORG_INSECURE=1)"
            );
            Ok(())
        } else {
            Err(format!(
                "{reason}. Refusing to install an unverified binary; set FLAT_CYBORG_INSECURE=1 to override"
            ))
        }
    };

    let sum_text = match fetch_text(&format!("{base}/{asset}.sha256")) {
        Ok(t) if !t.trim().is_empty() => t,
        _ => return skip(&format!("checksum file {asset}.sha256 unavailable")),
    };
    let expected = sum_text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    let Some(actual) = sha256_file(tmp) else {
        return skip("no sha256 tool (sha256sum/shasum) found");
    };
    if expected != actual.to_lowercase() {
        return Err(format!(
            "checksum mismatch for {asset}\n  expected: {expected}\n  actual:   {actual}"
        ));
    }
    println!("Checksum verified.");
    Ok(())
}

/// Installs `staged` as `current_exe` using **rename-aside**: the live binary
/// is moved aside, the new one is renamed into the now-empty path, and the
/// backup is deleted. On failure the backup is restored, so the user is never
/// left without a working binary. `staged` must be on the same filesystem as
/// `current_exe` (the caller stages it in the install directory).
///
/// Moving the running binary *away* is permitted on Linux (unlike renaming
/// *over* it, which fails with `ETXTBSY`), so this path avoids `ETXTBSY` by
/// construction.
fn replace_executable(staged: &Path, current_exe: &Path) -> Result<(), String> {
    let backup = current_exe.with_file_name(format!(".flat-cyborg-old-{}", std::process::id()));

    std::fs::rename(current_exe, &backup)
        .map_err(|e| format!("cannot move the current binary aside: {e}"))?;

    match std::fs::rename(staged, current_exe) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup);
            Ok(())
        }
        Err(e) => {
            // Put the original back; the install is never left empty.
            let _ = std::fs::rename(&backup, current_exe);
            Err(format!(
                "failed to install the new binary (original restored): {e}"
            ))
        }
    }
}

/// Privileged variant of [`replace_executable`] for a non-writable install
/// directory. Uses `sudo mv` (rename-aside), so the install is never left
/// empty and a running ELF is moved aside rather than overwritten.
fn sudo_replace(staged: &Path, current_exe: &Path) -> Result<(), String> {
    let backup = current_exe.with_file_name(format!(".flat-cyborg-old-{}", std::process::id()));
    eprintln!("Permission required - installing with sudo...");

    if !sudo_mv(current_exe, &backup) {
        return Err("sudo: could not move the current binary aside - update aborted".into());
    }
    if sudo_mv(staged, current_exe) {
        let _ = Command::new("sudo")
            .arg("rm")
            .arg("-f")
            .arg(&backup)
            .status();
        Ok(())
    } else {
        // Restore the original.
        let _ = sudo_mv(&backup, current_exe);
        Err("sudo: failed to install the new binary (original restored)".into())
    }
}

fn sudo_mv(from: &Path, to: &Path) -> bool {
    Command::new("sudo")
        .arg("mv")
        .arg("-f")
        .arg(from)
        .arg(to)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("failed to set permissions: {e}"))
}

// --- platform / version helpers -------------------------------------------

/// The GitHub release asset name for the running platform.
fn asset_name() -> Result<String, String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        other => return Err(format!("unsupported OS for self-update: {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("unsupported architecture for self-update: {other}")),
    };
    Ok(format!("flat-cyborg-{os}-{arch}"))
}

fn latest_tag() -> Result<String, String> {
    let json = fetch_text(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))?;
    parse_tag_name(&json).ok_or_else(|| "could not determine the latest release".into())
}

/// Extracts the `tag_name` value from a GitHub releases API JSON body.
fn parse_tag_name(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let after_key = &json[json.find(key)? + key.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let after_open = &after_colon[after_colon.find('"')? + 1..];
    let end = after_open.find('"')?;
    Some(after_open[..end].to_string())
}

/// Whether release tag `latest` is a newer version than `current`. Components
/// are compared left to right, treating a missing component as zero, so `1.0`
/// and `1.0.0` compare equal.
fn is_newer(latest: &str, current: &str) -> bool {
    let l = parse_version(latest);
    let c = parse_version(current);
    for i in 0..l.len().max(c.len()) {
        let a = l.get(i).copied().unwrap_or(0);
        let b = c.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

fn parse_version(s: &str) -> Vec<u64> {
    s.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| {
            // Drop any pre-release/build suffix (e.g. "1-rc2" -> 1).
            part.split(['-', '+'])
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

// --- subprocess helpers (curl/wget, sha256sum/shasum) ----------------------

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn fetch_text(url: &str) -> Result<String, String> {
    let output = if have("curl") {
        Command::new("curl").args(["-fsSL", url]).output()
    } else if have("wget") {
        Command::new("wget").args(["-qO-", url]).output()
    } else {
        return Err("curl or wget is required".into());
    };
    let output = output.map_err(|e| format!("failed to run downloader: {e}"))?;
    if !output.status.success() {
        return Err(format!("download failed: {url}"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 in response: {e}"))
}

fn fetch_to_file(url: &str, path: &Path) -> Result<(), String> {
    let status = if have("curl") {
        Command::new("curl")
            .arg("-fsSL")
            .arg("-o")
            .arg(path)
            .arg(url)
            .status()
    } else if have("wget") {
        Command::new("wget").arg("-qO").arg(path).arg(url).status()
    } else {
        return Err("curl or wget is required".into());
    };
    let status = status.map_err(|e| format!("failed to run downloader: {e}"))?;
    if !status.success() {
        return Err(format!("download failed: {url}"));
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(format!("download produced an empty file: {url}"));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Option<String> {
    let output = if have("sha256sum") {
        Command::new("sha256sum").arg(path).output().ok()?
    } else if have("shasum") {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
            .ok()?
    } else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.split_whitespace().next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tag_name() {
        let json = r#"{"url":"x","tag_name": "v1.2.3", "name":"v1.2.3"}"#;
        assert_eq!(parse_tag_name(json).as_deref(), Some("v1.2.3"));
        assert_eq!(parse_tag_name("{}"), None);
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        // Pre-release suffix on the patch is ignored.
        assert!(!is_newer("v0.1.0-rc1", "0.1.0"));
        // Differing component counts: missing components are treated as zero.
        assert!(!is_newer("v1.0", "1.0.0"));
        assert!(!is_newer("v1.0.0", "1.0"));
        assert!(is_newer("v1.0.1", "1.0"));
        assert!(is_newer("v1.1", "1.0.9"));
    }

    #[test]
    fn asset_name_matches_release_convention() {
        // On a supported CI host this resolves; otherwise it errors cleanly.
        match asset_name() {
            Ok(name) => {
                assert!(name.starts_with("flat-cyborg-"));
                assert!(
                    name.ends_with("x86_64") || name.ends_with("aarch64"),
                    "unexpected asset name: {name}"
                );
            }
            Err(e) => assert!(e.contains("unsupported")),
        }
    }
}
