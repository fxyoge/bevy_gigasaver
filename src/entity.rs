//! `#[serde(with = "bevy_gigasaver::entity")]` writes stable IDs, not Bevy's raw
//! entity representation. Resources and nested collections need no additional
//! remapping registration: references resolve into the staged destination world.

use crate::identity::Identity;
use bevy_ecs::entity::Entity;
use serde::{Deserialize, Deserializer, Serializer, de, ser};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::Arc,
};

enum Context {
    Save(Arc<HashMap<Entity, Identity>>),
    Load(Arc<HashMap<i64, Entity>>, Option<bevy_asset::AssetServer>),
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
    static REFERENCES: RefCell<Option<HashSet<i64>>> = const { RefCell::new(None) };
}

struct Guard(Option<Context>);

impl Drop for Guard {
    fn drop(&mut self) {
        CONTEXT.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

fn scope<R>(context: Context, f: impl FnOnce() -> R) -> R {
    let _guard = Guard(CONTEXT.with(|slot| slot.replace(Some(context))));
    f()
}

pub(crate) fn saving<R>(ids: Arc<HashMap<Entity, Identity>>, f: impl FnOnce() -> R) -> R {
    scope(Context::Save(ids), f)
}

pub(crate) fn loading<R>(
    ids: Arc<HashMap<i64, Entity>>,
    assets: Option<bevy_asset::AssetServer>,
    f: impl FnOnce() -> R,
) -> R {
    scope(Context::Load(ids, assets), f)
}

pub(crate) fn assets() -> Option<bevy_asset::AssetServer> {
    CONTEXT.with(|slot| match slot.borrow().as_ref() {
        Some(Context::Load(_, assets)) => assets.clone(),
        _ => None,
    })
}

struct ReferencesGuard(Option<HashSet<i64>>);

impl Drop for ReferencesGuard {
    fn drop(&mut self) {
        REFERENCES.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub(crate) fn references<R>(f: impl FnOnce() -> R) -> (R, Vec<i64>) {
    let _guard = ReferencesGuard(REFERENCES.with(|slot| slot.replace(Some(HashSet::new()))));
    let value = f();
    let references = REFERENCES.with(|slot| slot.borrow_mut().take().expect("reference context"));
    (value, references.into_iter().collect())
}

pub fn serialize<S: Serializer>(entity: &Entity, serializer: S) -> Result<S::Ok, S::Error> {
    let id = saved_id(*entity).map_err(ser::Error::custom)?;
    serializer.serialize_i64(id)
}

pub fn saved_id(entity: Entity) -> Result<i64, crate::Error> {
    let id = CONTEXT
        .with(|slot| match slot.borrow().as_ref() {
            Some(Context::Save(ids)) => ids
                .get(&entity)
                .filter(|entry| entry.memberships > 0)
                .map(|entry| entry.id)
                .ok_or("entity reference points outside the saved selection"),
            _ => Err("entity serialization requires a save context"),
        })
        .map_err(|message| crate::Error::InvalidData(message.into()))?;
    REFERENCES.with(|slot| {
        if let Some(references) = slot.borrow_mut().as_mut() {
            references.insert(id);
        }
    });
    Ok(id)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Entity, D::Error> {
    let id = i64::deserialize(deserializer)?;
    loaded_entity(id).map_err(de::Error::custom)
}

pub fn loaded_entity(id: i64) -> Result<Entity, crate::Error> {
    CONTEXT
        .with(|slot| match slot.borrow().as_ref() {
            Some(Context::Load(ids, _)) => ids
                .get(&id)
                .copied()
                .ok_or("dangling saved entity reference"),
            _ => Err("entity deserialization requires a restore context"),
        })
        .map_err(|message| crate::Error::InvalidData(message.into()))
}

pub mod optional {
    use super::*;
    use serde::Serialize;
    pub fn serialize<S: Serializer>(
        value: &Option<Entity>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .map(saved_id)
            .transpose()
            .map_err(ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Entity>, D::Error> {
        Option::<i64>::deserialize(deserializer)?
            .map(loaded_entity)
            .transpose()
            .map_err(de::Error::custom)
    }
}
pub mod list {
    use super::*;
    use serde::Serialize;
    pub fn serialize<S: Serializer>(values: &[Entity], serializer: S) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(|&e| saved_id(e))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Entity>, D::Error> {
        Vec::<i64>::deserialize(deserializer)?
            .into_iter()
            .map(|id| loaded_entity(id).map_err(de::Error::custom))
            .collect()
    }
}
pub mod map {
    use super::*;
    use bevy_ecs::entity::EntityHashMap;
    use serde::Serialize;
    pub fn serialize<S: Serializer, T: Serialize>(
        values: &EntityHashMap<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut values = values
            .iter()
            .map(|(&e, v)| Ok((saved_id(e)?, v)))
            .collect::<Result<Vec<_>, crate::Error>>()
            .map_err(ser::Error::custom)?;
        values.sort_by_key(|(id, _)| *id);
        values.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<EntityHashMap<T>, D::Error> {
        let mut map = EntityHashMap::default();
        for (id, value) in Vec::<(i64, T)>::deserialize(deserializer)? {
            if map
                .insert(loaded_entity(id).map_err(de::Error::custom)?, value)
                .is_some()
            {
                return Err(de::Error::custom("duplicate entity map key"));
            }
        }
        Ok(map)
    }
}
