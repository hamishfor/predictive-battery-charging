//! On-device SQLite store. Everything stays local (default /var/lib/pbcd/pbcd.db).
//!
//! The raw event log is the source of truth. Sessions are derived from it
//! by `pbc_model::session` and cached in `sessions`, so a builder fix can
//! rebuild them from history.

use anyhow::{Context, Result};
use pbc_model::session::{End, PowerEvent, Session, When};
use pbc_model::Ts;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE events (
        id      INTEGER PRIMARY KEY,
        ts      INTEGER NOT NULL,
        kind    TEXT    NOT NULL,
        json    TEXT    NOT NULL
    );
    CREATE INDEX events_ts ON events(ts);

    CREATE TABLE sessions (
        id               INTEGER PRIMARY KEY,
        plugged_lo       INTEGER NOT NULL,
        plugged_hi       INTEGER NOT NULL,
        end_kind         TEXT    NOT NULL CHECK (end_kind IN ('unplugged','censored')),
        end_lo           INTEGER NOT NULL,
        end_hi           INTEGER NOT NULL,
        capacity_at_plug INTEGER,
        capacity_at_end  INTEGER,
        merged_gaps      INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX sessions_plugged ON sessions(plugged_lo);

    CREATE TABLE samples (
        ts          INTEGER NOT NULL,
        online      INTEGER NOT NULL,
        status      TEXT,
        capacity    INTEGER,
        energy_uwh  INTEGER,
        power_uw    INTEGER,
        th_start    INTEGER,
        th_end      INTEGER
    );
    CREATE INDEX samples_ts ON samples(ts);

    CREATE TABLE kv (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // v2: per-machine capability records (see pbc_model::selfcheck).
    r#"
    CREATE TABLE capabilities (
        capability   TEXT PRIMARY KEY,
        json         TEXT NOT NULL
    );
    "#,
];

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub ts: Ts,
    pub online: bool,
    pub status: Option<String>,
    pub capacity: Option<u8>,
    pub energy_uwh: Option<u64>,
    pub power_uw: Option<u64>,
    pub th_start: Option<u8>,
    pub th_end: Option<u8>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut s = Self { conn };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&mut self) -> Result<()> {
        let v: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(v as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    // ---- events

    pub fn append_event(&self, ev: &PowerEvent) -> Result<()> {
        let json = serde_json::to_string(ev)?;
        let kind = json_kind(&json);
        self.conn.execute(
            "INSERT INTO events (ts, kind, json) VALUES (?1, ?2, ?3)",
            params![ev.ts(), kind, json],
        )?;
        Ok(())
    }

    pub fn events_since(&self, ts: Ts) -> Result<Vec<PowerEvent>> {
        let mut st = self
            .conn
            .prepare("SELECT json FROM events WHERE ts >= ?1 ORDER BY ts, id")?;
        let rows = st.query_map([ts], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }

    pub fn last_event_ts(&self) -> Result<Option<Ts>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(ts) FROM events", [], |r| r.get(0))?)
    }

    // ---- sessions

    pub fn insert_session(&self, s: &Session) -> Result<i64> {
        insert_session_tx(&self.conn, s)
    }

    /// Sessions that started at or after `since`, oldest first.
    pub fn sessions_since(&self, since: Ts) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(
            "SELECT plugged_lo, plugged_hi, end_kind, end_lo, end_hi, capacity_at_plug, capacity_at_end, merged_gaps
             FROM sessions WHERE plugged_lo >= ?1 ORDER BY plugged_lo, id",
        )?;
        let rows = st.query_map([since], |r| {
            let kind: String = r.get(2)?;
            let (lo, hi): (i64, i64) = (r.get(3)?, r.get(4)?);
            Ok(Session {
                plugged: When::between(r.get(0)?, r.get(1)?),
                end: if kind == "censored" {
                    End::Censored(lo)
                } else {
                    End::Unplugged(When::between(lo, hi))
                },
                capacity_at_plug: r.get(5)?,
                capacity_at_end: r.get(6)?,
                merged_gaps: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replace the whole sessions table (rebuild from the event log) atomically.
    pub fn replace_sessions(&mut self, sessions: &[Session]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM sessions", [])?;
        for s in sessions {
            insert_session_tx(&tx, s)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn session_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?)
    }

    // ---- samples

    pub fn append_sample(&self, s: &Sample) -> Result<()> {
        self.conn.execute(
            "INSERT INTO samples (ts, online, status, capacity, energy_uwh, power_uw, th_start, th_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![s.ts, s.online, s.status, s.capacity, s.energy_uwh.map(|v| v as i64), s.power_uw.map(|v| v as i64), s.th_start, s.th_end],
        )?;
        Ok(())
    }

    pub fn samples_between(&self, from: Ts, to: Ts) -> Result<Vec<Sample>> {
        let mut st = self.conn.prepare(
            "SELECT ts, online, status, capacity, energy_uwh, power_uw, th_start, th_end
             FROM samples WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts",
        )?;
        let rows = st.query_map([from, to], |r| {
            Ok(Sample {
                ts: r.get(0)?,
                online: r.get(1)?,
                status: r.get(2)?,
                capacity: r.get(3)?,
                energy_uwh: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                power_uw: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                th_start: r.get(6)?,
                th_end: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Delete samples older than `before`. Samples are high-volume. Events and sessions are kept.
    pub fn prune_samples(&self, before: Ts) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM samples WHERE ts < ?1", [before])?)
    }

    // ---- kv

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn capability(
        &self,
        c: pbc_model::selfcheck::Capability,
    ) -> Result<pbc_model::selfcheck::Record> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM capabilities WHERE capability = ?1",
                [c.key()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match json {
            Some(j) => serde_json::from_str(&j)?,
            None => pbc_model::selfcheck::Record::new(c),
        })
    }

    pub fn capabilities(&self) -> Result<Vec<pbc_model::selfcheck::Record>> {
        pbc_model::selfcheck::Capability::ALL
            .iter()
            .map(|c| self.capability(*c))
            .collect()
    }

    pub fn set_capability(&self, r: &pbc_model::selfcheck::Record) -> Result<()> {
        self.conn.execute(
            "INSERT INTO capabilities (capability, json) VALUES (?1, ?2)
             ON CONFLICT(capability) DO UPDATE SET json = excluded.json",
            params![r.capability.key(), serde_json::to_string(r)?],
        )?;
        Ok(())
    }

    /// Wipe all learned data (the UI's "reset history").
    pub fn reset(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM events; DELETE FROM sessions; DELETE FROM samples; DELETE FROM kv; DELETE FROM capabilities;",
        )?;
        Ok(())
    }
}

fn insert_session_tx(conn: &Connection, s: &Session) -> Result<i64> {
    let (kind, lo, hi) = match s.end {
        End::Unplugged(w) => ("unplugged", w.lo, w.hi),
        End::Censored(t) => ("censored", t, t),
    };
    conn.execute(
        "INSERT INTO sessions (plugged_lo, plugged_hi, end_kind, end_lo, end_hi, capacity_at_plug, capacity_at_end, merged_gaps)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![s.plugged.lo, s.plugged.hi, kind, lo, hi, s.capacity_at_plug, s.capacity_at_end, s.merged_gaps],
    )?;
    Ok(conn.last_insert_rowid())
}

fn json_kind(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(String::from))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/pbcd.db");
        {
            let s = Store::open(&p).unwrap();
            assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as i64);
            s.set("k", "v1").unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v1"));
        s.set("k", "v2").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn events_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let evs = vec![
            PowerEvent::Ac {
                ts: 10,
                online: true,
                capacity: Some(40),
            },
            PowerEvent::Suspend {
                ts: 20,
                capacity: None,
            },
            PowerEvent::Resume {
                ts: 30,
                online: false,
                capacity: Some(39),
            },
        ];
        for e in &evs {
            s.append_event(e).unwrap();
        }
        assert_eq!(s.events_since(0).unwrap(), evs);
        assert_eq!(s.events_since(15).unwrap(), evs[1..]);
        assert_eq!(s.last_event_ts().unwrap(), Some(30));
    }

    #[test]
    fn sessions_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let a = Session {
            plugged: When::exact(100),
            end: End::Unplugged(When::between(200, 300)),
            capacity_at_plug: Some(20),
            capacity_at_end: Some(80),
            merged_gaps: 2,
        };
        let b = Session {
            plugged: When::between(400, 450),
            end: End::Censored(900),
            capacity_at_plug: None,
            capacity_at_end: None,
            merged_gaps: 0,
        };
        s.insert_session(&a).unwrap();
        s.insert_session(&b).unwrap();
        assert_eq!(s.sessions_since(0).unwrap(), vec![a, b.clone()]);
        assert_eq!(s.sessions_since(101).unwrap(), vec![b]);
        assert_eq!(s.session_count().unwrap(), 2);
    }

    #[test]
    fn capabilities_round_trip_and_migrate() {
        use pbc_model::selfcheck::{Capability, Verdict};
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db");
        {
            let s = Store::open(&p).unwrap();
            assert_eq!(s.schema_version().unwrap(), 2);
            let mut r = s.capability(Capability::Enforce).unwrap();
            assert_eq!(r.verdict(), Verdict::Unknown);
            r.pass(1);
            r.fail(2, "kept charging");
            s.set_capability(&r).unwrap();
        }
        let s = Store::open(&p).unwrap();
        let r = s.capability(Capability::Enforce).unwrap();
        assert_eq!((r.passes, r.failures), (1, 1));
        assert_eq!(r.verdict(), Verdict::Failed);
        assert_eq!(s.capabilities().unwrap().len(), 4);
    }

    #[test]
    fn samples_and_prune() {
        let s = Store::open_in_memory().unwrap();
        for ts in 0..10 {
            s.append_sample(&Sample {
                ts,
                online: true,
                status: Some("Charging".into()),
                capacity: Some(50),
                energy_uwh: Some(1),
                power_uw: Some(2),
                th_start: Some(75),
                th_end: Some(80),
            })
            .unwrap();
        }
        assert_eq!(s.samples_between(3, 5).unwrap().len(), 3);
        assert_eq!(s.prune_samples(5).unwrap(), 5);
        assert_eq!(s.samples_between(0, 100).unwrap().len(), 5);
        s.reset().unwrap();
        assert_eq!(s.samples_between(0, 100).unwrap().len(), 0);
    }
}
