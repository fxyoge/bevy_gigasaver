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
