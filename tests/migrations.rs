use bevy_app::App;
use bevy_ecs::prelude::*;
use bevy_gigasaver::{AppSaveExt, Capture, Error, SavePlugin, SavedType, WorldSaveExt, component};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Component, Serialize, Deserialize)]
struct OldNode {
    amount: u64,
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    peer: Entity,
}

#[derive(Component, Serialize, Deserialize)]
struct Node {
    count: String,
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    target: Entity,
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct Settings {
    enabled: bool,
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct Obsolete(u64);

fn old(path: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(path))
        .persist(component::<OldNode>("old_node"))
        .persist_resource::<Obsolete>("obsolete")
        .insert_resource(Obsolete(99));
    app
}

fn seed(path: &Path) {
    let mut app = old(path);
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(a)
        .insert(OldNode { amount: 3, peer: b });
    app.world_mut()
        .entity_mut(b)
        .insert(OldNode { amount: 7, peer: a });
    app.world_mut().save().unwrap();
}

fn current(path: &Path, calls: Arc<AtomicUsize>) -> App {
    let mut app = App::new();
    let second_calls = calls.clone();
    app.add_plugins(
        SavePlugin::sqlite(path)
            .version(3)
            .migrate(1, move |data| {
                calls.fetch_add(1, Ordering::SeqCst);
                let SavedType::Component(mut rows) = data.remove("old_node").unwrap() else {
                    unreachable!()
                };
                for value in rows.values_mut() {
                    let object = value.as_object_mut().unwrap();
                    let amount = object.remove("amount").unwrap();
                    object.insert("count".into(), amount);
                    let peer = object.remove("peer").unwrap();
                    object.insert("target".into(), peer);
                }
                data.insert("node".into(), SavedType::Component(rows));
                data.remove("obsolete");
                Ok(())
            })
            .migrate(2, move |data| {
                second_calls.fetch_add(1, Ordering::SeqCst);
                let SavedType::Component(rows) = data.get_mut("node").unwrap() else {
                    unreachable!()
                };
                for value in rows.values_mut() {
                    value["count"] = json!(value["count"].as_u64().unwrap().to_string());
                }
                data.insert(
                    "settings".into(),
                    SavedType::Resource(Some(json!({"enabled": true}))),
                );
                Ok(())
            }),
    )
    .persist(component::<Node>("node").capture(Capture::Changed))
    .persist_resource::<Settings>("settings");
    app
}

fn generation(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT generation FROM meta", [], |row| row.get(0))
        .unwrap()
}

fn assert_old_readable(path: &Path) {
    let mut app = old(path);
    app.world_mut().load().unwrap();
    assert_eq!(app.world().resource::<Obsolete>().0, 99);
    assert_eq!(
        app.world_mut()
            .query::<&OldNode>()
            .iter(app.world())
            .count(),
        2
    );
    assert_eq!(generation(path), 1);
}

#[test]
fn chain_stages_manifest_and_payload_changes_then_commits_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    seed(&path);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut app = current(&path, calls.clone());
    app.world_mut().load().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(app.world().resource::<Settings>().enabled);
    let nodes: Vec<_> = app
        .world_mut()
        .query::<(Entity, &Node)>()
        .iter(app.world())
        .map(|(entity, node)| (entity, node.count.clone(), node.target))
        .collect();
    assert_eq!(nodes.len(), 2);
    for (entity, _, target) in &nodes {
        assert_eq!(app.world().get::<Node>(*target).unwrap().target, *entity);
    }
    assert_old_readable(&path);
    let stats = app.world_mut().save().unwrap();
    assert_eq!(stats.generation, 2);
    assert_eq!(stats.deletions, 3); // two old components and the removed resource
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT schema_version FROM meta", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM entity_refs", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        2
    );
    // The old reader must fail rather than downgrade the new save.
    let mut outdated = old(&path);
    assert!(matches!(
        outdated.world_mut().load(),
        Err(Error::NewerSchema {
            saved: 3,
            supported: 1
        })
    ));
    assert!(matches!(
        outdated.world_mut().save(),
        Err(Error::LoadRequired)
    ));
    let mut reopened = current(&path, calls.clone());
    reopened.world_mut().load().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    reopened.world_mut().save().unwrap(); // full baseline after load
    assert_eq!(reopened.world_mut().save().unwrap().upserts, 1); // resource only
}

#[test]
fn missing_and_failing_steps_preserve_live_state_and_committed_data() {
    for failure in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        seed(&path);
        let mut plugin = SavePlugin::sqlite(&path).version(3).migrate(1, |data| {
            data.remove("obsolete");
            Ok(())
        });
        if failure {
            plugin = plugin.migrate(2, |_| Err("injected failure".into()));
        }
        let mut app = App::new();
        app.add_plugins(plugin)
            .persist_resource::<Settings>("settings")
            .insert_resource(Settings { enabled: false });
        let error = app.world_mut().load().unwrap_err();
        assert!(matches!(
            error,
            Error::MissingMigration(2) | Error::Migration { from: 2, .. }
        ));
        assert!(!app.world().resource::<Settings>().enabled);
        assert!(matches!(app.world_mut().save(), Err(Error::LoadRequired)));
        assert_old_readable(&path);
    }
}

