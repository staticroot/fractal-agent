//! The client is a principal, not a transport: it raises the pkexec prompt itself,
//! because the prompt has to land in the user's session and the agent has none.

mod client;
mod edit;
mod render;

use clap::{Parser, Subcommand};
use fractal_protocol::config::Value;
use fractal_protocol::messages::{
    Endpoint, Payload, Request, Response, Revision, Solution,
};

#[derive(Parser)]
#[command(name = "fractal", version, about = "Configure a Fractal Linux system")]
struct Cli {
    /// Print the agent's answers as newline-delimited JSON instead of prose.
    ///
    /// One protocol `Response` per line, verbatim, in the order the agent sent
    /// them, so a chain like `apply` reads the same way the socket does.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build your draft, show what changes, obtain consent, and activate.
    ///
    /// Takes in your own draft and nothing else. What another principal drafted
    /// stays drafted, and the configuration history moves only if this succeeds.
    Apply {
        /// Message for the commit this creates.
        #[arg(short, long)]
        message: Option<String>,
        /// Skip the confirmation and go straight to the authorization prompt.
        #[arg(short, long)]
        yes: bool,
    },
    /// Read one option in every layer it has.
    Get { key: String },
    /// Draft a value for one option. The value is parsed as JSON, or taken as a
    /// string if it is not valid JSON.
    Set { key: String, value: String },
    /// Draft the removal of one option.
    Unset { key: String },
    /// Show what everyone has drafted, and who drafted it.
    Drafts,
    /// Discard your own draft. All of it, or the keys named.
    Discard { keys: Vec<String> },
    /// Read and edit the human-authored files.
    #[command(subcommand)]
    File(FileCommand),
    /// Check that the agent is reachable.
    Ping,
    /// Build what an apply would activate, without activating it.
    Build {
        #[arg(short, long)]
        message: Option<String>,
    },
    /// The options this device exposes.
    Catalog,
    /// Every generation, oldest first.
    History,
    /// The generation running now.
    Current,
    /// Compare two configurations. Each side is a generation number, a candidate
    /// commit, or `running`.
    Diff { from: String, to: String },
    /// Everything known about one generation.
    Evidence { generation: i64 },
    /// Return to an earlier generation, with fresh consent.
    Rollback { generation: i64 },
}

