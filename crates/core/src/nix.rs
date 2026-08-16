//! Evaluate and build the configuration with Lix, shelling out to `nix`. Reads
//! go through evaluation — the authoritative, typed reader — never by parsing the
//! generated file back. Building streams its output line by line so the agent
//! can relay progress; the pre-apply difference leans on Nix's own closure
//! comparison and dry-run reporting rather than a custom closure representation.
//!
//! Nothing here takes a working directory. The configuration repository is bare,
//! so every flake operation names a revision instead.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::diff::ClosureDiff;
use crate::error::{Error, Result};

/// The `nix` binary (Lix provides it). Overridable for tests or unusual installs.
fn nix_bin() -> String {
    std::env::var("FRACTAL_NIX_BIN").unwrap_or_else(|_| "nix".to_string())
}

/// Parsing has no `nix` subcommand, so this is a second binary rather than a
/// second argument. Overridable for the same reason.
fn nix_instantiate_bin() -> String {
    std::env::var("FRACTAL_NIX_INSTANTIATE_BIN").unwrap_or_else(|_| "nix-instantiate".to_string())
}

fn nix_cmd() -> Command {
    let mut cmd = Command::new(nix_bin());
    cmd.args(["--extra-experimental-features", "nix-command flakes"]);
    cmd
}

fn output(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd.output().map_err(|e| Error::Nix(e.to_string()))?;
    if !out.status.success() {
        return Err(Error::Nix(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(out.stdout)
}

/// Evaluate a raw expression to JSON. Used to pull many option values or option
/// metadata in one shot.
pub fn eval_expr(expr: &str) -> Result<serde_json::Value> {
    let mut cmd = nix_cmd();
    cmd.args(["eval", "--impure", "--json", "--expr", expr]);
    Ok(serde_json::from_slice(&output(cmd)?)?)
}

/// Evaluate the generated module's source to the nested attrset it defines, as
/// JSON. The module is a `{ ... }: <plain data>` function, so it is applied to
/// the empty attrset and needs no flake inputs, which keeps reading a draft's
/// model cheap and offline. Pairs with [`crate::config::Model::from_eval_json`],
/// which turns the result back into a model.
pub fn eval_module_source(src: &str) -> Result<serde_json::Value> {
    let mut cmd = nix_cmd();
    cmd.args(["eval", "--json", "--expr", &module_expr(src)]);
    Ok(serde_json::from_slice(&output(cmd)?)?)
}

/// Apply the module function to the empty attrset so evaluation yields its body.
fn module_expr(src: &str) -> String {
    format!("({src}) {{ }}")
}

/// The configuration flake at one revision, as a URL.
///
/// A reference has to be named beside the revision: Lix will not fetch a commit
/// no reference covers, which is why a draft and a candidate each have one. The
/// reference is not enough on its own either, since it moves.
pub fn flake_url(dir: &Path, reference: &str, rev: &str) -> String {
    format!("git+file://{}?ref={reference}&rev={rev}", dir.display())
}

/// Build one flake installable and return its store path, streaming every
/// build-log line to `on_line` as it happens. Progress is on stderr; the out
/// path is on stdout.
///
/// `out_link` registers an indirect garbage-collection root at that path, so the
/// closure survives a collection between being built and being used. Passing
/// `None` builds without a root, for callers that only want the path.
///
/// This does not know what it is building: which attribute holds a system rather
/// than a home is authority wiring and lives with that authority.
pub fn build_attr(
    installable: &str,
    out_link: Option<&Path>,
    mut on_line: impl FnMut(&str),
) -> Result<String> {
    let mut cmd = nix_cmd();
    cmd.args(["build", installable, "--print-out-paths", "-L", "--no-write-lock-file"]);
    match out_link {
        Some(link) => {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            cmd.arg("--out-link").arg(link);
        }
        None => {
            cmd.arg("--no-link");
        }
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Nix(e.to_string()))?;

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

    let status = child.wait().map_err(|e| Error::Nix(e.to_string()))?;
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
    let mut cmd = nix_cmd();
    cmd.args(["store", "diff-closures", "--json", before, after]);
    Ok(serde_json::from_slice(&output(cmd)?)?)
}

/// Drop a source tree an evaluation fetched and nothing needs any more. Callers
/// treat a failure as cosmetic: `nix.gc.automatic` is the backstop, so a store
/// that refuses the deletion costs disk rather than correctness.
pub fn store_delete(path: &str) -> Result<()> {
    let mut cmd = nix_cmd();
    cmd.args(["store", "delete", path]);
    output(cmd).map(|_| ())
}

/// Whether `bytes` is a Nix expression the evaluator will accept, so a file that
/// cannot parse bounces back to its author rather than taking their own reads
/// down. Parse only: a module that parses may still fail to evaluate, and
/// evaluating one file out of a flake is not a thing that can be done.
pub fn parse_check(bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new(nix_instantiate_bin())
        .args(["--parse", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Nix(e.to_string()))?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(bytes)
        .map_err(|e| Error::Nix(e.to_string()))?;
    let out = child.wait_with_output().map_err(|e| Error::Nix(e.to_string()))?;
    if !out.status.success() {
        return Err(Error::Nix(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(())
}

/// Run the configuration flake's own pinned formatter over `bytes` and return
/// what it made of them, so the generated module reads like the hand-authored
/// ones beside it. `FRACTAL_NIX_FORMATTER` (program and args; the path is
/// appended) overrides the flake's.
///
/// Purely cosmetic, so callers treat a failure as non-fatal. It takes bytes
/// rather than a path because the repository is bare and there is no file to
/// format in place.
pub fn format_bytes(flake: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let file = tempfile::Builder::new()
        .suffix(".nix")
        .tempfile()
        .map_err(|e| Error::Nix(e.to_string()))?;
    std::fs::write(file.path(), bytes).map_err(|e| Error::io(file.path(), e))?;

    let mut cmd = match std::env::var("FRACTAL_NIX_FORMATTER") {
        Ok(custom) => {
            let mut parts = custom.split_whitespace();
            let prog = parts
                .next()
                .ok_or_else(|| Error::Nix("FRACTAL_NIX_FORMATTER is empty".into()))?;
            let mut c = Command::new(prog);
            c.args(parts);
            c
        }
        Err(_) => {
            let mut c = nix_cmd();
            c.args(["run", &format!("{flake}#formatter"), "--"]);
            c
        }
    };
    cmd.arg(file.path());
    output(cmd)?;
    std::fs::read(file.path()).map_err(|e| Error::io(file.path(), e))
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
    fn module_expr_applies_to_empty_attrs() {
        assert_eq!(module_expr("{ ... }: { x = 1; }"), "({ ... }: { x = 1; }) { }");
    }

    /// Both halves are named, because a revision alone is not fetchable and a
    /// reference alone moves.
    #[test]
    fn a_flake_url_names_a_reference_and_a_revision() {
        assert_eq!(
            flake_url(Path::new("/var/lib/fractal-agent/system-config"), "refs/fractal/draft/1000", "abc"),
            "git+file:///var/lib/fractal-agent/system-config?ref=refs/fractal/draft/1000&rev=abc"
        );
    }

    /// Skipped where Nix is absent, since the rest of the suite has no such
    /// dependency.
    #[test]
    fn a_parse_error_is_reported_and_valid_source_is_not() {
        match parse_check(b"{ ... }: { x = 1; }\n") {
            Ok(()) => {}
            Err(Error::Nix(e)) if e.contains("No such file") => return,
            Err(e) => panic!("{e}"),
        }
        assert!(parse_check(b"{ x = ; }").is_err());
    }
}