#[test]
fn invalid_migrated_payload_and_dangling_references_fail_before_replacement() {
    for dangling in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        seed(&path);
        let mut app = App::new();
        app.add_plugins(
            SavePlugin::sqlite(&path)
                .version(2)
                .migrate(1, move |data| {
                    let SavedType::Component(rows) = data.get_mut("old_node").unwrap() else {
                        unreachable!()
                    };
                    let value = rows.values_mut().next().unwrap();
                    if dangling {
                        value["peer"] = json!(999999);
                    } else {
                        value["amount"] = json!("wrong type");
                    }
                    Ok(())
                }),
        )
        .persist(component::<OldNode>("old_node"))
        .persist_resource::<Obsolete>("obsolete");
        let live = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(live).insert(OldNode {
            amount: 42,
            peer: live,
        });
        assert!(matches!(app.world_mut().load(), Err(Error::Decode { .. })));
        assert_eq!(app.world().get::<OldNode>(live).unwrap().amount, 42);
        assert!(matches!(app.world_mut().save(), Err(Error::LoadRequired)));
        assert_old_readable(&path);
    }
}

#[test]
fn failed_first_migrated_commit_rolls_back_version_manifest_and_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    seed(&path);
    let mut app = current(&path, Arc::default());
    app.world_mut().load().unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_commit BEFORE UPDATE ON meta BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(matches!(app.world_mut().save(), Err(Error::Sqlite(_))));
    assert_old_readable(&path);
    connection
        .execute_batch("DROP TRIGGER reject_commit")
        .unwrap();
    assert_eq!(app.world_mut().save().unwrap().generation, 2);
}

#[test]
fn migration_retains_conflict_detection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    seed(&path);
    let mut app = current(&path, Arc::default());
    app.world_mut().load().unwrap();
    let mut writer = old(&path);
    writer.world_mut().load().unwrap();
    writer.world_mut().save().unwrap();
    assert!(matches!(
        app.world_mut().save(),
        Err(Error::Conflict {
            expected: 1,
            actual: 2
        })
    ));
}

#[test]
fn original_storage_format_is_read_without_upgrading_until_successful_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    seed(&path);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("ALTER TABLE meta DROP COLUMN schema_version; PRAGMA user_version = 1;")
        .unwrap();
    let mut app = current(&path, Arc::default());
    app.world_mut().load().unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    connection.execute_batch("CREATE TRIGGER reject_commit BEFORE UPDATE ON meta BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
    assert!(app.world_mut().save().is_err());
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_old_readable(&path);
    connection
        .execute_batch("DROP TRIGGER reject_commit")
        .unwrap();
    app.world_mut().save().unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn removing_a_referenced_entity_during_migration_does_not_leave_a_phantom_target() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    seed(&path);
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(&path).version(2).migrate(1, |data| {
        let SavedType::Component(rows) = data.get_mut("old_node").unwrap() else {
            unreachable!()
        };
        rows.pop_first();
        Ok(())
    }))
    .persist(component::<OldNode>("old_node"))
    .persist_resource::<Obsolete>("obsolete");
    assert!(
        matches!(app.world_mut().load(), Err(Error::InvalidData(message)) if message.contains("without a restored owner"))
    );
    assert!(matches!(app.world_mut().save(), Err(Error::LoadRequired)));
    assert_old_readable(&path);
}

#[test]
fn checkpoints_use_application_migrations_and_preserve_reference_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unused.sqlite");
    let mut source = old(&path);
    let a = source.world_mut().spawn_empty().id();
    let b = source.world_mut().spawn_empty().id();
    source
        .world_mut()
        .entity_mut(a)
        .insert(OldNode { amount: 3, peer: b });
    source
        .world_mut()
        .entity_mut(b)
        .insert(OldNode { amount: 7, peer: a });
    let checkpoint = bevy_gigasaver::Checkpoint::capture(source.world_mut()).unwrap();
    let checkpoint: bevy_gigasaver::Checkpoint =
        serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut target = current(&path, calls.clone());
    checkpoint.restore(target.world_mut()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(target.world().resource::<Settings>().enabled);
    let nodes: Vec<_> = target
        .world_mut()
        .query::<(Entity, &Node)>()
        .iter(target.world())
        .map(|(e, n)| (e, n.count.clone(), n.target))
        .collect();
    assert_eq!(nodes.len(), 2);
    for (entity, _, peer) in nodes {
        assert_eq!(target.world().get::<Node>(peer).unwrap().target, entity);
    }
    assert!(!path.exists());

    let mut failing = App::new();
    failing
        .add_plugins(
            SavePlugin::sqlite(&path)
                .version(2)
                .migrate(1, |_| Err("deliberate failure".into())),
        )
        .persist(component::<OldNode>("old_node"))
        .persist_resource::<Obsolete>("obsolete");
    let survivor = failing.world_mut().spawn_empty().id();
    failing.world_mut().entity_mut(survivor).insert(OldNode {
        amount: 99,
        peer: survivor,
    });
    assert!(matches!(
        checkpoint.restore(failing.world_mut()),
        Err(Error::Migration { .. })
    ));
    assert_eq!(failing.world().get::<OldNode>(survivor).unwrap().amount, 99);
    assert!(!path.exists());
}

#[test]
fn newly_registered_resources_default_but_saved_absence_stays_absent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut source = old(&path);
    source.world_mut().remove_resource::<Obsolete>();
    source.world_mut().save().unwrap();
    let checkpoint = bevy_gigasaver::Checkpoint::capture(source.world_mut()).unwrap();
    for use_checkpoint in [true, false] {
        let mut target = old(&path);
        target
            .persist_resource::<Settings>("settings")
            .insert_resource(Settings { enabled: true });
        if use_checkpoint {
            checkpoint.restore(target.world_mut()).unwrap();
        } else {
            target.world_mut().load().unwrap();
        }
        assert!(!target.world().resource::<Settings>().enabled);
        assert!(!target.world().contains_resource::<Obsolete>());
        if !use_checkpoint {
            target.world_mut().save().unwrap();
            let mut reloaded = old(&path);
            reloaded.persist_resource::<Settings>("settings");
            reloaded.world_mut().load().unwrap();
            assert!(!reloaded.world().resource::<Settings>().enabled);
        }
    }
}
