//! What the agent built, and from which commit. This binding is why the agent
//! builds rather than accepting a store path from a caller: with it, a
//! generation's provenance is witnessed rather than guessed.
//!
//! Each row names the garbage-collection root holding the path, so a closure
//! cannot be collected between being built and being activated. An activated
//! path needs no root of its own, because the system profile is one.

use std::path::Path;

use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::generations::LogRef;

/// A closure the agent built, and the commit it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Build {
    pub id: i64,
    pub timestamp: Timestamp,
    pub store_path: String,
    pub commit: String,
    /// The garbage-collection root symlink keeping `store_path` alive.
    pub gc_root: String,
    pub log: Option<LogRef>,
}

/// The facts known when a build finishes.
#[derive(Debug, Clone)]
pub struct NewBuild {
    pub store_path: String,
    pub commit: String,
    pub gc_root: String,
    pub log: Option<LogRef>,
}

pub struct Builds {
    conn: Connection,
}

impl Builds {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_conn(Connection::open(path)?)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Replaces any earlier build of the same commit: a rebuild is the same fact
    /// observed again, not a second one.
    pub fn record(&self, rec: &NewBuild) -> Result<i64> {
        let (lp, ls, lt) = match &rec.log {
            Some(l) => (Some(l.path.clone()), Some(l.size as i64), Some(l.tail.clone())),
            None => (None, None, None),
        };
        self.conn.execute(
            "INSERT INTO builds (
                timestamp, store_path, config_commit, gc_root, log_path, log_size, log_tail
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(config_commit) DO UPDATE SET
                timestamp = excluded.timestamp,
                store_path = excluded.store_path,
                gc_root = excluded.gc_root,
                log_path = excluded.log_path,
                log_size = excluded.log_size,
                log_tail = excluded.log_tail",
            rusqlite::params![
                Timestamp::now().to_string(),
                rec.store_path,
                rec.commit,
                rec.gc_root,
                lp,
                ls,
                lt,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn by_commit(&self, commit: &str) -> Result<Option<Build>> {
        self.one(
            &format!("SELECT {COLUMNS} FROM builds WHERE config_commit = ?1"),
            rusqlite::params![commit],
        )
    }

    fn one(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Option<Build>> {
        self.conn
            .query_row(sql, params, row_to_build)
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }
}

const COLUMNS: &str =
    "id, timestamp, store_path, config_commit, gc_root, log_path, log_size, log_tail";

fn row_to_build(row: &Row) -> rusqlite::Result<Build> {
    let ts: String = row.get(1)?;
    let log = match (row.get::<_, Option<String>>(5)?, row.get::<_, Option<i64>>(6)?, row.get::<_, Option<String>>(7)?) {
        (Some(path), Some(size), Some(tail)) => Some(LogRef {
            path,
            size: size as u64,
            tail,
        }),
        _ => None,
    };
    Ok(Build {
        id: row.get(0)?,
        timestamp: ts.parse().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?,
        store_path: row.get(2)?,
        commit: row.get(3)?,
        gc_root: row.get(4)?,
        log,
    })
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS builds (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp     TEXT    NOT NULL,
    store_path    TEXT    NOT NULL,
    config_commit TEXT    NOT NULL UNIQUE,
    gc_root       TEXT    NOT NULL,
    log_path      TEXT,
    log_size      INTEGER,
    log_tail      TEXT
);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(commit: &str, path: &str) -> NewBuild {
        NewBuild {
            store_path: path.into(),
            commit: commit.into(),
            gc_root: format!("/var/lib/fractal-agent/gcroots/{commit}"),
            log: Some(LogRef {
                path: "/var/lib/fractal-agent/logs/build.log".into(),
                size: 3,
                tail: "ok".into(),
            }),
        }
    }

    #[test]
    fn records_and_finds_by_commit() {
        let b = Builds::in_memory().unwrap();
        b.record(&sample("abc", "/nix/store/aaa-system")).unwrap();

        let found = b.by_commit("abc").unwrap().unwrap();
        assert_eq!(found.store_path, "/nix/store/aaa-system");
        assert_eq!(found.log.unwrap().size, 3);
    }

    #[test]
    fn an_unbuilt_commit_is_none() {
        let b = Builds::in_memory().unwrap();
        assert!(b.by_commit("nobody-built-this").unwrap().is_none());
    }

    #[test]
    fn rebuilding_a_commit_replaces_the_row() {
        let b = Builds::in_memory().unwrap();
        b.record(&sample("abc", "/nix/store/aaa-system")).unwrap();
        b.record(&sample("abc", "/nix/store/bbb-system")).unwrap();

        assert_eq!(
            b.by_commit("abc").unwrap().unwrap().store_path,
            "/nix/store/bbb-system"
        );
    }

    /// Two candidates with one tree and different messages: one closure, two
    /// rows. A lookup by path would have to pick one, which is why there is no
    /// such lookup.
    #[test]
    fn one_closure_can_belong_to_two_commits() {
        let b = Builds::in_memory().unwrap();
        b.record(&sample("alices", "/nix/store/same-system")).unwrap();
        b.record(&sample("bobs", "/nix/store/same-system")).unwrap();

        assert_eq!(b.by_commit("alices").unwrap().unwrap().store_path, "/nix/store/same-system");
        assert_eq!(b.by_commit("bobs").unwrap().unwrap().store_path, "/nix/store/same-system");
    }
}
