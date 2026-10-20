use crate::{Error, entity, identity::Identities};
use bevy_app::App;
use bevy_ecs::{
    change_detection::{DetectChanges, MAX_CHANGE_AGE, Tick},
    entity::EntityHashMap,
    entity_disabling::DefaultQueryFilters,
    prelude::*,
    query::{ArchetypeFilter, QueryState},
    system::SystemParam,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    any::{Any, TypeId},
    collections::HashSet,
    marker::PhantomData,
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Capture {
    #[default]
    Snapshot,
    /// Scans all matches, comparing ticks against the last successful commit.
    Changed,
    /// Lifecycle changes are automatic; in-place mutations require `Dirty::mark`.
    Dirty,
}

pub struct ComponentSave<T, F = ()> {
    key: &'static str,
    capture: Capture,
    attached: bool,
    marker: PhantomData<fn() -> (T, F)>,
}

pub fn component<T: Component + Serialize + DeserializeOwned>(
    key: &'static str,
) -> ComponentSave<T> {
    ComponentSave {
        key,
        capture: Capture::Snapshot,
        attached: false,
        marker: PhantomData,
    }
}

impl<T, F> ComponentSave<T, F> {
    /// Save this component only on entities selected by other registrations.
    pub fn attached(mut self) -> Self {
        self.attached = true;
        self
    }
    pub fn capture(mut self, capture: Capture) -> Self {
        self.capture = capture;
        self
    }

    /// Requires a membership scan even with dirty capture. Persist filter markers
    /// too if selection must survive loading.
    pub fn filter<G: ArchetypeFilter + 'static>(self) -> ComponentSave<T, G> {
        ComponentSave {
            key: self.key,
            capture: self.capture,
            attached: self.attached,
            marker: PhantomData,
        }
    }
}

struct ResourceSave<T> {
    key: &'static str,
    initialize: fn(&mut World),
    marker: PhantomData<T>,
}

fn resource<T: Resource + FromWorld + Serialize + DeserializeOwned>(
    key: &'static str,
) -> ResourceSave<T> {
    ResourceSave {
        key,
        initialize: |world| {
            world.init_resource::<T>();
        },
        marker: PhantomData,
    }
}

pub struct Registration {
    key: &'static str,
    kind: i64,
    type_id: TypeId,
    pub(crate) setup: Setup,
}

impl Registration {
    pub(crate) fn new_adapter(key: &'static str, type_id: TypeId, setup: Setup) -> Self {
        Self {
            key,
            kind: 0,
            type_id,
            setup,
        }
    }
}

pub(crate) type Setup = Box<dyn FnOnce(&mut World) -> Box<dyn Codec> + Send + Sync>;

pub trait AppSaveExt {
    /// Keys are on-disk schema identifiers. Registration freezes at first save/load.
    fn persist(&mut self, registration: impl Into<Registration>) -> &mut Self;
    fn persist_resource<T: Resource + FromWorld + Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> &mut Self;
    fn persist_component<T: Component + Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> &mut Self;
    fn persist_attached_component<T: Component + Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> &mut Self;
}

impl AppSaveExt for App {
    fn persist_resource<T: Resource + FromWorld + Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> &mut Self {
        self.persist(resource::<T>(intern_key(key)))
    }
    fn persist_component<T: Component + Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> &mut Self {
        self.persist(component::<T>(intern_key(key)))
    }
    fn persist_attached_component<T: Component + Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> &mut Self {
        self.persist(component::<T>(intern_key(key)).attached())
    }
    fn persist(&mut self, registration: impl Into<Registration>) -> &mut Self {
        let registration = registration.into();
        self.init_resource::<Registry>()
            .init_resource::<Identities>();
        let registry = self.world().resource::<Registry>();
        assert!(
            !registry.started,
            "register persistence before the first save/load"
        );
        assert!(
            !registration.key.is_empty(),
            "persistent type keys must not be empty"
        );
        assert!(
            registry
                .entries
                .iter()
                .all(|entry| entry.key != registration.key),
            "duplicate persistent type key: {}",
            registration.key
        );
        assert!(
            registry
                .entries
                .iter()
                .all(|entry| (entry.type_id, entry.kind)
                    != (registration.type_id, registration.kind)),
            "a type can only be persisted once per component/resource role"
        );
        let codec = (registration.setup)(self.world_mut());
        self.world_mut()
            .resource_mut::<Registry>()
            .entries
            .push(Entry {
                key: registration.key,
                kind: registration.kind,
                type_id: registration.type_id,
                codec,
            });
        self.world_mut()
            .resource_mut::<Registry>()
            .entries
            .sort_by_key(|entry| entry.codec.priority());
        self
    }
}

