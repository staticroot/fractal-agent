//! The record of every system generation the device has activated. Enough is
//! persisted to reconstruct history and to roll back without rebuilding: the
//! store path stays referenced so an earlier generation can be re-signed and
//! re-activated directly.
//!
//! Three fields are subtler than they look. `parent_id` is the *activation*
//! lineage — what this generation descended from as activated — which diverges
//! from git parentage on a rollback or a failed apply. The log fields hold only
//! a path, a size, and a tail: the full build and activation output lives in
//! files, not in the database. And `verifying_key` is the trigger's own answer
//! to "who authorized this", returned from the switch call: the signature is the
//! consent, and the key names the authority that gave it. There is deliberately
//! no consent record, because the prompt happens in the principal's session
//! where the agent cannot see it, so anything written here about a human having
//! approved would be a claim dressed as a fact.

use std::path::Path;

use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A forward change: staged edits applied.
    Forward,
    /// A return to an earlier generation's store path.
    Rollback,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Forward => "forward",
            Kind::Rollback => "rollback",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "forward" => Ok(Kind::Forward),
            "rollback" => Ok(Kind::Rollback),
            other => Err(crate::error::Error::Other(format!("unknown kind {other:?}"))),
        }
    }
}

/// How the activation turned out. `Failed` carries the reason as it happened —
/// an event fact that cannot be recomputed later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failed { detail: String },
}

/// A captured build or activation log: where the full output lives, how big it
/// is, and the last slice for a quick glance without opening the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRef {
    pub path: String,
    pub size: u64,
    pub tail: String,
}

/// The facts known at the moment a generation is recorded.
#[derive(Debug, Clone)]
pub struct NewGeneration {
    pub store_path: String,
    pub config_commit: String,
    pub parent_id: Option<i64>,
    pub kind: Kind,
    pub description: String,
    pub actor: String,
    pub verifying_key: String,
    pub signature: String,
    pub burned_nonce: String,
    pub outcome: Outcome,
    /// Empty in standalone; the seam managed mode and compliance work will use.
    pub policy_version: Option<String>,
    pub build_log: Option<LogRef>,
    pub activation_log: Option<LogRef>,
}

/// A stored generation: a `NewGeneration` plus the identity and timestamp the
/// store assigned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    pub id: i64,
    pub timestamp: Timestamp,
    pub store_path: String,
    pub config_commit: String,
    pub parent_id: Option<i64>,
    pub kind: Kind,
    pub description: String,
    pub actor: String,
    pub verifying_key: String,
    pub signature: String,
    pub burned_nonce: String,
    pub outcome: Outcome,
    pub policy_version: Option<String>,
    pub build_log: Option<LogRef>,
    pub activation_log: Option<LogRef>,
}

pub struct Generations {
    conn: Connection,
}

impl Generations {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record a generation and return its assigned id. The timestamp is taken
    /// now, as the witnessed moment of activation.
    pub fn record(&self, rec: &NewGeneration) -> Result<i64> {
        let (outcome, outcome_detail) = match &rec.outcome {
            Outcome::Success => ("success", None),
            Outcome::Failed { detail } => ("failed", Some(detail.as_str())),
        };
        let (blp, bls, blt) = split_log(&rec.build_log);
        let (alp, als, alt) = split_log(&rec.activation_log);

        self.conn.execute(
            "INSERT INTO generations (
                timestamp, store_path, config_commit, parent_id, kind, description,
                actor, verifying_key, signature, burned_nonce, outcome, outcome_detail,
                policy_version, build_log_path, build_log_size, build_log_tail,
                activation_log_path, activation_log_size, activation_log_tail
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
             )",
            rusqlite::params![
                Timestamp::now().to_string(),
                rec.store_path,
                rec.config_commit,
                rec.parent_id,
                rec.kind.as_str(),
                rec.description,
                rec.actor,
                rec.verifying_key,
                rec.signature,
                rec.burned_nonce,
                outcome,
                outcome_detail,
                rec.policy_version,
                blp,
                bls,
                blt,
                alp,
                als,
                alt,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Generation>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM generations WHERE id = ?1"),
                [id],
                row_to_generation,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }

    /// All generations, oldest first.
    pub fn list(&self) -> Result<Vec<Generation>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM generations ORDER BY id ASC"))?;
        let rows = stmt.query_map([], row_to_generation)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether any generation ever activated this closure. A membership test, so
    /// it asks the database rather than loading every row to look through them.
    pub fn has_store_path(&self, store_path: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM generations WHERE store_path = ?1 LIMIT 1",
                [store_path],
                |row| row.get(0),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(found.is_some())
    }

    /// The most recent successfully activated generation — the one running now.
    pub fn latest_success(&self) -> Result<Option<Generation>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM generations WHERE outcome = 'success' ORDER BY id DESC LIMIT 1"
                ),
                [],
                row_to_generation,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }
}

fn split_log(log: &Option<LogRef>) -> (Option<String>, Option<i64>, Option<String>) {
    match log {
        Some(l) => (Some(l.path.clone()), Some(l.size as i64), Some(l.tail.clone())),
        None => (None, None, None),
    }
}

