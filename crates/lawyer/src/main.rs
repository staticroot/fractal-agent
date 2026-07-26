//! fractal-lawyer: the transient, privileged signing program. It is not a
//! resident daemon — the agent invokes it for the rare, human-paced moment of an
//! activation, it does its one job, and it exits, so there is no idle root
//! process and no socket to defend, and the key stays at rest under root except
//! for the authorized moment.
//!
//! It does two things:
//!   keygen — mint the standalone Ed25519 keypair at install time.
//!   sign   — raise the administrator prompt for a specific activation and, only
//!            on consent, sign the store path and nonce with the root-held key.
//!
//! The prompt is a polkit authorization checked against the *requesting human's*
//! process, conveyed by the agent as a (pid, start-time) reference that pid
//! reuse cannot forge. The signing program checks nothing about intent: the
//! administrator's consent is the authorization.

mod encoding;

use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;

use ed25519_dalek::{Signer, SigningKey};
use zbus::zvariant::{Type, Value};
use zbus::Connection;

/// polkit action the administrator authorizes to sign one activation.
const ACTION_SIGN: &str = "systems.staticroot.agent.sign";
/// AllowUserInteraction — this is the call that raises the prompt.
const FLAG_INTERACTIVE: u32 = 1;

fn default_key_path() -> String {
    std::env::var("FRACTAL_LAWYER_KEY")
        .unwrap_or_else(|_| "/var/lib/fractal-agent/keys/standalone.key".to_string())
}

// --- polkit -----------------------------------------------------------------

#[derive(serde::Serialize, Type)]
struct Subject<'a> {
    kind: &'a str,
    details: HashMap<&'a str, Value<'a>>,
}

#[derive(serde::Deserialize, Type)]
struct AuthResult {
    is_authorized: bool,
    #[allow(dead_code)]
    is_challenge: bool,
    #[allow(dead_code)]
    details: HashMap<String, String>,
}

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    fn check_authorization(
        &self,
        subject: &Subject<'_>,
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<AuthResult>;
}

/// Raise the administrator prompt for `pid`/`start_time`, passing `metadata` for
/// the polkit message (which generation, what changes, download size). Returns
/// whether the human consented.
async fn consented(
    pid: u32,
    start_time: u64,
    metadata: &HashMap<String, String>,
) -> Result<bool, String> {
    let conn = Connection::system().await.map_err(|e| e.to_string())?;
    let authority = AuthorityProxy::new(&conn).await.map_err(|e| e.to_string())?;

    // A unix-process subject bound by start-time: if the pid was reused, the
    // start-time will not match and polkit rejects it.
    let mut subject_details = HashMap::new();
    subject_details.insert("pid", Value::U32(pid));
    subject_details.insert("start-time", Value::U64(start_time));
    let subject = Subject {
        kind: "unix-process",
        details: subject_details,
    };

    let detail_refs: HashMap<&str, &str> = metadata
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let result = authority
        .check_authorization(&subject, ACTION_SIGN, detail_refs, FLAG_INTERACTIVE, "")
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.is_authorized)
}

// --- signing ----------------------------------------------------------------

fn load_key(path: &str) -> Result<SigningKey, String> {
    let hexed = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read signing key {path}: {e}"))?;
    let bytes = hex::decode(hexed.trim()).map_err(|_| "signing key is not valid hex".to_string())?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "signing key is not 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&seed))
}

fn keygen(private_path: &str, public_path: &str) -> Result<(), String> {
    let seed: [u8; 32] = rand::random();
    let key = SigningKey::from_bytes(&seed);

    if let Some(parent) = std::path::Path::new(private_path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Root-only: the private key never leaves rest except for the signer itself.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    {
        use std::io::Write;
        let mut f = opts.open(private_path).map_err(|e| e.to_string())?;
        f.write_all(hex::encode(seed).as_bytes()).map_err(|e| e.to_string())?;
    }

    std::fs::write(public_path, format!("{}\n", hex::encode(key.verifying_key().to_bytes())))
        .map_err(|e| e.to_string())?;
    Ok(())
}

// --- argument handling ------------------------------------------------------

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn require<'a>(args: &'a [String], name: &str) -> Result<&'a str, String> {
    flag(args, name).ok_or_else(|| format!("missing required {name}"))
}

/// Collect repeated `--detail key=value` into polkit message substitutions.
fn details(args: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (i, a) in args.iter().enumerate() {
        if a == "--detail"
            && let Some(kv) = args.get(i + 1)
            && let Some((k, v)) = kv.split_once('=')
        {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("keygen") => keygen(require(&args, "--private")?, require(&args, "--public")?),
        Some("sign") => {
            let store = require(&args, "--store")?;
            let nonce = require(&args, "--nonce")?;
            let pid: u32 = require(&args, "--pid")?.parse().map_err(|_| "bad --pid")?;
            let start_time: u64 = require(&args, "--start-time")?
                .parse()
                .map_err(|_| "bad --start-time")?;
            let key_path = flag(&args, "--key").map(String::from).unwrap_or_else(default_key_path);
            let metadata = details(&args);

            let authorized = async_io::block_on(consented(pid, start_time, &metadata))?;
            if !authorized {
                return Err("administrator did not authorize the signature".to_string());
            }

            // Consent given: load the key, sign, print the signature, exit.
            let key = load_key(&key_path)?;
            let sig = key.sign(&encoding::activation_message(store, nonce));
            print!("{}", hex::encode(sig.to_bytes()));
            Ok(())
        }
        _ => Err("usage: fractal-lawyer (keygen --private P --public P | sign --store S --nonce N --pid PID --start-time T [--key K] [--detail k=v]...)".to_string()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fractal-lawyer: {e}");
            ExitCode::FAILURE
        }
    }
}