#[derive(SystemParam)]
pub struct Dirty<'w, T: Component> {
    // Shared scheduler access; the mutex protects only the pending set.
    tracking: Res<'w, Tracking<T>>,
}

impl<T: Component> Dirty<'_, T> {
    pub fn mark(&mut self, entity: Entity) {
        self.tracking
            .pending
            .lock()
            .expect("dirty queue poisoned")
            .insert(entity);
    }
}

#[derive(Resource)]
struct Tracking<T: Component> {
    members: HashSet<Entity>,
    pending: Mutex<HashSet<Entity>>,
    filtered: bool,
    marker: PhantomData<T>,
}

fn inserted<T: Component>(
    event: On<Insert<T>>,
    mut tracking: ResMut<Tracking<T>>,
    mut ids: ResMut<Identities>,
) {
    if ids.suspended {
        return;
    }
    if !tracking.filtered && tracking.members.insert(event.entity) {
        ids.enter(event.entity);
    }
    tracking
        .pending
        .get_mut()
        .expect("dirty queue poisoned")
        .insert(event.entity);
}

fn removed<T: Component>(
    event: On<Remove<T>>,
    mut tracking: ResMut<Tracking<T>>,
    mut ids: ResMut<Identities>,
) {
    if ids.suspended {
        return;
    }
    if tracking.members.remove(&event.entity) {
        ids.leave(event.entity);
    }
    tracking
        .pending
        .get_mut()
        .expect("dirty queue poisoned")
        .insert(event.entity);
}

#[derive(Resource, Default)]
pub(crate) struct Registry {
    pub entries: Vec<Entry>,
    pub baseline: bool,
    pub started: bool,
    pub generation: Option<u64>,
    pub rewrite_manifest: bool,
}

pub(crate) struct Entry {
    pub key: &'static str,
    pub kind: i64,
    type_id: TypeId,
    pub codec: Box<dyn Codec>,
}

pub(crate) struct Record {
    pub id: i64,
    pub key: &'static str,
    pub payload: Vec<u8>,
    pub references: Vec<i64>,
}

