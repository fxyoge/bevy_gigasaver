//! Transaction participants for runtimes whose state is not ordinary ECS data.
use crate::{AppSaveExt, Error, registration::*};
use bevy_app::App;
use bevy_ecs::{entity::EntityHashMap, prelude::*};
use std::{
    any::{Any, TypeId},
    collections::{HashMap, HashSet},
};

pub type Apply = Box<dyn FnOnce(&mut World)>;

/// The context guard stays alive while every participant and ordinary saved type
/// is encoded/decoded. This allows opaque runtime references inside ordinary data.
pub struct Captured {
    pub bytes: Vec<u8>,
    pub context: Box<dyn Any>,
}

pub struct Prepared {
    pub apply: Apply,
    pub context: Box<dyn Any>,
}

pub trait SaveAdapter: Send + Sync + 'static {
    fn entities(&self, world: &mut World) -> Vec<Entity>;
    fn capture(&self, world: &mut World) -> Result<Captured, Error>;
    fn prepare(&self, world: &mut World, bytes: &[u8]) -> Result<Prepared, Error>;
    fn clear(&self, world: &mut World);
}

pub fn register<A: SaveAdapter>(app: &mut App, key: &str, adapter: A) {
    let key = intern_key(key);
    let type_id = TypeId::of::<A>();
    app.persist(Registration::adapter(key, type_id, adapter));
}

struct AdapterCodec<A> {
    key: &'static str,
    adapter: A,
    members: HashSet<Entity>,
}

impl Registration {
    fn adapter(key: &'static str, type_id: TypeId, adapter: impl SaveAdapter) -> Self {
        Self::new_adapter(
            key,
            type_id,
            Box::new(move |_| {
                Box::new(AdapterCodec {
                    key,
                    adapter,
                    members: HashSet::new(),
                })
            }),
        )
    }
}

impl<A: SaveAdapter> Codec for AdapterCodec<A> {
    fn priority(&self) -> u8 {
        0
    }
    fn clear(&self, world: &mut World) {
        self.adapter.clear(world);
    }
    fn reconcile(&mut self, world: &mut World) {
        let selected: HashSet<_> = self.adapter.entities(world).into_iter().collect();
        let mut ids = world.resource_mut::<crate::identity::Identities>();
        for entity in self.members.difference(&selected) {
            ids.leave(*entity);
        }
        let mut added: Vec<_> = selected.difference(&self.members).copied().collect();
        added.sort();
        for entity in added {
            ids.enter(entity);
        }
        self.members = selected;
    }
    fn capture(
        &mut self,
        world: &mut World,
        _baseline: bool,
        batch: &mut Batch,
    ) -> Result<Option<Box<dyn Any>>, Error> {
        let (captured, references) = crate::entity::references(|| self.adapter.capture(world));
        let captured = captured?;
        batch.upserts.push(Record {
            id: 0,
            key: self.key,
            payload: captured.bytes,
            references,
        });
        Ok(Some(captured.context))
    }
    fn decode(
        &self,
        world: &mut World,
        mut records: Vec<(i64, Vec<u8>)>,
    ) -> Result<Box<dyn Decoded>, Error> {
        if records.len() != 1 || records[0].0 != 0 {
            return Err(Error::InvalidData(format!(
                "missing or invalid adapter {}",
                self.key
            )));
        }
        Ok(Box::new(AdapterDecoded(
            self.adapter.prepare(world, &records.remove(0).1)?,
        )))
    }
    fn acknowledge(&mut self, _world: &mut World, _tick: bevy_ecs::change_detection::Tick) {}
    fn reset(&mut self, world: &mut World) {
        self.members.clear();
        self.reconcile(world);
    }
}

struct AdapterDecoded(Prepared);
impl Decoded for AdapterDecoded {
    fn apply(
        self: Box<Self>,
        world: &mut World,
        _: &HashMap<i64, Entity>,
        _: &mut EntityHashMap<Entity>,
    ) {
        (self.0.apply)(world);
    }
}
