//! Machines, settings, devices and ingest cursors.

use super::{Change, Result, Store, all, one};
use crate::model::{Device, Machine};
use rusqlite::{Row, params};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

pub(super) fn machine_row(r: &Row<'_>) -> rusqlite::Result<Machine> {
    Ok(Machine {
        id: r.get("id")?,
        name: r.get("name")?,
        os: r.get("os")?,
        role: r.get("role")?,
        last_seen: r.get("last_seen")?,
        revoked: r.get("revoked")?,
    })
}

pub(super) fn device_row(r: &Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: r.get("id")?,
        name: r.get("name")?,
        kind: r.get("kind")?,
        token_hash: r.get("token_hash")?,
        node_id: r.get("node_id")?,
        created_at: r.get("created_at")?,
        last_seen: r.get("last_seen")?,
        revoked: r.get("revoked")?,
        can_control_terminals: r.get("can_control_terminals")?,
        can_access_files: r.get("can_access_files")?,
    })
}

/// Settings keys are short identifiers; values are arbitrary JSON.
pub fn validate_setting_key(key: &str) -> Result<()> {
    let ok = (1..=100).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(super::StoreError::Invalid(format!(
            "setting key {key:?} must be 1-100 chars of [A-Za-z0-9._-]"
        )))
    }
}

impl Store {
    // ---------------------------------------------------------------- machines

    pub fn upsert_machine(&self, m: &Machine) -> Result<()> {
        self.apply(Change::Machine(m.clone())).map(|_| ())
    }