#[derive(Default)]
pub(crate) struct Batch {
    pub upserts: Vec<Record>,
    pub deletes: Vec<(i64, &'static str)>,
}

pub(crate) trait Codec: Send + Sync {
    fn priority(&self) -> u8 {
        1
    }
    fn initialize(&self, _world: &mut World) {}
    fn clear(&self, _world: &mut World) {}
    fn reconcile(&mut self, world: &mut World);
    fn capture(
        &mut self,
        world: &mut World,
        baseline: bool,
        batch: &mut Batch,
    ) -> Result<Option<Box<dyn Any>>, Error>;
    fn decode(
        &self,
        world: &mut World,
        records: Vec<(i64, Vec<u8>)>,
    ) -> Result<Box<dyn Decoded>, Error>;
    fn decode_missing(&self, world: &mut World) -> Result<Box<dyn Decoded>, Error> {
        self.decode(world, Vec::new())
    }
    fn acknowledge(&mut self, world: &mut World, tick: Tick);
    fn reset(&mut self, world: &mut World);
}

pub(crate) trait Decoded {
    fn entities(&self, _: &std::collections::HashMap<i64, Entity>) -> Vec<Entity> {
        Vec::new()
    }
    fn apply(
        self: Box<Self>,
        world: &mut World,
        ids: &std::collections::HashMap<i64, Entity>,
        mapper: &mut EntityHashMap<Entity>,
    );
}

struct ComponentCodec<T: Component, F: ArchetypeFilter + 'static> {
    key: &'static str,
    capture: Capture,
    query: QueryState<(Entity, Ref<'static, T>), F>,
    last_commit: Tick,
    attached: bool,
}

impl<T, F> From<ComponentSave<T, F>> for Registration
where
    T: Component + Serialize + DeserializeOwned,
    F: ArchetypeFilter + 'static,
{
    fn from(value: ComponentSave<T, F>) -> Self {
        Self {
            key: value.key,
            kind: 1,
            type_id: TypeId::of::<T>(),
            setup: Box::new(move |world| {
                world.insert_resource(Tracking::<T> {
                    members: HashSet::new(),
                    pending: Mutex::default(),
                    filtered: value.attached || TypeId::of::<F>() != TypeId::of::<()>(),
                    marker: PhantomData,
                });
                world.add_observer(inserted::<T>);
                world.add_observer(removed::<T>);
                let mut codec = ComponentCodec::<T, F> {
                    key: value.key,
                    capture: value.capture,
                    query: if world.contains_resource::<DefaultQueryFilters>() {
                        world.resource_scope(|world, _: Mut<DefaultQueryFilters>| {
                            world.query_filtered()
                        })
                    } else {
                        world.query_filtered()
                    },
                    last_commit: Tick::new(0),
                    attached: value.attached,
                };
                codec.reset(world);
                Box::new(codec)
            }),
        }
    }
}

impl<T, F> ComponentCodec<T, F>
where
    T: Component + Serialize + DeserializeOwned,
    F: ArchetypeFilter + 'static,
{
    fn refresh_membership(&mut self, world: &mut World) {
        if self.attached {
            return;
        }
        self.query.update_archetypes(world);
        let ordered: Vec<_> = self.query.iter(world).map(|(entity, _)| entity).collect();
        let selected: HashSet<_> = ordered.iter().copied().collect();
        world.resource_scope(|world, mut tracking: Mut<Tracking<T>>| {
            let mut ids = world.resource_mut::<Identities>();
            for entity in tracking.members.difference(&selected) {
                ids.leave(*entity);
                tracking
                    .pending
                    .lock()
                    .expect("dirty queue poisoned")
                    .insert(*entity);
            }
            // Allocate initial IDs in query order, not randomized hash order.
            // Preserve storage locality for components present at registration.
            for entity in ordered
                .iter()
                .filter(|entity| !tracking.members.contains(entity))
            {
                ids.enter(*entity);
                tracking
                    .pending
                    .lock()
                    .expect("dirty queue poisoned")
                    .insert(*entity);
            }
            tracking.members = selected;
        });
    }

    fn encode(
        &self,
        entity: Entity,
        value: &T,
        world: &World,
        batch: &mut Batch,
    ) -> Result<(), Error> {
        let id = world.resource::<Identities>().entries[&entity].id;
        let (payload, references) = entity::references(|| serde_json::to_vec(value));
        let payload = payload.map_err(|source| Error::Encode {
            key: self.key,
            source,
        })?;
        batch.upserts.push(Record {
            id,
            key: self.key,
            payload,
            references,
        });
        Ok(())
    }
}

impl<T, F> Codec for ComponentCodec<T, F>
where
    T: Component + Serialize + DeserializeOwned,
    F: ArchetypeFilter + 'static,
{
    fn reconcile(&mut self, world: &mut World) {
        self.query.update_archetypes(world);
        if world.resource::<Tracking<T>>().filtered {
            self.refresh_membership(world);
        }
    }

    fn capture(
        &mut self,
        world: &mut World,
        baseline: bool,
        batch: &mut Batch,
    ) -> Result<Option<Box<dyn Any>>, Error> {
        if self.attached {
            for (entity, value) in self.query.iter_manual(world) {
                if world
                    .resource::<Identities>()
                    .entries
                    .get(&entity)
                    .is_some_and(|id| id.memberships > 0)
                {
                    self.encode(entity, &value, world, batch)?;
                }
            }
            let tracking = world.resource::<Tracking<T>>();
            let pending = tracking.pending.lock().expect("dirty queue poisoned");
            let ids = world.resource::<Identities>();
            for entity in pending
                .iter()
                .filter(|entity| world.get::<T>(**entity).is_none())
            {
                if let Some(identity) = ids.entries.get(entity) {
                    batch.deletes.push((identity.id, self.key));
                }
            }
            return Ok(None);
        }
        let tracking = world.resource::<Tracking<T>>();
        let pending = tracking.pending.lock().expect("dirty queue poisoned");
        let expired = world
            .read_change_tick()
            .get()
            .wrapping_sub(self.last_commit.get())
            >= MAX_CHANGE_AGE;
        if self.capture == Capture::Dirty && baseline {
            for entity in pending
                .iter()
                .filter(|entity| tracking.members.contains(entity))
            {
                if let Some(value) = world.get::<T>(*entity) {
                    self.encode(*entity, value, world, batch)?;
                }
            }
        } else {
            for (entity, value) in self.query.iter_manual(world) {
                if !baseline
                    || self.capture == Capture::Snapshot
                    || expired
                    || pending.contains(&entity)
                    || value
                        .last_changed()
                        .is_newer_than(self.last_commit, world.read_change_tick())
                {
                    self.encode(entity, &value, world, batch)?;
                }
            }
        }
        let ids = world.resource::<Identities>();
        for entity in pending
            .iter()
            .filter(|entity| !tracking.members.contains(entity))
        {
            if let Some(identity) = ids.entries.get(entity) {
                batch.deletes.push((identity.id, self.key));
            }
        }
        Ok(None)
    }

    fn decode(
        &self,
        _world: &mut World,
        records: Vec<(i64, Vec<u8>)>,
    ) -> Result<Box<dyn Decoded>, Error> {
        let values = records
            .into_iter()
            .map(|(id, payload)| {
                if id <= 0 {
                    return Err(Error::InvalidData("component stored as a resource".into()));
                }
                serde_json::from_slice(&payload)
                    .map(|value| (id, value))
                    .map_err(|source| Error::Decode {
                        key: self.key,
                        source,
                    })
            })
            .collect::<Result<Vec<(i64, T)>, _>>()?;
        Ok(Box::new(DecodedComponent(values)))
    }

    fn acknowledge(&mut self, world: &mut World, tick: Tick) {
        self.last_commit = tick;
        world
            .resource_mut::<Tracking<T>>()
            .pending
            .get_mut()
            .expect("dirty queue poisoned")
            .clear();
    }

    fn reset(&mut self, world: &mut World) {
        world.resource_mut::<Tracking<T>>().members.clear();
        world
            .resource_mut::<Tracking<T>>()
            .pending
            .get_mut()
            .expect("dirty queue poisoned")
            .clear();
        self.refresh_membership(world);
    }
}

struct DecodedComponent<T>(Vec<(i64, T)>);

impl<T: Component> Decoded for DecodedComponent<T> {
    fn entities(&self, ids: &std::collections::HashMap<i64, Entity>) -> Vec<Entity> {
        self.0.iter().map(|(id, _)| ids[id]).collect()
    }
    fn apply(
        self: Box<Self>,
        world: &mut World,
        ids: &std::collections::HashMap<i64, Entity>,
        mapper: &mut EntityHashMap<Entity>,
    ) {
        for (id, mut value) in self.0 {
            T::map_entities(&mut value, mapper);
            world.entity_mut(ids[&id]).insert(value);
        }
    }
}

struct ResourceCodec<T> {
    key: &'static str,
    present: bool,
    initialize: fn(&mut World),
    marker: PhantomData<T>,
}

impl<T: Resource + Serialize + DeserializeOwned> From<ResourceSave<T>> for Registration {
    fn from(value: ResourceSave<T>) -> Self {
        Self {
            key: value.key,
            kind: 0,
            type_id: TypeId::of::<T>(),
            setup: Box::new(move |_| {
                Box::new(ResourceCodec::<T> {
                    key: value.key,
                    present: false,
                    initialize: value.initialize,
                    marker: PhantomData,
                })
            }),
        }
    }
}

impl<T: Resource + Serialize + DeserializeOwned> Codec for ResourceCodec<T> {
    fn decode_missing(&self, _world: &mut World) -> Result<Box<dyn Decoded>, Error> {
        Ok(Box::new(InitializeResource(self.initialize)))
    }
    fn initialize(&self, world: &mut World) {
        (self.initialize)(world);
    }
    fn clear(&self, world: &mut World) {
        world.remove_resource::<T>();
    }
    fn reconcile(&mut self, _: &mut World) {}

