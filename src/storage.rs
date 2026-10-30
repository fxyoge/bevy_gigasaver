use crate::{
    LoadOutcome, entity,
    identity::{Identities, Identity},
    registration::{Batch, Registry},
};
use bevy_ecs::{
    entity::{Entity, EntityHashMap},
    prelude::*,
};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const APPLICATION_ID: i64 = 0x4749_4741;
const FORMAT: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("install SavePlugin before saving or loading")]
    MissingPlugin,
    #[error("session is not ready to save")]
    NotReady,
    #[error("session is closed; open a save path first")]
    Closed,
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("encoding {key}: {source}")]
    Encode {
        key: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("decoding {key}: {source}")]
    Decode {
        key: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("load this save successfully before writing")]
    LoadRequired,
    #[error("save file: {0}")]
    Io(#[from] std::io::Error),
    #[error("unrecognized database or unsupported save format")]
    Format,
    #[error("persistent type keys/roles do not match this save's manifest")]
    Manifest,
    #[error("save generation changed: expected {expected}, found {actual}; load before retrying")]
    Conflict { expected: u64, actual: u64 },
    #[error("invalid save data: {0}")]
    InvalidData(String),
    #[error("waiting for asset: {0}")]
    AssetNotReady(String),
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidData(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveStats {
    pub generation: u64,
    pub entities: usize,
    /// Counts records, not entities; includes snapshot rewrites.
    pub upserts: usize,
    pub deletions: usize,
    /// Payload only; excludes SQL/index/WAL overhead.
    pub bytes: usize,
}

#[derive(Resource)]
pub(crate) struct Storage {
    path: PathBuf,
    // Connection is Send but not Sync. Exclusive world operations use get_mut,
    // so this wrapper supplies Resource's Sync bound without a runtime lock.
    connection: Mutex<Option<Connection>>,
    blocked: bool,
    pub closed: bool,
}

impl Storage {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            connection: Mutex::new(None),
            blocked: false,
            closed: false,
        }
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.connection
            .get_mut()
            .expect("storage mutex poisoned")
            .take();
    }

    fn connection(&mut self, create: bool) -> Result<&mut Connection, Error> {
        let slot = self.connection.get_mut().expect("storage mutex poisoned");
        if slot.is_none() {
            let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
            if create {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            }
            let mut connection = Connection::open_with_flags(&self.path, flags)?;
            let application: i64 =
                connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
            if application == 0 && create {
                let tables: i64 = connection.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )?;
                if tables != 0 {
                    return Err(Error::Format);
                }
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute_batch(
                    "PRAGMA application_id = 1195984705;
                     PRAGMA user_version = 1;
                     CREATE TABLE meta (
                         singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                         generation INTEGER NOT NULL CHECK(generation >= 0)
                     ) STRICT;
                     INSERT INTO meta VALUES (1, 0);
                     CREATE TABLE types (
                         key TEXT PRIMARY KEY, kind INTEGER NOT NULL CHECK(kind IN (0, 1))
                     ) STRICT;
                     CREATE TABLE entities (id INTEGER PRIMARY KEY CHECK(id >= 0)) STRICT;
                     INSERT INTO entities VALUES (0);
                     CREATE TABLE records (
                         entity INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
                         type_key TEXT NOT NULL REFERENCES types(key),
                         payload BLOB NOT NULL,
                         PRIMARY KEY(entity, type_key)
                     ) STRICT;
                     CREATE TABLE entity_refs (
                         source_entity INTEGER NOT NULL,
                         source_type TEXT NOT NULL,
                         target INTEGER NOT NULL REFERENCES entities(id) DEFERRABLE INITIALLY DEFERRED,
                         FOREIGN KEY(source_entity, source_type) REFERENCES records(entity, type_key) ON DELETE CASCADE,
                         PRIMARY KEY(source_entity, source_type, target)
                     ) STRICT;
                     CREATE INDEX entity_refs_target ON entity_refs(target);"
                )?;
                transaction.commit()?;
            } else if application != APPLICATION_ID {
                return Err(Error::Format);
            }
            let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version != FORMAT {
                return Err(Error::Format);
            }
            connection.busy_timeout(Duration::from_secs(5))?;
            let journal: String =
                connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
            if journal != "wal" {
                return Err(Error::InvalidData("WAL mode unavailable".into()));
            }
            connection.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
            *slot = Some(connection);
        }
        Ok(slot.as_mut().expect("initialized connection"))
    }
}

