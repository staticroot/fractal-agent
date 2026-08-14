//! Who staged what. Several principals share one working copy, because the
//! system configuration is one entity with one authority; a working copy each
//! would make several writers of one entity.
//!
//! The file records the values, so this records who put each one there.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Keyed by uid, because that is what the kernel attests and what survives a
/// rename.
pub type Uid = u32;

pub struct Staged {
    conn: Connection,
}

impl Staged {
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

    /// Claim `key` for `uid`, refusing when another principal holds it unless
    /// `override_staged` is set.
    ///
    /// This is the one true conflict in a shared working copy: taking the last
    /// write silently would lose a change its author still believes is pending.
    pub fn claim(&self, key: &str, uid: Uid, override_staged: bool) -> Result<()> {
        if let Some(holder) = self.holder(key)?
            && holder != uid
            && !override_staged
        {
            return Err(Error::Conflict(format!(
                "{key} is already staged by uid {holder}; restage it deliberately to take it over"
            )));
        }
        self.conn.execute(
            "INSERT INTO staged (key, uid) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET uid = excluded.uid",
            rusqlite::params![key, uid],
        )?;
        Ok(())
    }

    pub fn holder(&self, key: &str) -> Result<Option<Uid>> {
        self.conn
            .query_row("SELECT uid FROM staged WHERE key = ?1", [key], |row| {
                row.get::<_, i64>(0)
            })
            .map(|uid| Some(uid as Uid))
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }

    pub fn release(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM staged WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn all(&self) -> Result<BTreeMap<String, Uid>> {
        let mut stmt = self.conn.prepare("SELECT key, uid FROM staged")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as Uid))
        })?;
        Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }

    pub fn keys_of(&self, uid: Uid) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT key FROM staged WHERE uid = ?1 ORDER BY key")?;
        let rows = stmt.query_map([uid as i64], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// What discarding everything does. A commit releases only the keys it took
    /// in.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM staged", [])?;
        Ok(())
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS staged (
    key TEXT    PRIMARY KEY,
    uid INTEGER NOT NULL
);
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_release_and_list() {
        let s = Staged::in_memory().unwrap();
        s.claim("time.timeZone", 1000, false).unwrap();
        s.claim("networking.hostName", 1001, false).unwrap();

        assert_eq!(s.holder("time.timeZone").unwrap(), Some(1000));
        assert_eq!(s.keys_of(1000).unwrap(), ["time.timeZone"]);
        assert_eq!(s.all().unwrap().len(), 2);

        s.release("time.timeZone").unwrap();
        assert!(s.holder("time.timeZone").unwrap().is_none());
    }

    #[test]
    fn restaging_your_own_key_is_not_a_conflict() {
        let s = Staged::in_memory().unwrap();
        s.claim("time.timeZone", 1000, false).unwrap();
        s.claim("time.timeZone", 1000, false).unwrap();
        assert_eq!(s.holder("time.timeZone").unwrap(), Some(1000));
    }

    #[test]
    fn staging_over_another_principal_is_refused() {
        let s = Staged::in_memory().unwrap();
        s.claim("time.timeZone", 1000, false).unwrap();

        let err = s.claim("time.timeZone", 1001, false).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        // The refusal left the original author holding it.
        assert_eq!(s.holder("time.timeZone").unwrap(), Some(1000));
    }

    #[test]
    fn the_override_takes_the_key_over() {
        let s = Staged::in_memory().unwrap();
        s.claim("time.timeZone", 1000, false).unwrap();
        s.claim("time.timeZone", 1001, true).unwrap();
        assert_eq!(s.holder("time.timeZone").unwrap(), Some(1001));
    }

    #[test]
    fn clear_forgets_everything() {
        let s = Staged::in_memory().unwrap();
        s.claim("a", 1000, false).unwrap();
        s.claim("b", 1001, false).unwrap();
        s.clear().unwrap();
        assert!(s.all().unwrap().is_empty());
    }
}