    pub fn get_machine(&self, id: &str) -> Result<Option<Machine>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM machines WHERE id = ?1",
                params![id],
                machine_row,
            )
        })
    }

    pub fn list_machines(&self) -> Result<Vec<Machine>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM machines ORDER BY name COLLATE NOCASE",
                [],
                machine_row,
            )
        })
    }

    // ---------------------------------------------------------------- settings

    pub fn get_setting(&self, key: &str) -> Result<Option<JsonValue>> {
        let raw: Option<String> = self.read(|c| {
            one(
                c,
                "SELECT value_json FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
        })?;
        Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    pub fn all_settings(&self) -> Result<BTreeMap<String, JsonValue>> {
        let rows: Vec<(String, String)> = self.read(|c| {
            all(
                c,
                "SELECT key, value_json FROM settings ORDER BY key",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })?;
        rows.into_iter()
            .map(|(k, v)| Ok((k, serde_json::from_str(&v)?)))
            .collect()
    }

    /// Set (or with `None`, delete) several settings in one transaction.
    pub fn set_settings(&self, values: &BTreeMap<String, Option<JsonValue>>) -> Result<()> {
        for k in values.keys() {
            validate_setting_key(k)?;
        }
        self.write(|tx| {
            for (k, v) in values {
                match v {
                    Some(v) => tx.execute(
                        "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                        params![k, v.to_string()],
                    )?,
                    None => tx.execute("DELETE FROM settings WHERE key = ?1", params![k])?,
                };
            }
            Ok(())
        })
    }

    pub fn set_setting(&self, key: &str, value: &JsonValue) -> Result<()> {
        self.set_settings(&BTreeMap::from([(key.to_string(), Some(value.clone()))]))
    }

    // ---------------------------------------------------------------- devices

    pub fn list_devices(&self) -> Result<Vec<Device>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM devices ORDER BY created_at",
                [],
                device_row,
            )
        })
    }

    pub fn device_by_token_hash(&self, token_hash: &str) -> Result<Option<Device>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM devices WHERE token_hash = ?1 AND revoked = 0",
                params![token_hash],
                device_row,
            )
        })
    }

    pub fn upsert_device(&self, d: &Device) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO devices(id, name, kind, token_hash, node_id, created_at, last_seen, revoked,
                   can_control_terminals, can_access_files) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name, kind=excluded.kind,
                   token_hash=excluded.token_hash, node_id=excluded.node_id, last_seen=excluded.last_seen,
                   revoked=excluded.revoked, can_control_terminals=excluded.can_control_terminals,
                   can_access_files=excluded.can_access_files",
                params![
                    d.id, d.name, d.kind, d.token_hash, d.node_id, d.created_at, d.last_seen, d.revoked,
                    d.can_control_terminals, d.can_access_files
                ],
            )?;
            Ok(())
        })
    }

    /// Bump only `last_seen`: a concurrent revoke or rights change made
    /// between reading the device and this write must survive it.
    pub fn touch_device(&self, id: &str, now: i64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE devices SET last_seen = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- ingest cursors

    pub fn get_cursor(&self, adapter: &str, source: &str) -> Result<Option<JsonValue>> {
        let raw: Option<String> = self.read(|c| {
            one(
                c,
                "SELECT cursor_json FROM ingest_cursors WHERE adapter = ?1 AND source = ?2",
                params![adapter, source],
                |r| r.get(0),
            )
        })?;
        Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    /// Forget a source's cursor: the next full pass reads it from the start
    /// (events already stored are deduplicated). Returns whether it had one.
    pub fn delete_cursor(&self, adapter: &str, source: &str) -> Result<bool> {
        self.write(|tx| {
            Ok(tx.execute(
                "DELETE FROM ingest_cursors WHERE adapter = ?1 AND source = ?2",
                params![adapter, source],
            )? > 0)
        })
    }

    /// Sources of `adapter` whose cursor state names `parent` as their
    /// parent session (`state.parent`, e.g. codex subagent rollouts).
    pub fn cursors_with_parent(&self, adapter: &str, parent: &str) -> Result<Vec<String>> {
        self.read(|c| {
            all(
                c,
                "SELECT source FROM ingest_cursors WHERE adapter = ?1
                   AND json_extract(cursor_json, '$.state.parent') = ?2 ORDER BY source",
                params![adapter, parent],
                |r| r.get(0),
            )
        })
    }

    pub fn set_cursor(&self, adapter: &str, source: &str, cursor: &JsonValue) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO ingest_cursors(adapter, source, cursor_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(adapter, source) DO UPDATE SET cursor_json = excluded.cursor_json",
                params![adapter, source, cursor.to_string()],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store;
    use super::*;
    use crate::model::{DeviceKind, MachineRole};
    use serde_json::json;

    #[test]
    fn settings_machines_devices_cursors() {
        let (_d, store) = temp_store();
        store.set_setting("ui.theme", &json!("dark")).unwrap();
        assert_eq!(store.get_setting("ui.theme").unwrap(), Some(json!("dark")));
        store
            .set_settings(&BTreeMap::from([("ui.theme".to_string(), None)]))
            .unwrap();
        assert!(store.all_settings().unwrap().is_empty());
        assert!(store.set_setting("bad key", &json!(1)).is_err());

        let m = Machine {
            id: "m".into(),
            name: "box".into(),
            os: "windows".into(),
            role: MachineRole::Standalone,
            last_seen: 1,
            revoked: false,
        };
        store.upsert_machine(&m).unwrap();
        assert_eq!(store.list_machines().unwrap(), vec![m.clone()]);
        assert_eq!(store.outbox_after(0, 10).unwrap()[0].entity, "machines");

        let d = Device {
            id: "d".into(),
            name: "phone".into(),
            kind: DeviceKind::Browser,
            token_hash: Some("h".into()),
            node_id: None,
            created_at: 1,
            last_seen: 1,
            revoked: false,
            can_control_terminals: false,
            can_access_files: false,
        };
        store.upsert_device(&d).unwrap();
        assert_eq!(store.device_by_token_hash("h").unwrap(), Some(d.clone()));
        // A stale read-modify-write of last_seen must not undo a revoke.
        store
            .upsert_device(&Device {
                revoked: true,
                ..d.clone()
            })
            .unwrap();
        store.touch_device(&d.id, 99).unwrap();
        let back = &store.list_devices().unwrap()[0];
        assert!(back.revoked);
        assert_eq!(back.last_seen, 99);
        assert_eq!(store.device_by_token_hash("h").unwrap(), None);
        assert_eq!(store.list_devices().unwrap().len(), 1);

        store
            .set_cursor("claude", "/a.jsonl", &json!({"offset": 10}))
            .unwrap();
        assert_eq!(
            store.get_cursor("claude", "/a.jsonl").unwrap(),
            Some(json!({"offset": 10}))
        );
        assert_eq!(store.get_cursor("claude", "/b").unwrap(), None);
    }
}