fn generation(connection: &Connection) -> Result<i64, Error> {
    let generation = connection.query_row(
        "SELECT generation FROM meta WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if generation < 0 {
        return Err(Error::InvalidData("negative save generation".into()));
    }
    Ok(generation)
}

fn read_manifest(connection: &Connection) -> Result<BTreeMap<String, i64>, Error> {
    Ok(connection
        .prepare("SELECT key, kind FROM types")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?)
}

fn check_manifest(actual: &BTreeMap<String, i64>, registry: &Registry) -> Result<(), Error> {
    let expected: BTreeMap<_, _> = registry
        .entries
        .iter()
        .map(|entry| (entry.key.to_owned(), entry.kind))
        .collect();
    if *actual != expected {
        return Err(Error::Manifest);
    }
    Ok(())
}

pub(crate) fn check_load_manifest(
    actual: &BTreeMap<String, i64>,
    registry: &Registry,
) -> Result<(), Error> {
    for (key, kind) in actual {
        if !registry
            .entries
            .iter()
            .any(|entry| entry.key == key && entry.kind == *kind)
        {
            return Err(Error::Manifest);
        }
    }
    Ok(())
}

pub(crate) fn save(world: &mut World) -> Result<SaveStats, Error> {
    if !world.contains_resource::<Storage>() {
        return Err(Error::MissingPlugin);
    }
    if world.resource::<Storage>().closed {
        return Err(Error::Closed);
    }
    if world.resource::<Storage>().blocked {
        return Err(Error::LoadRequired);
    }
    world.flush();
    world.resource_scope(|world, mut registry: Mut<Registry>| {
        registry.started = true;
        for entry in &mut registry.entries {
            entry.codec.reconcile(world);
        }
        let mut batch = Batch::default();
        let baseline = registry.baseline;
        entity::saving(world.resource::<Identities>().entries.clone(), || {
            let mut contexts = Vec::new();
            for entry in &mut registry.entries {
                contexts.extend(entry.codec.capture(world, baseline, &mut batch)?);
            }
            Ok::<_, Error>(())
        })?;
        // Dirty sets have hash order. Cluster writes by persistent identity so
        // dense dirty saves do not repeatedly bounce between SQLite pages.
        batch
            .upserts
            .sort_unstable_by(|a, b| (a.id, a.key).cmp(&(b.id, b.key)));
        let tick = world.read_change_tick();
        let stats = world.resource_scope(|world, mut storage: Mut<Storage>| {
            commit(
                storage.connection(true)?,
                &registry,
                world.resource::<Identities>(),
                &batch,
            )
        })?;
        for entry in &mut registry.entries {
            entry.codec.acknowledge(world, tick);
        }
        registry.baseline = true;
        registry.generation = Some(stats.generation);
        registry.rewrite_manifest = false;
        world.resource_mut::<Identities>().acknowledge();
        // Direct World mutations after this call must not share the saved tick.
        world.increment_change_tick();
        Ok(stats)
    })
}

