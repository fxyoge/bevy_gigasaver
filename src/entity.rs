//! `#[serde(with = "bevy_gigasaver::entity")]` writes stable IDs, not Bevy's raw
//! entity representation. Component fields also need `#[entities]` for remapping;
//! resources need `ResourceSave::map_entities()`.

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
    Load(Arc<HashMap<i64, Entity>>),
    Forbidden,
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

pub(crate) fn loading<R>(ids: Arc<HashMap<i64, Entity>>, f: impl FnOnce() -> R) -> R {
    scope(Context::Load(ids), f)
}

pub(crate) fn forbidden<R>(f: impl FnOnce() -> R) -> R {
    scope(Context::Forbidden, f)
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
    let id = CONTEXT
        .with(|slot| match slot.borrow().as_ref() {
            Some(Context::Save(ids)) => ids
                .get(entity)
                .filter(|entry| entry.memberships > 0)
                .map(|entry| entry.id)
                .ok_or("entity reference points outside the saved selection"),
            _ => Err(
                "entity adapter requires a save context; resource references need map_entities()",
            ),
        })
        .map_err(ser::Error::custom)?;
    REFERENCES.with(|slot| {
        if let Some(references) = slot.borrow_mut().as_mut() {
            references.insert(id);
        }
    });
    serializer.serialize_i64(id)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Entity, D::Error> {
    let id = i64::deserialize(deserializer)?;
    CONTEXT
        .with(|slot| match slot.borrow().as_ref() {
            Some(Context::Load(ids)) => ids
                .get(&id)
                .copied()
                .ok_or("dangling saved entity reference"),
            _ => Err(
                "entity adapter requires a load context; resource references need map_entities()",
            ),
        })
        .map_err(de::Error::custom)
}
