//! Editing a human-authored file: read it at the caller's own draft, open it in
//! their editor, and land the result. The repository is bare, so there is no
//! checkout to edit and this session is the whole editing mechanism.

use std::io::Write;

use fractal_protocol::messages::{Request, Response, Revision};

use crate::client;

pub async fn edit(json: bool, path: &str) -> Result<(), String> {
    let (mut contents, digest) = read(path).await?;
    let file = tempfile::Builder::new()
        .prefix("fractal-")
        .suffix(&suffix(path))
        .tempfile()
        .map_err(|e| e.to_string())?;

    loop {
        std::fs::write(file.path(), &contents).map_err(|e| e.to_string())?;
        open(file.path())?;
        contents = std::fs::read_to_string(file.path()).map_err(|e| e.to_string())?;

        let request = Request::WriteFile {
            path: path.to_string(),
            contents: contents.clone(),
            base_digest: digest.clone(),
        };
        match client::send(&request).await {
            Ok(answer) => {
                if json {
                    println!("{}", serde_json::to_string(&answer).map_err(|e| e.to_string())?);
                }
                return Ok(());
            }
            // Reopened on the user's own text rather than discarded, the way
            // visudo does it: the refusal is about what they wrote, and they are
            // the only one who can repair it.
            Err(message) => {
                eprintln!("fractal: {message}");
                if !retry()? {
                    return Err("not submitted".to_string());
                }
            }
        }
    }
}

/// An absent file is editable as an empty one, with the empty digest saying so.
async fn read(path: &str) -> Result<(String, String), String> {
    let request = Request::ReadFile {
        at: Revision::Draft { author: None },
        path: path.to_string(),
    };
    match client::send(&request).await {
        Ok(Response::FileContents { contents, digest }) => Ok((contents, digest)),
        Ok(other) => Err(format!("unexpected answer: {other:?}")),
        Err(_) => Ok((String::new(), String::new())),
    }
}

fn suffix(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((_, extension)) => format!(".{extension}"),
        None => String::new(),
    }
}

fn open(file: &std::path::Path) -> Result<(), String> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().ok_or_else(|| "EDITOR is empty".to_string())?;
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(file)
        .status()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(())
}

fn retry() -> Result<bool, String> {
    eprint!("Edit again? [Y/n] ");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    // No answer at all, rather than an empty line, means nobody is there to give
    // one. Defaulting to yes there would reopen the editor forever.
    if std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())? == 0 {
        return Ok(false);
    }
    Ok(!matches!(answer.trim(), "n" | "N" | "no"))
}