fn commit(
    connection: &mut Connection,
    registry: &Registry,
    ids: &Identities,
    batch: &Batch,
) -> Result<SaveStats, Error> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous = generation(&transaction)?;
    let expected = registry.generation.unwrap_or(0);
    if expected != previous as u64 {
        if registry.generation.is_none() {
            return Err(Error::LoadRequired);
        }
        return Err(Error::Conflict {
            expected,
            actual: previous as u64,
        });
    }
    let next = previous
        .checked_add(1)
        .ok_or_else(|| Error::InvalidData("generation exhausted".into()))?;
    let mut deletions = 0;
    if registry.rewrite_manifest {
        deletions += transaction.execute("DELETE FROM records", [])?;
        transaction.execute("DELETE FROM entities WHERE id != 0", [])?;
        transaction.execute("DELETE FROM types", [])?;
    }
    if previous == 0 || registry.rewrite_manifest {
        for entry in &registry.entries {
            transaction.execute(
                "INSERT INTO types(key, kind) VALUES (?1, ?2)",
                params![entry.key, entry.kind],
            )?;
        }
    }
    check_manifest(&read_manifest(&transaction)?, registry)?;
    if !registry.baseline {
        deletions += transaction.execute("DELETE FROM records", [])?;
        transaction.execute("DELETE FROM entities WHERE id != 0", [])?;
    }
    {
        let mut insert =
            transaction.prepare_cached("INSERT OR IGNORE INTO entities(id) VALUES (?1)")?;
        if registry.baseline {
            for entity in &ids.added {
                insert.execute([ids.entries[entity].id])?;
            }
        } else {
            for identity in ids
                .entries
                .values()
                .filter(|identity| identity.memberships > 0)
            {
                insert.execute([identity.id])?;
            }
        }
        let mut delete = transaction
            .prepare_cached("DELETE FROM records WHERE entity = ?1 AND type_key = ?2")?;
        for (id, key) in &batch.deletes {
            deletions += delete.execute(params![id, key])?;
        }
        let mut upsert = transaction.prepare_cached(
            "INSERT INTO records(entity, type_key, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT(entity, type_key) DO UPDATE SET payload = excluded.payload",
        )?;
        let mut clear_refs = transaction.prepare_cached(
            "DELETE FROM entity_refs WHERE source_entity = ?1 AND source_type = ?2",
        )?;
        let mut insert_ref =
            transaction.prepare_cached("INSERT INTO entity_refs VALUES (?1, ?2, ?3)")?;
        for record in &batch.upserts {
            upsert.execute(params![record.id, record.key, record.payload])?;
            clear_refs.execute(params![record.id, record.key])?;
            for target in &record.references {
                insert_ref.execute(params![record.id, record.key, target])?;
            }
        }
        let mut delete_entity = transaction.prepare_cached("DELETE FROM entities WHERE id = ?1")?;
        for entity in &ids.removed {
            delete_entity.execute([ids.entries[entity].id])?;
        }
    }
    transaction.execute(
        "UPDATE meta SET generation = ?1 WHERE singleton = 1",
        [next],
    )?;
    transaction.commit()?;
    Ok(SaveStats {
        generation: next as u64,
        entities: ids.count,
        upserts: batch.upserts.len(),
        deletions,
        bytes: batch
            .upserts
            .iter()
            .map(|record| record.payload.len())
            .sum(),
    })
}

pub(crate) fn load(world: &mut World) -> Result<LoadOutcome, Error> {
    if !world.contains_resource::<Storage>() {
        return Err(Error::MissingPlugin);
    }
    if world.resource::<Storage>().closed {
        return Err(Error::Closed);
    }
    // Also guards against a panic during user deserialization/component hooks.
    world.resource_mut::<Storage>().blocked = true;
    let result = load_inner(world);
    if result.is_ok() {
        world.resource_mut::<Storage>().blocked = false;
    }
    result
}

pub(crate) fn can_save(world: &World) -> bool {
    let storage = world.resource::<Storage>();
    !storage.closed && !storage.blocked && world.resource::<Registry>().generation.is_some()
}

pub(crate) fn clear_scope(world: &mut World) {
    world.flush();
    world.resource_scope(|world, mut registry: Mut<Registry>| {
        for entry in &mut registry.entries {
            entry.codec.reconcile(world);
        }
        let entities: Vec<_> = world
            .resource::<Identities>()
            .entries
            .iter()
            .filter(|(_, id)| id.memberships > 0)
            .map(|(entity, _)| *entity)
            .collect();
        for entity in entities {
            world.despawn(entity);
        }
        for entry in &registry.entries {
            entry.codec.clear(world);
        }
        for entry in &registry.entries {
            entry.codec.initialize(world);
        }
        world.insert_resource(Identities::default());
        for entry in &mut registry.entries {
            entry.codec.reset(world);
        }
        registry.baseline = false;
        registry.rewrite_manifest = false;
    });
}