    fn capture(
        &mut self,
        world: &mut World,
        baseline: bool,
        batch: &mut Batch,
    ) -> Result<Option<Box<dyn Any>>, Error> {
        if let Some(value) = world.get_resource::<T>() {
            let encode = || {
                serde_json::to_vec(value).map_err(|source| Error::Encode {
                    key: self.key,
                    source,
                })
            };
            let (payload, references) = entity::references(encode);
            batch.upserts.push(Record {
                id: 0,
                key: self.key,
                payload: payload?,
                references,
            });
        } else if baseline && self.present {
            batch.deletes.push((0, self.key));
        }
        Ok(None)
    }

    fn decode(
        &self,
        _world: &mut World,
        records: Vec<(i64, Vec<u8>)>,
    ) -> Result<Box<dyn Decoded>, Error> {
        if records.len() > 1 || records.first().is_some_and(|(id, _)| *id != 0) {
            return Err(Error::InvalidData("resource stored as components".into()));
        }
        let value = records
            .into_iter()
            .next()
            .map(|(_, payload)| {
                let decode = || {
                    serde_json::from_slice(&payload).map_err(|source| Error::Decode {
                        key: self.key,
                        source,
                    })
                };
                decode()
            })
            .transpose()?;
        Ok(Box::new(DecodedResource::<T> { value }))
    }

