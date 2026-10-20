use crate::Error;
use bevy_ecs::prelude::Resource;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// Staged application data indexed by stable persistence key. Renaming/removing
/// an entry changes the manifest. Component IDs are opaque positive identities:
/// preserve them and their references when transforming payloads.
pub type MigrationData = BTreeMap<String, SavedType>;

/// The two persistence roles. An absent resource still has a manifest entry.
#[derive(Debug)]
pub enum SavedType {
    Component(BTreeMap<i64, Value>),
    Resource(Option<Value>),
}

type Step = Arc<dyn Fn(&mut MigrationData) -> Result<(), String> + Send + Sync>;

#[derive(Clone, Resource)]
pub(crate) struct Schema {
    pub version: u32,
    pub steps: BTreeMap<u32, Step>,
}

impl Default for Schema {
    fn default() -> Self {
        Self {
            version: 1,
            steps: BTreeMap::new(),
        }
    }
}

impl Schema {
    pub fn migrate(&self, mut version: u32, data: &mut MigrationData) -> Result<(), Error> {
        if version > self.version {
            return Err(Error::NewerSchema {
                saved: version,
                supported: self.version,
            });
        }
        while version < self.version {
            let step = self
                .steps
                .get(&version)
                .ok_or(Error::MissingMigration(version))?;
            step(data).map_err(|message| Error::Migration {
                from: version,
                message,
            })?;
            version += 1;
        }
        Ok(())
    }
}

pub(crate) type Records = BTreeMap<String, Vec<(i64, Vec<u8>)>>;

/// Shared by SQLite loads and backend-independent checkpoints.
pub(crate) fn migrate_records(
    schema: &Schema,
    version: u32,
    saved_ids: &[i64],
    records: &mut Records,
    manifest: &mut BTreeMap<String, i64>,
) -> Result<(), Error> {
    if version == 0 {
        return Err(Error::InvalidData("zero schema version".into()));
    }
    if version > schema.version {
        return Err(Error::NewerSchema {
            saved: version,
            supported: schema.version,
        });
    }
    if version == schema.version {
        return Ok(());
    }
    let mut data = MigrationData::new();
    for (key, kind) in manifest.iter() {
        let rows = records.remove(key).unwrap_or_default();
        let mut values = BTreeMap::new();
        for (id, bytes) in rows {
            if (*kind == 0 && id != 0) || (*kind == 1 && id <= 0) {
                return Err(Error::InvalidData("record role mismatch".into()));
            }
            let value = serde_json::from_slice(&bytes).map_err(|error| {
                Error::InvalidData(format!("decoding migration input {key}: {error}"))
            })?;
            if values.insert(id, value).is_some() {
                return Err(Error::InvalidData(
                    "duplicate migration record identity".into(),
                ));
            }
        }
        let value = match kind {
            0 => SavedType::Resource(values.remove(&0)),
            1 => SavedType::Component(values),
            _ => return Err(Error::InvalidData("unknown persistence role".into())),
        };
        data.insert(key.clone(), value);
    }
    if !records.is_empty() {
        return Err(Error::InvalidData("unknown record type".into()));
    }
    schema.migrate(version, &mut data)?;
    manifest.clear();
    for (key, value) in data {
        let (kind, values) = match value {
            SavedType::Resource(value) => {
                (0, value.map(|v| (0, v)).into_iter().collect::<Vec<_>>())
            }
            SavedType::Component(values) => {
                if values
                    .keys()
                    .any(|id| *id <= 0 || saved_ids.binary_search(id).is_err())
                {
                    return Err(Error::InvalidData(
                        "migration introduced an unknown component identity".into(),
                    ));
                }
                (1, values.into_iter().collect())
            }
        };
        manifest.insert(key.clone(), kind);
        records.insert(
            key,
            values
                .into_iter()
                .map(|(id, value)| {
                    serde_json::to_vec(&value)
                        .map(|bytes| (id, bytes))
                        .map_err(|error| Error::InvalidData(error.to_string()))
                })
                .collect::<Result<_, _>>()?,
        );
    }
    Ok(())
}
