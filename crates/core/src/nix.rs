//! Evaluate and build the configuration with Lix, shelling out to `nix`. Reads
//! go through evaluation — the authoritative, typed reader — never by parsing the
//! generated file back. Building streams its output line by line so the agent
//! can relay progress; the pre-apply difference leans on Nix's own closure
//! comparison and dry-run reporting rather than a custom closure representation.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::Value;
use crate::diff::ClosureDiff;
use crate::error::{Error, Result};

/// The `nix` binary (Lix provides it). Overridable for tests or unusual installs.
fn nix_bin() -> String {
    std::env::var("FRACTAL_NIX_BIN").unwrap_or_else(|_| "nix".to_string())
}

/// No working directory: for operations that don't touch a flake.
fn nix_cmd() -> Command {
    let mut cmd = Command::new(nix_bin());
    cmd.args(["--extra-experimental-features", "nix-command flakes"]);
    cmd
}

fn base(dir: &Path) -> Command {
    let mut cmd = nix_cmd();
    cmd.current_dir(dir);
    cmd
}

fn attr(host: &str, suffix: &str) -> String {
    format!(".#nixosConfigurations.{host}.{suffix}")
}

/// Evaluate one option to its resolved, typed value: the authoritative reader.
pub fn eval_option(dir: &Path, host: &str, option: &str) -> Result<Value> {
    let mut cmd = base(dir);
    cmd.args(["eval", &attr(host, &format!("config.{option}")), "--json"]);
    let out = cmd.output().map_err(|e| Error::io(dir, e))?;
    if !out.status.success() {
        return Err(Error::Nix(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Evaluate a raw expression against the flake to JSON. Used to pull many option
/// values or option metadata in one shot.
pub fn eval_expr(dir: &Path, expr: &str) -> Result<serde_json::Value> {
    let mut cmd = base(dir);
    cmd.args(["eval", "--impure", "--json", "--expr", expr]);
    let out = cmd.output().map_err(|e| Error::io(dir, e))?;
    if !out.status.success() {
        return Err(Error::Nix(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Build the system closure and return its store path, streaming every build-log
/// line to `on_line` as it happens. Progress is on stderr; the out path is on
/// stdout.
pub fn build_system(
    dir: &Path,
    host: &str,
    mut on_line: impl FnMut(&str),
) -> Result<String> {
    let mut child = base(dir)
        .args([
            "build",
            &attr(host, "config.system.build.toplevel"),
            "--print-out-paths",
            "--no-link",
            "-L",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::io(dir, e))?;

    // Capture stdout (the out path) on a side thread while we stream stderr, so
    // neither pipe can fill and deadlock the child.
    let stdout = child.stdout.take();
    let out_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_string(&mut buf);
        }
        buf
    });

    if let Some(err) = child.stderr.take() {
        use std::io::BufRead;
        for line in std::io::BufReader::new(err).lines().map_while(std::result::Result::ok) {
            on_line(&line);
        }
    }

    let status = child.wait().map_err(|e| Error::io(dir, e))?;
    let stdout = out_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::Nix("nix build failed".to_string()));
    }
    last_out_path(&stdout)
        .map(str::to_string)
        .ok_or_else(|| Error::Nix("nix build produced no out path".to_string()))
}

/// Closure difference between two store paths (package/version deltas), from
/// Nix's own `diff-closures` in its JSON form.
pub fn diff_closures(before: &str, after: &str) -> Result<ClosureDiff> {
    let out = nix_cmd()
        .args(["store", "diff-closures", "--json", before, after])
        .output()
        .map_err(|e| Error::Nix(e.to_string()))?;
    if !out.status.success() {
        return Err(Error::Nix(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Format `file` in place so the generated module reads like the hand-authored
/// ones beside it. Uses the config flake's own formatter (`nix fmt`), pinned by
/// its lock so output stays deterministic, or the command in
/// `FRACTAL_NIX_FORMATTER` (program and args; the path is appended) if set.
/// Purely cosmetic: the serializer already emits valid Nix, so callers may treat
/// a failure here as non-fatal.
pub fn format_file(dir: &Path, file: &Path) -> Result<()> {
    let mut cmd = match std::env::var("FRACTAL_NIX_FORMATTER") {
        Ok(custom) => {
            let mut parts = custom.split_whitespace();
            let prog = parts.next().ok_or_else(|| Error::Nix("FRACTAL_NIX_FORMATTER is empty".into()))?;
            let mut c = Command::new(prog);
            c.args(parts).current_dir(dir);
            c
        }
        Err(_) => {
            let mut c = base(dir);
            c.args(["fmt", "--"]);
            c
        }
    };
    let out = cmd.arg(file).output().map_err(|e| Error::io(file, e))?;
    if !out.status.success() {
        return Err(Error::Nix(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(())
}

/// The last non-empty line of `nix build --print-out-paths` output.
fn last_out_path(stdout: &str) -> Option<&str> {
    stdout.lines().map(str::trim).rev().find(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_last_out_path() {
        assert_eq!(
            last_out_path("/nix/store/aaa-x\n/nix/store/bbb-system\n"),
            Some("/nix/store/bbb-system")
        );
        assert_eq!(last_out_path("  /nix/store/only  \n\n"), Some("/nix/store/only"));
        assert_eq!(last_out_path("\n\n"), None);
    }

    #[test]
    fn parses_diff_closures_json() {
        let json = r#"{"packages":{
            "hello":{"sizeDelta":226504,"versionsBefore":["2.12"],"versionsAfter":["2.12.1"]},
            "removed-pkg":{"sizeDelta":-1024,"versionsBefore":["1.0"],"versionsAfter":[]}
        }}"#;
        let diff: ClosureDiff = serde_json::from_str(json).unwrap();
        let hello = &diff.packages["hello"];
        assert_eq!(hello.size_delta, 226504);
        assert_eq!(hello.versions_before, ["2.12"]);
        assert_eq!(hello.versions_after, ["2.12.1"]);
        assert_eq!(diff.packages["removed-pkg"].size_delta, -1024);
        assert!(diff.packages["removed-pkg"].versions_after.is_empty());
    }

    #[test]
    fn attr_shape() {
        assert_eq!(
            attr("box", "config.system.build.toplevel"),
            ".#nixosConfigurations.box.config.system.build.toplevel"
        );
    }
}
