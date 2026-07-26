//! fractal-lawyer: the transient, privileged signing program. It is not a
//! resident daemon — a principal invokes it, through pkexec, for the rare,
//! human-paced moment of an activation. It does its one job and exits, so there
//! is no idle root process and no socket to defend, and the key stays at rest
//! under root except for the authorized moment.
//!
//! It does two things:
//!   keygen — mint the standalone Ed25519 keypair at install time.
//!   sign   — sign one typed activation payload with the root-held key.
//!
//! The lawyer raises no prompt of its own and checks nothing about intent.
//! Consent is the pkexec authorization the principal already had to pass to
//! launch it, rendered in the principal's own session; that the lawyer runs at
//! all is the proof a human consented. It only ever signs a message it builds
//! itself from typed fields, so it can never be turned into a raw signing oracle.

mod encoding;

use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;

use ed25519_dalek::{Signer, SigningKey};

fn default_key_path() -> String {
    std::env::var("FRACTAL_LAWYER_KEY")
        .unwrap_or_else(|_| "/var/lib/fractal-agent/keys/standalone.key".to_string())
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

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("keygen") => keygen(require(&args, "--private")?, require(&args, "--public")?),
        Some("sign") => {
            let nonce = require(&args, "--nonce")?;
            let message = match require(&args, "--kind")? {
                "activation" => encoding::activation_message(require(&args, "--store")?, nonce),
                "lock" => {
                    return Err("lock signatures are issued by the managed signer, not the lawyer".to_string());
                }
                other => return Err(format!("unknown --kind {other}")),
            };
            let key_path = flag(&args, "--key").map(String::from).unwrap_or_else(default_key_path);

            let key = load_key(&key_path)?;
            let sig = key.sign(&message);
            print!("{}", hex::encode(sig.to_bytes()));
            Ok(())
        }
        _ => Err("usage: fractal-lawyer (keygen --private P --public P | sign --kind activation --store S --nonce N [--key K])".to_string()),
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