fn load_inner(world: &mut World) -> Result<LoadOutcome, Error> {
    world.flush();
    world.resource_scope(|world, mut registry: Mut<Registry>| {
        registry.started = true;
        let saved = world.resource_scope(|_, mut storage: Mut<Storage>| {
            if storage
                .connection
                .get_mut()
                .expect("storage mutex poisoned")
                .is_none()
            {
                match std::fs::metadata(&storage.path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(Error::Io(error)),
                    Ok(_) => {}
                }
            }
            let transaction = storage.connection(false)?.transaction()?;
            let saved_generation = generation(&transaction)?;
            if saved_generation == 0 {
                return Ok(None);
            }
            let manifest = read_manifest(&transaction)?;
            let ids = transaction
                .prepare("SELECT id FROM entities WHERE id != 0 ORDER BY id")?
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut records: BTreeMap<String, Vec<(i64, Vec<u8>)>> = BTreeMap::new();
            {
                let mut statement =
                    transaction.prepare("SELECT entity, type_key, payload FROM records")?;
                for row in statement.query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })? {
                    let (id, key, payload) = row?;
                    if id != 0 && ids.binary_search(&id).is_err() {
                        return Err(Error::InvalidData(
                            "record references a missing entity".into(),
                        ));
                    }
                    records.entry(key).or_default().push((id, payload));
                }
            }
            transaction.commit()?;
            Ok::<_, Error>(Some((saved_generation, ids, records, manifest)))
        })?;
        let Some((saved_generation, saved_ids, records, manifest)) = saved else {
            registry.baseline = false;
            registry.generation = Some(0);
            registry.rewrite_manifest = false;
            return Ok(LoadOutcome::Missing);
        };
        check_load_manifest(&manifest, &registry)?;
        restore_records(world, &mut registry, saved_ids, records, &manifest)?;
        registry.baseline = false;
        registry.generation = Some(saved_generation as u64);
        registry.rewrite_manifest = manifest.len() != registry.entries.len();
        world.increment_change_tick();
        Ok(LoadOutcome::Loaded)
    })
}

pub(crate) fn restore_records(
    world: &mut World,
    registry: &mut Registry,
    saved_ids: Vec<i64>,
    mut records: BTreeMap<String, Vec<(i64, Vec<u8>)>>,
    manifest: &BTreeMap<String, i64>,
) -> Result<(), Error> {
    let next = saved_ids
        .last()
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| Error::InvalidData("persistent entity IDs exhausted".into()))?;
    let mut actual: HashMap<_, _> = saved_ids
        .into_iter()
        .map(|id| (id, world.spawn_empty().id()))
        .collect();
    let (prepared, references) = entity::references(|| {
        entity::loading(
            Arc::new(actual.clone()),
            world.get_resource::<bevy_asset::AssetServer>().cloned(),
            || {
                let mut decoded = Vec::new();
                for entry in &registry.entries {
                    decoded.push(if manifest.contains_key(entry.key) {
                        entry
                            .codec
                            .decode(world, records.remove(entry.key).unwrap_or_default())?
                    } else {
                        entry.codec.decode_missing(world)?
                    });
                }
                if !records.is_empty() {
                    return Err(Error::InvalidData("unknown record type".into()));
                }
                Ok(decoded)
            },
        )
    });
    let decoded = match prepared {
        Ok(decoded) => decoded,
        Err(error) => {
            for entity in actual.values() {
                world.despawn(*entity);
            }
            return Err(error);
        }
    };
    let owned: std::collections::HashSet<_> = decoded
        .iter()
        .flat_map(|value| value.entities(&actual))
        .collect();
    let allocated: std::collections::HashSet<_> = actual.values().copied().collect();
    if !owned.is_subset(&allocated)
        || references
            .iter()
            .any(|id| !actual.get(id).is_some_and(|entity| owned.contains(entity)))
    {
        for entity in actual.values() {
            world.despawn(*entity);
        }
        return Err(Error::InvalidData(
            "reference to an entity without a restored owner".into(),
        ));
    }
    actual.retain(|_, entity| {
        if owned.contains(entity) {
            true
        } else {
            world.despawn(*entity);
            false
        }
    });
    for entry in &mut registry.entries {
        entry.codec.reconcile(world);
    }
    let old: Vec<_> = world
        .resource::<Identities>()
        .entries
        .iter()
        .filter(|(_, identity)| identity.memberships > 0)
        .map(|(entity, _)| *entity)
        .collect();
    world.resource_mut::<Identities>().suspended = true;
    for entity in old {
        world.despawn(entity);
    }
    world.flush();
    for entry in &registry.entries {
        entry.codec.clear(world);
    }
    let mut mapper: EntityHashMap<Entity> =
        actual.values().map(|entity| (*entity, *entity)).collect();
    for value in decoded {
        value.apply(world, &actual, &mut mapper);
    }
    world.flush();
    world.insert_resource(Identities {
        entries: Arc::new(
            actual
                .into_iter()
                .map(|(id, entity)| (entity, Identity { id, memberships: 0 }))
                .collect(),
        ),
        next,
        ..Default::default()
    });
    for entry in &mut registry.entries {
        entry.codec.reset(world);
    }
    registry.baseline = false;
    world.increment_change_tick();
    Ok(())
}
