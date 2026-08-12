//! The client is a principal, not a transport: it raises the pkexec prompt itself,
//! because the prompt has to land in the user's session and the agent has none.

mod client;
mod render;

use clap::{Parser, Subcommand};
use fractal_protocol::config::Value;
use fractal_protocol::messages::{Endpoint, Payload, Request, Response, Solution};

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
    /// Commit, build, show what changes, obtain consent, and activate.
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
    /// Stage a value for one option. The value is parsed as JSON, or taken as a
    /// string if it is not valid JSON.
    Set {
        key: String,
        value: String,
        /// Take over a key another principal has staged.
        #[arg(long)]
        override_staged: bool,
    },
    /// Stage the removal of one option.
    Unset {
        key: String,
        #[arg(long)]
        override_staged: bool,
    },
    /// Show what is staged, and who staged it.
    Staged,
    /// Discard staged changes. Yours by default.
    Discard {
        /// Discard everybody's, not only your own.
        #[arg(long)]
        all: bool,
    },
    /// Commit the staged changes without building or activating.
    Commit {
        #[arg(short, long)]
        message: Option<String>,
        /// The fingerprint from `fractal staged`. Commits exactly that and is
        /// refused if anyone staged anything since.
        #[arg(long)]
        expect: Option<String>,
    },
    /// Check that the agent is reachable.
    Ping,
    /// Build the committed configuration.
    Build,
    /// The options this device exposes.
    Catalog,
    /// Every generation, oldest first.
    History,
    /// The generation running now.
    Current,
    /// Compare two configurations. Each side is a generation number, a store
    /// path, or `running`.
    Diff { from: String, to: String },
    /// Everything known about one generation.
    Evidence { generation: i64 },
    /// Return to an earlier generation, with fresh consent.
    Rollback { generation: i64 },
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
        Command::Build => {
            let built = build(cli).await?;
            show(cli, &built)
        }
        simple => {
            let answer = client::send(&request_for(simple)?).await?;
            show(cli, &answer)
        }
    }
}

fn request_for(command: &Command) -> Result<Request, String> {
    Ok(match command {
        Command::Get { key } => Request::GetOption { key: key.clone() },
        Command::Set { key, value, override_staged } => Request::SetOption {
            key: key.clone(),
            value: parse_value(value),
            override_staged: *override_staged,
        },
        Command::Unset { key, override_staged } => Request::UnsetOption {
            key: key.clone(),
            override_staged: *override_staged,
        },
        Command::Staged => Request::StagedDiff,
        Command::Discard { all } => Request::Discard { all: *all },
        Command::Commit { message, expect } => Request::Commit {
            message: message.clone(),
            expect: expect.clone(),
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
        Command::Apply { .. } | Command::Rollback { .. } | Command::Build => {
            unreachable!("handled by their own paths")
        }
    })
}

async fn apply(cli: &Cli, message: Option<String>, yes: bool) -> Result<(), String> {
    let staged = client::send(&Request::StagedDiff).await?;
    let (changes, fingerprint) = match &staged {
        Response::StagedDiff { changes, fingerprint } => (changes.clone(), fingerprint.clone()),
        other => return Err(format!("unexpected answer: {other:?}")),
    };
    step(cli, &staged, || {
        if changes.is_empty() {
            eprintln!("Nothing staged.");
        } else {
            eprintln!("{}\n", render::staged(&changes));
        }
    })?;

    let committed = client::send(&Request::Commit {
        message,
        expect: Some(fingerprint),
    })
    .await?;
    step(cli, &committed, || {
        if let Response::Committed { commit: Some(hash) } = &committed {
            eprintln!("Committed {}.", render::short(hash));
        }
    })?;

    let built = build(cli).await?;
    let store_path = match &built {
        Response::Built { store_path, .. } => store_path.clone(),
        other => return Err(format!("unexpected answer: {other:?}")),
    };
    step(cli, &built, || {})?;

    // Consent is shown against the closure about to be activated, not the staged
    // view read earlier, so anything that arrived in between can still be refused.
    let diff = client::send(&Request::Diff {
        from: Endpoint::Running,
        to: Endpoint::Build { store_path: store_path.clone() },
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
    activate(cli, Request::BeginActivation { store_path: store_path.clone() }, |nonce, sig| {
        Request::CompleteActivation {
            store_path: store_path.clone(),
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

// An absolute path: polkit matches an action by the path pkexec was handed, and a
// name resolved through PATH matches none, falling back to the generic prompt.
fn lawyer() -> String {
    std::env::var("FRACTAL_LAWYER").unwrap_or_else(|_| "fractal-lawyer".to_string())
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

async fn build(cli: &Cli) -> Result<Response, String> {
    let quiet = cli.json;
    client::call(&Request::Build, |line| {
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

fn parse_endpoint(raw: &str) -> Result<Endpoint, String> {
    if raw == "running" {
        return Ok(Endpoint::Running);
    }
    if let Ok(id) = raw.parse::<i64>() {
        return Ok(Endpoint::Generation { id });
    }
    if raw.starts_with("/nix/store/") {
        return Ok(Endpoint::Build { store_path: raw.to_string() });
    }
    Err(format!("{raw} is not a generation number, a store path, or `running`"))
}
