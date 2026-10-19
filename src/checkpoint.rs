use crate::{
    Error, entity,
    identity::Identities,
    registration::{Batch, Registry},
    storage,
};
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An owned, backend-independent snapshot using the same registrations,
/// identities and staged restore transaction as SQLite saves.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    version: u32,
    entities: Vec<i64>,
    kinds: BTreeMap<String, i64>,
    data: BTreeMap<String, Vec<(i64, Vec<u8>)>>,
}

impl Checkpoint {
    pub fn clear(world: &mut World) {
        storage::clear_scope(world);
    }

    pub fn capture(world: &mut World) -> Result<Self, Error> {
        world.flush();
        if !world.contains_resource::<Registry>() {
            return Err(Error::InvalidData("no save declarations".into()));
        }
        world.resource_scope(|world, mut registry: Mut<Registry>| {
            registry.started = true;
            for entry in &mut registry.entries {
                entry.codec.reconcile(world);
            }
            let mut batch = Batch::default();
            entity::saving(world.resource::<Identities>().entries.clone(), || {
                let mut contexts = Vec::new();
                for entry in &mut registry.entries {
                    contexts.extend(entry.codec.capture(world, false, &mut batch)?);
                }
                Ok::<_, Error>(())
            })?;
            let mut records: BTreeMap<String, Vec<(i64, Vec<u8>)>> = registry
                .entries
                .iter()
                .map(|entry| (entry.key.into(), Vec::new()))
                .collect();
            for record in batch.upserts {
                records
                    .get_mut(record.key)
                    .unwrap()
                    .push((record.id, record.payload));
            }
            for values in records.values_mut() {
                values.sort_by_key(|(id, _)| *id);
            }
            let mut entities: Vec<_> = world
                .resource::<Identities>()
                .entries
                .values()
                .filter(|id| id.memberships > 0)
                .map(|id| id.id)
                .collect();
            entities.sort();
            Ok(Self {
                version: 1,
                entities,
                kinds: registry
                    .entries
                    .iter()
                    .map(|entry| (entry.key.into(), entry.kind))
                    .collect(),
                data: records,
            })
        })
    }

    pub fn restore(&self, world: &mut World) -> Result<(), Error> {
        if self.version != 1
            || self.entities.iter().any(|id| *id <= 0)
            || self.entities.windows(2).any(|ids| ids[0] >= ids[1])
        {
            return Err(Error::InvalidData(
                "invalid checkpoint version or entity identities".into(),
            ));
        }
        world.flush();
        world.resource_scope(|world, mut registry: Mut<Registry>| {
            let expected: BTreeMap<_, _> = registry
                .entries
                .iter()
                .map(|entry| (entry.key.to_owned(), entry.kind))
                .collect();
            if expected != self.kinds || !self.kinds.keys().eq(self.data.keys()) {
                return Err(Error::Manifest);
            }
            for (key, records) in &self.data {
                let kind = self.kinds[key];
                if records.windows(2).any(|values| values[0].0 >= values[1].0)
                    || records.iter().any(|(id, _)| match kind {
                        0 => *id != 0,
                        1 => self.entities.binary_search(id).is_err(),
                        _ => true,
                    })
                {
                    return Err(Error::InvalidData(
                        "invalid checkpoint record identity".into(),
                    ));
                }
            }
            storage::restore_records(
                world,
                &mut registry,
                self.entities.clone(),
                self.data.clone(),
            )?;
            registry.started = true;
            Ok(())
        })
    }
}
