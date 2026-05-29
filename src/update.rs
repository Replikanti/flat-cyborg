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
const ETXTBSY: i32 = 26;

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

    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;

    // Download next to the running binary so a later rename stays on the same
    // filesystem; fall back to the system temp dir if that directory is not
    // writable (the replace step then uses copy/sudo).
    let same_dir_tmp = current_exe.with_file_name(".flat-cyborg-update.tmp");
    let tmp = if fetch_to_file(&format!("{base}/{asset}"), &same_dir_tmp).is_ok() {
        same_dir_tmp
    } else {
        let fallback = std::env::temp_dir().join(".flat-cyborg-update.tmp");
        fetch_to_file(&format!("{base}/{asset}"), &fallback)?;
        fallback
    };

    if let Err(e) = verify_checksum(&base, &asset, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    set_executable(&tmp)?;

    if let Err(e) = replace_executable(&tmp, &current_exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    println!("Updated flat-cyborg to {latest}.");
    Ok(ExitCode::SUCCESS)
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

/// Atomically replaces `current_exe` with `tmp`, handling a running-binary
/// rename failure (Linux `ETXTBSY`) and falling back to `sudo`.
fn replace_executable(tmp: &Path, current_exe: &Path) -> Result<(), String> {
    match std::fs::rename(tmp, current_exe) {
        Ok(()) => return Ok(()),
        Err(e) if e.raw_os_error() == Some(ETXTBSY) || cross_device(&e) => {
            // A running ELF can't be renamed over; unlink first (the kernel
            // keeps the inode alive for the running process), then copy.
            if std::fs::remove_file(current_exe).is_ok() && std::fs::copy(tmp, current_exe).is_ok()
            {
                let _ = std::fs::remove_file(tmp);
                return Ok(());
            }
        }
        Err(_) => {}
    }

    // Permission or other failure: retry with sudo (rm + cp).
    eprintln!("Permission required - retrying with sudo...");
    let _ = Command::new("sudo")
        .arg("rm")
        .arg("-f")
        .arg(current_exe)
        .status();
    let status = Command::new("sudo")
        .arg("cp")
        .arg(tmp)
        .arg(current_exe)
        .status()
        .map_err(|e| format!("failed to run sudo: {e}"))?;
    let _ = std::fs::remove_file(tmp);
    if status.success() {
        Ok(())
    } else {
        Err("sudo cp failed - update aborted".into())
    }
}

fn cross_device(e: &std::io::Error) -> bool {
    // EXDEV (cross-device link) when tmp landed in a different filesystem.
    e.raw_os_error() == Some(18)
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

/// Whether release tag `latest` is a newer version than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    parse_version(latest) > parse_version(current)
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