fn join_log(path: Option<String>, size: Option<i64>, tail: Option<String>) -> Option<LogRef> {
    match (path, size, tail) {
        (Some(path), Some(size), Some(tail)) => Some(LogRef {
            path,
            size: size as u64,
            tail,
        }),
        _ => None,
    }
}

const COLUMNS: &str = "id, timestamp, store_path, config_commit, parent_id, kind, description, \
     actor, verifying_key, signature, burned_nonce, outcome, outcome_detail, policy_version, \
     build_log_path, build_log_size, build_log_tail, \
     activation_log_path, activation_log_size, activation_log_tail";

fn row_to_generation(row: &Row) -> rusqlite::Result<Generation> {
    let ts: String = row.get(1)?;
    let outcome_status: String = row.get(11)?;
    let outcome_detail: Option<String> = row.get(12)?;
    let outcome = match outcome_status.as_str() {
        "success" => Outcome::Success,
        _ => Outcome::Failed {
            detail: outcome_detail.unwrap_or_default(),
        },
    };
    let kind_str: String = row.get(5)?;

    Ok(Generation {
        id: row.get(0)?,
        timestamp: ts.parse().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?,
        store_path: row.get(2)?,
        config_commit: row.get(3)?,
        parent_id: row.get(4)?,
        kind: Kind::parse(&kind_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        description: row.get(6)?,
        actor: row.get(7)?,
        verifying_key: row.get(8)?,
        signature: row.get(9)?,
        burned_nonce: row.get(10)?,
        outcome,
        policy_version: row.get(13)?,
        build_log: join_log(row.get(14)?, row.get(15)?, row.get(16)?),
        activation_log: join_log(row.get(17)?, row.get(18)?, row.get(19)?),
    })
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS generations (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp           TEXT    NOT NULL,
    store_path          TEXT    NOT NULL,
    config_commit       TEXT    NOT NULL,
    parent_id           INTEGER REFERENCES generations(id),
    kind                TEXT    NOT NULL,
    description         TEXT    NOT NULL,
    actor               TEXT    NOT NULL,
    verifying_key       TEXT    NOT NULL,
    signature           TEXT    NOT NULL,
    burned_nonce        TEXT    NOT NULL,
    outcome             TEXT    NOT NULL,
    outcome_detail      TEXT,
    policy_version      TEXT,
    build_log_path      TEXT,
    build_log_size      INTEGER,
    build_log_tail      TEXT,
    activation_log_path TEXT,
    activation_log_size INTEGER,
    activation_log_tail TEXT
);
CREATE INDEX IF NOT EXISTS generations_store_path ON generations (store_path);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(kind: Kind, parent: Option<i64>, outcome: Outcome) -> NewGeneration {
        NewGeneration {
            store_path: "/nix/store/aaa-system".into(),
            config_commit: "abc123".into(),
            parent_id: parent,
            kind,
            description: "turn on firewall".into(),
            actor: "alice".into(),
            verifying_key: "aa".repeat(32),
            signature: "deadbeef".into(),
            burned_nonce: "cafe".into(),
            outcome,
            policy_version: None,
            build_log: Some(LogRef {
                path: "/var/lib/fractal-agent/logs/1-build.log".into(),
                size: 42,
                tail: "done".into(),
            }),
            activation_log: None,
        }
    }

    #[test]
    fn records_and_reads_back() {
        let g = Generations::in_memory().unwrap();
        let id = g.record(&sample(Kind::Forward, None, Outcome::Success)).unwrap();
        let got = g.get(id).unwrap().unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.kind, Kind::Forward);
        assert_eq!(got.outcome, Outcome::Success);
        assert_eq!(got.build_log.unwrap().size, 42);
        assert!(got.activation_log.is_none());
    }

    #[test]
    fn latest_success_ignores_failures() {
        let g = Generations::in_memory().unwrap();
        let ok = g.record(&sample(Kind::Forward, None, Outcome::Success)).unwrap();
        g.record(&sample(
            Kind::Forward,
            Some(ok),
            Outcome::Failed { detail: "build broke".into() },
        ))
        .unwrap();
        assert_eq!(g.latest_success().unwrap().unwrap().id, ok);
    }

    #[test]
    fn lists_oldest_first() {
        let g = Generations::in_memory().unwrap();
        let a = g.record(&sample(Kind::Forward, None, Outcome::Success)).unwrap();
        let b = g.record(&sample(Kind::Rollback, Some(a), Outcome::Success)).unwrap();
        let ids: Vec<i64> = g.list().unwrap().iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn has_store_path_answers_without_loading_history() {
        let g = Generations::in_memory().unwrap();
        g.record(&sample(Kind::Forward, None, Outcome::Success)).unwrap();
        assert!(g.has_store_path("/nix/store/aaa-system").unwrap());
        assert!(!g.has_store_path("/nix/store/never-activated").unwrap());
    }

    #[test]
    fn missing_id_is_none() {
        let g = Generations::in_memory().unwrap();
        assert!(g.get(999).unwrap().is_none());
    }
}