#[derive(Subcommand)]
enum FileCommand {
    /// Every file in the configuration.
    List {
        /// `committed`, `draft`, `draft:<uid>`, a generation number, or a commit.
        #[arg(long, default_value = "draft")]
        at: String,
    },
    /// Print one file.
    Read {
        path: String,
        /// `committed`, `draft`, `draft:<uid>`, a generation number, or a commit.
        #[arg(long, default_value = "draft")]
        at: String,
    },
    /// Open one file in your editor and land the result in your draft.
    Edit { path: String },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(&cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fractal: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: &Cli) -> Result<(), String> {
    match &cli.command {
        Command::Apply { message, yes } => apply(cli, message.clone(), *yes).await,
        Command::Rollback { generation } => rollback(cli, *generation).await,
        Command::Build { message } => {
            let built = build(cli, message.clone()).await?;
            show(cli, &built)
        }
        Command::File(FileCommand::Edit { path }) => edit::edit(cli.json, path).await,
        simple => {
            let answer = client::send(&request_for(simple)?).await?;
            show(cli, &answer)
        }
    }
}

fn request_for(command: &Command) -> Result<Request, String> {
    Ok(match command {
        Command::Get { key } => Request::GetOption { key: key.clone() },
        Command::Set { key, value } => Request::SetOption {
            key: key.clone(),
            value: parse_value(value),
        },
        Command::Unset { key } => Request::UnsetOption { key: key.clone() },
        Command::Drafts => Request::Drafts,
        Command::Discard { keys } => Request::Discard { keys: keys.clone() },
        Command::File(FileCommand::List { at }) => Request::ListFiles { at: parse_revision(at)? },
        Command::File(FileCommand::Read { path, at }) => Request::ReadFile {
            at: parse_revision(at)?,
            path: path.clone(),
        },
        Command::Ping => Request::Ping,
        Command::Catalog => Request::Catalog,
        Command::History => Request::History,
        Command::Current => Request::Current,
        Command::Evidence { generation } => Request::Evidence { generation: *generation },
        Command::Diff { from, to } => Request::Diff {
            from: parse_endpoint(from)?,
            to: parse_endpoint(to)?,
        },
        Command::Apply { .. }
        | Command::Rollback { .. }
        | Command::Build { .. }
        | Command::File(FileCommand::Edit { .. }) => unreachable!("handled by their own paths"),
    })
}

async fn apply(cli: &Cli, message: Option<String>, yes: bool) -> Result<(), String> {
    let built = build(cli, message).await?;
    let Response::Built { commit, .. } = &built else {
        return Err(format!("unexpected answer: {built:?}"));
    };
    let commit = commit.clone();
    step(cli, &built, || {})?;

    // Consent is shown against the closure about to be activated, not the drafts
    // view read earlier, so anything that arrived in between can still be refused.
    let diff = client::send(&Request::Diff {
        from: Endpoint::Running,
        to: Endpoint::Candidate { commit: commit.clone() },
    })
    .await;
    match &diff {
        Ok(answer @ Response::Diff(diff)) => {
            step(cli, answer, || eprintln!("\n{}", render::semantic(diff)))?
        }
        // The first activation has nothing to compare against.
        Err(e) => eprintln!("\n(cannot show what changes: {e})"),
        Ok(other) => return Err(format!("unexpected answer: {other:?}")),
    }

    if !yes && !confirm("Authorize this change?")? {
        return Err("cancelled".to_string());
    }
    activate(cli, Request::BeginActivation { commit: commit.clone() }, |nonce, sig| {
        Request::CompleteActivation {
            commit: commit.clone(),
            nonce,
            solution: Solution { signature: sig },
        }
    })
    .await
}

async fn rollback(cli: &Cli, generation: i64) -> Result<(), String> {
    activate(cli, Request::BeginRollback { generation }, move |nonce, sig| {
        Request::CompleteRollback {
            generation,
            nonce,
            solution: Solution { signature: sig },
        }
    })
    .await
}

async fn activate(
    cli: &Cli,
    begin: Request,
    complete: impl FnOnce(String, String) -> Request,
) -> Result<(), String> {
    let offered = client::send(&begin).await?;
    step(cli, &offered, || {})?;
    let Response::Challenge(challenge) = offered else {
        return Err(format!("unexpected answer: {offered:?}"));
    };
    let Payload::Activation { store_path, nonce } = challenge.payload else {
        return Err("the agent asked for a signature this client cannot obtain".to_string());
    };

    let signature = sign(&store_path, &nonce).await?;
    let answer = client::send(&complete(nonce, signature)).await?;
    show(cli, &answer)
}

// polkit matches its action against the path pkexec is handed, so this has to stay
// character-for-character the same as the lawyer's `exec.path` annotation. Anything
// else falls back to the generic prompt, which authorizes but describes nothing.
const LAWYER: &str = "/run/current-system/sw/bin/fractal-lawyer";

fn lawyer() -> String {
    std::env::var("FRACTAL_LAWYER").unwrap_or_else(|_| LAWYER.to_string())
}

async fn sign(store_path: &str, nonce: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("pkexec")
        .args([&lawyer(), "sign", "--kind", "activation"])
        .args(["--store", store_path, "--nonce", nonce])
        .output()
        .await
        .map_err(|e| format!("cannot run pkexec: {e}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            "authorization refused".to_string()
        } else {
            format!("authorization refused: {detail}")
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn build(cli: &Cli, message: Option<String>) -> Result<Response, String> {
    let drafts = client::send(&Request::Drafts).await?;
    let Response::Drafts { changes, quarantined } = &drafts else {
        return Err(format!("unexpected answer: {drafts:?}"));
    };
    step(cli, &drafts, || {
        if changes.is_empty() {
            eprintln!("Nothing drafted.");
        } else {
            eprintln!("{}\n", render::drafts(changes, quarantined));
        }
    })?;

    let quiet = cli.json;
    client::call(&Request::Build { message }, |line| {
        if !quiet {
            eprintln!("{line}");
        }
    })
    .await
}

fn show(cli: &Cli, answer: &Response) -> Result<(), String> {
    step(cli, answer, || {
        if let Some(text) = render::response(answer) {
            println!("{text}");
        }
    })
}

fn step(cli: &Cli, answer: &Response, prose: impl FnOnce()) -> Result<(), String> {
    if cli.json {
        println!("{}", serde_json::to_string(answer).map_err(|e| e.to_string())?);
    } else {
        prose();
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool, String> {
    use std::io::Write;
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn parse_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::Str(raw.to_string()))
}

fn parse_revision(raw: &str) -> Result<Revision, String> {
    if raw == "committed" || raw == "running" {
        return Ok(Revision::Commit { commit: None });
    }
    if raw == "draft" {
        return Ok(Revision::Draft { author: None });
    }
    if let Some(uid) = raw.strip_prefix("draft:") {
        let author = uid.parse().map_err(|_| format!("{uid} is not a uid"))?;
        return Ok(Revision::Draft { author: Some(author) });
    }
    if let Ok(id) = raw.parse::<i64>() {
        return Ok(Revision::Generation { id });
    }
    Ok(Revision::Commit { commit: Some(raw.to_string()) })
}

fn parse_endpoint(raw: &str) -> Result<Endpoint, String> {
    if raw == "running" {
        return Ok(Endpoint::Running);
    }
    if let Ok(id) = raw.parse::<i64>() {
        return Ok(Endpoint::Generation { id });
    }
    if is_commit(raw) {
        return Ok(Endpoint::Candidate { commit: raw.to_string() });
    }
    Err(format!("{raw} is not a generation number, a commit, or `running`"))
}

/// A full object id, which no generation number can be mistaken for: it is forty
/// hex digits, and forty digits do not fit an i64.
fn is_commit(raw: &str) -> bool {
    raw.len() == 40 && raw.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_reads_as_committed_a_draft_or_a_generation() {
        assert_eq!(parse_revision("committed").unwrap(), Revision::Commit { commit: None });
        assert_eq!(parse_revision("draft").unwrap(), Revision::Draft { author: None });
        assert_eq!(parse_revision("draft:1000").unwrap(), Revision::Draft { author: Some(1000) });
        assert_eq!(parse_revision("7").unwrap(), Revision::Generation { id: 7 });
        assert_eq!(
            parse_revision("refs/fractal/draft/1000").unwrap(),
            Revision::Commit { commit: Some("refs/fractal/draft/1000".into()) },
            "a reference is a revision like any other"
        );
        assert!(parse_revision("draft:alice").is_err());
    }
}