    fn acknowledge(&mut self, world: &mut World, _: Tick) {
        self.present = world.contains_resource::<T>();
    }
    fn reset(&mut self, _: &mut World) {
        self.present = false;
    }
}

pub(crate) fn intern_key(key: &str) -> &'static str {
    use std::sync::OnceLock;
    static KEYS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut keys = KEYS
        .get_or_init(Default::default)
        .lock()
        .expect("save key interner poisoned");
    if let Some(key) = keys.get(key) {
        return key;
    }
    let key = Box::leak(key.to_owned().into_boxed_str());
    keys.insert(key);
    key
}

#[doc(hidden)]
pub struct Declaration {
    pub key: &'static str,
    pub install: fn(&mut App),
}
inventory::collect!(Declaration);

#[doc(hidden)]
pub fn declare_resource<T: Resource + FromWorld + Serialize + DeserializeOwned>(
    app: &mut App,
    key: &str,
) {
    app.persist_resource::<T>(key);
}

#[doc(hidden)]
pub fn declare_component<T: Component + Serialize + DeserializeOwned>(app: &mut App, key: &str) {
    app.persist_component::<T>(key);
}

pub(crate) fn initialize(world: &mut World) {
    world.resource_scope(|world, registry: Mut<Registry>| {
        for entry in &registry.entries {
            entry.codec.initialize(world);
        }
    });
}

struct DecodedResource<T> {
    value: Option<T>,
}

struct InitializeResource(fn(&mut World));

impl Decoded for InitializeResource {
    fn apply(
        self: Box<Self>,
        world: &mut World,
        _: &std::collections::HashMap<i64, Entity>,
        _: &mut EntityHashMap<Entity>,
    ) {
        (self.0)(world);
    }
}

impl<T: Resource> Decoded for DecodedResource<T> {
    fn apply(
        self: Box<Self>,
        world: &mut World,
        _: &std::collections::HashMap<i64, Entity>,
        _: &mut EntityHashMap<Entity>,
    ) {
        if let Some(value) = self.value {
            world.insert_resource(value);
        } else {
            world.remove_resource::<T>();
        }
    }
}
