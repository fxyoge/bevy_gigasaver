use bevy_app::App;
use bevy_ecs::{entity::MapEntities, prelude::*, system::RunSystemOnce};
use bevy_gigasaver::{
    AppSaveExt, Capture, Dirty, Error, LoadOutcome, SavePlugin, WorldSaveExt, component, resource,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

const STRATEGIES: [Capture; 3] = [Capture::Snapshot, Capture::Changed, Capture::Dirty];

#[derive(Component, Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
struct Position {
    name: u32,
    value: u64,
}

#[derive(Component, Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Label(String);

#[derive(Resource, Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Clock(u64);

#[derive(Component)]
struct Unrelated;

fn app(path: &Path, strategy: Capture) -> App {
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(path))
        .persist(component::<Position>("position").capture(strategy))
        .persist(component::<Label>("label").capture(strategy))
        .persist(resource::<Clock>("clock"));
    app
}

fn positions(world: &mut World) -> Vec<Position> {
    let mut values: Vec<_> = world.query::<&Position>().iter(world).cloned().collect();
    values.sort_by_key(|value| value.name);
    values
}

fn mark<T: Component>(world: &mut World, entity: Entity) {
    world
        .run_system_once(move |mut dirty: Dirty<T>| dirty.mark(entity))
        .unwrap();
}

#[test]
fn strategies_restore_identical_state_and_preserve_unrelated_data() {
    for strategy in STRATEGIES {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        let mut app = app(&path, strategy);
        let first = app
            .world_mut()
            .spawn((Position { name: 1, value: 10 }, Label("one".into())))
            .id();
        let second = app.world_mut().spawn(Position { name: 2, value: 20 }).id();
        let unrelated = app.world_mut().spawn(Unrelated).id();
        app.insert_resource(Clock(7));
        let initial = app.world_mut().save().unwrap();
        assert_eq!(
            (initial.generation, initial.entities, initial.upserts),
            (1, 2, 4)
        );
        app.world_mut().get_mut::<Position>(first).unwrap().value = 11;
        mark::<Position>(app.world_mut(), first);
        app.world_mut().entity_mut(first).remove::<Label>();
        app.world_mut().despawn(second);
        app.world_mut().spawn(Position { name: 3, value: 30 });
        app.insert_resource(Clock(8));
        // Deletions remain pending even after Bevy's removal buffers expire.
        for _ in 0..5 {
            app.update();
        }
        let stats = app.world_mut().save().unwrap();
        assert_eq!(
            (
                stats.generation,
                stats.entities,
                stats.upserts,
                stats.deletions
            ),
            (2, 2, 3, 2)
        );
        let expected = positions(app.world_mut());
        app.world_mut().get_mut::<Position>(first).unwrap().value = 999;
        app.insert_resource(Clock(999));
        app.world_mut().load().unwrap();
        assert_eq!(positions(app.world_mut()), expected);
        assert_eq!(*app.world().resource::<Clock>(), Clock(8));
        assert!(app.world().get::<Unrelated>(unrelated).is_some());
        let mut fresh = self::app(&path, strategy);
        fresh.world_mut().load().unwrap();
        assert_eq!(positions(fresh.world_mut()), expected);
        assert_eq!(*fresh.world().resource::<Clock>(), Clock(8));
        assert_eq!(fresh.world_mut().save().unwrap().generation, 3);
    }
}

#[test]
fn changed_ticks_are_relative_to_commits_not_frames_or_world_clear_trackers() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = app(&directory.path().join("save.sqlite"), Capture::Changed);
    let entity = app.world_mut().spawn(Position { name: 1, value: 1 }).id();
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
    assert_eq!(app.world_mut().save().unwrap().upserts, 0);
    app.world_mut().get_mut::<Position>(entity).unwrap().value = 2;
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
    assert_eq!(app.world_mut().save().unwrap().upserts, 0);
    // Direct world mutation immediately after save gets a fresh tick.
    app.world_mut().get_mut::<Position>(entity).unwrap().value = 3;
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
}

#[test]
fn dirty_marks_coalesce_and_insert_replacements_are_automatic() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = app(&directory.path().join("save.sqlite"), Capture::Dirty);
    let entity = app.world_mut().spawn(Position { name: 1, value: 1 }).id();
    app.world_mut().save().unwrap();
    app.world_mut().get_mut::<Position>(entity).unwrap().value = 2;
    // Dirty is deliberately opt-in for in-place mutation.
    assert_eq!(app.world_mut().save().unwrap().upserts, 0);
    for _ in 0..10 {
        mark::<Position>(app.world_mut(), entity);
    }
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
    assert_eq!(app.world_mut().save().unwrap().upserts, 0);
    app.world_mut()
        .entity_mut(entity)
        .insert(Position { name: 1, value: 3 });
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
    app.world_mut().entity_mut(entity).remove::<Position>();
    app.world_mut()
        .entity_mut(entity)
        .insert(Position { name: 1, value: 4 });
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
    app.world_mut().load().unwrap();
    assert_eq!(positions(app.world_mut()), [Position { name: 1, value: 4 }]);
}

#[test]
fn missing_resources_remove_the_persisted_value_and_restore_absence() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = app(&directory.path().join("save.sqlite"), Capture::Dirty);
    app.insert_resource(Clock(4));
    app.world_mut().save().unwrap();
    app.world_mut().remove_resource::<Clock>();
    assert_eq!(app.world_mut().save().unwrap().deletions, 1);
    app.insert_resource(Clock(99));
    app.world_mut().load().unwrap();
    assert!(!app.world().contains_resource::<Clock>());
}

#[derive(Component, Serialize, Deserialize)]
struct Selected;

#[test]
fn filtered_membership_changes_are_detected_without_marking_payloads() {
    for strategy in STRATEGIES {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.add_plugins(SavePlugin::sqlite(directory.path().join("save.sqlite")))
            .persist(
                component::<Position>("position")
                    .filter::<With<Selected>>()
                    .capture(strategy),
            )
            .persist(component::<Selected>("selected"));
        let entity = app.world_mut().spawn(Position { name: 1, value: 10 }).id();
        assert_eq!(app.world_mut().save().unwrap().entities, 0);
        app.world_mut().entity_mut(entity).insert(Selected);
        let stats = app.world_mut().save().unwrap();
        assert_eq!((stats.entities, stats.upserts), (1, 2));
        app.world_mut().entity_mut(entity).remove::<Selected>();
        let stats = app.world_mut().save().unwrap();
        assert_eq!((stats.entities, stats.deletions), (0, 2));
        app.world_mut().entity_mut(entity).insert(Selected);
        app.world_mut().save().unwrap();
        app.world_mut().load().unwrap();
        assert_eq!(
            positions(app.world_mut()),
            [Position { name: 1, value: 10 }]
        );
        assert_eq!(
            app.world_mut()
                .query::<&Selected>()
                .iter(app.world())
                .count(),
            1
        );
    }
}

#[test]
fn disabled_entities_are_persisted_consistently_by_every_strategy() {
    use bevy_ecs::entity_disabling::Disabled;
    for strategy in STRATEGIES {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(&directory.path().join("save.sqlite"), strategy);
        let entity = app
            .world_mut()
            .spawn((Position { name: 1, value: 4 }, Disabled))
            .id();
        assert_eq!(app.world_mut().save().unwrap().upserts, 1);
        app.world_mut().get_mut::<Position>(entity).unwrap().value = 5;
        mark::<Position>(app.world_mut(), entity);
        assert_eq!(app.world_mut().save().unwrap().upserts, 1);
        app.world_mut().load().unwrap();
        assert_eq!(positions(app.world_mut()), [Position { name: 1, value: 5 }]);
    }
}

#[derive(Component, Serialize, Deserialize)]
struct Link {
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    target: Entity,
}

#[derive(Resource, Serialize, Deserialize, MapEntities)]
struct Focus {
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    target: Entity,
}

fn graph_app(path: &Path, strategy: Capture) -> App {
    let mut app = app(path, strategy);
    app.persist(component::<Link>("link").capture(strategy))
        .persist(resource::<Focus>("focus").map_entities());
    app
}

#[test]
fn cyclic_entity_graphs_and_resource_references_remap_across_apps() {
    for strategy in STRATEGIES {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        let mut writer = graph_app(&path, strategy);
        let a = writer
            .world_mut()
            .spawn(Position { name: 1, value: 10 })
            .id();
        let b = writer
            .world_mut()
            .spawn(Position { name: 2, value: 20 })
            .id();
        writer.world_mut().entity_mut(a).insert(Link { target: b });
        writer.world_mut().entity_mut(b).insert(Link { target: a });
        writer.insert_resource(Focus { target: a });
        writer.world_mut().save().unwrap();
        let mut reader = graph_app(&path, strategy);
        for _ in 0..100 {
            reader.world_mut().spawn(Unrelated);
        }
        reader.world_mut().load().unwrap();
        let mut query = reader.world_mut().query::<(Entity, &Position, &Link)>();
        let nodes: Vec<_> = query
            .iter(reader.world())
            .map(|(e, p, l)| (e, p.name, l.target))
            .collect();
        let a = nodes.iter().find(|(_, name, _)| *name == 1).unwrap();
        let b = nodes.iter().find(|(_, name, _)| *name == 2).unwrap();
        assert_eq!((a.2, b.2), (b.0, a.0));
        assert_eq!(reader.world().resource::<Focus>().target, a.0);
        let before: Vec<u8> = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT payload FROM records WHERE type_key = 'focus'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        reader.world_mut().save().unwrap();
        let after: Vec<u8> = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT payload FROM records WHERE type_key = 'focus'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
    }
}

#[test]
fn untouched_references_prevent_incremental_target_deletion() {
    for strategy in [Capture::Changed, Capture::Dirty] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        let mut writer = graph_app(&path, strategy);
        let a = writer
            .world_mut()
            .spawn(Position { name: 1, value: 1 })
            .id();
        let b = writer
            .world_mut()
            .spawn(Position { name: 2, value: 2 })
            .id();
        writer.world_mut().entity_mut(a).insert(Link { target: b });
        writer.world_mut().save().unwrap();
        writer.world_mut().despawn(b);
        assert!(writer.world_mut().save().is_err());
        let mut reader = graph_app(&path, strategy);
        reader.world_mut().load().unwrap();
        assert_eq!(positions(reader.world_mut()).len(), 2);
        writer.world_mut().entity_mut(a).remove::<Link>();
        let retry = writer.world_mut().save().unwrap();
        assert_eq!(
            (retry.generation, retry.entities, retry.deletions),
            (2, 1, 2)
        );
    }
}

#[test]
fn references_outside_selection_fail_without_creating_a_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut app = graph_app(&path, Capture::Snapshot);
    let outside = app.world_mut().spawn(Unrelated).id();
    app.world_mut().spawn(Link { target: outside });
    assert!(matches!(app.world_mut().save(), Err(Error::Encode { .. })));
    assert!(!path.exists());
    app.world_mut()
        .entity_mut(outside)
        .insert(Position { name: 1, value: 2 });
    assert_eq!(app.world_mut().save().unwrap().generation, 1);
}

#[test]
fn decode_failure_preserves_the_entire_live_scope() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut app = app(&path, Capture::Changed);
    let entity = app.world_mut().spawn(Position { name: 1, value: 1 }).id();
    app.insert_resource(Clock(1));
    app.world_mut().save().unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE records SET payload = ?1 WHERE type_key = 'clock'",
            [b"invalid".to_vec()],
        )
        .unwrap();
    app.world_mut().get_mut::<Position>(entity).unwrap().value = 42;
    app.insert_resource(Clock(42));
    assert!(matches!(app.world_mut().load(), Err(Error::Decode { .. })));
    assert_eq!(app.world().get::<Position>(entity).unwrap().value, 42);
    assert_eq!(*app.world().resource::<Clock>(), Clock(42));
}

#[test]
fn dangling_reference_decode_fails_before_entities_are_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut app = graph_app(&path, Capture::Dirty);
    let entity = app.world_mut().spawn(Position { name: 1, value: 1 }).id();
    app.world_mut()
        .entity_mut(entity)
        .insert(Link { target: entity });
    app.world_mut().save().unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE records SET payload = ?1 WHERE type_key = 'link'",
            [br#"{"target":999999}"#.to_vec()],
        )
        .unwrap();
    assert!(matches!(app.world_mut().load(), Err(Error::Decode { .. })));
    assert!(app.world().get::<Position>(entity).is_some());
}

#[test]
fn sqlite_failure_rolls_back_prior_writes_and_preserves_pending_changes() {
    for strategy in STRATEGIES {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        let mut writer = app(&path, strategy);
        let entity = writer
            .world_mut()
            .spawn((Position { name: 1, value: 1 }, Label("old".into())))
            .id();
        writer.world_mut().save().unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_label BEFORE UPDATE ON records WHEN NEW.type_key = 'label'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        writer
            .world_mut()
            .get_mut::<Position>(entity)
            .unwrap()
            .value = 2;
        writer.world_mut().get_mut::<Label>(entity).unwrap().0 = "new".into();
        mark::<Position>(writer.world_mut(), entity);
        mark::<Label>(writer.world_mut(), entity);
        assert!(matches!(writer.world_mut().save(), Err(Error::Sqlite(_))));
        let mut reader = app(&path, strategy);
        reader.world_mut().load().unwrap();
        assert_eq!(
            positions(reader.world_mut()),
            [Position { name: 1, value: 1 }]
        );
        connection
            .execute_batch("DROP TRIGGER reject_label")
            .unwrap();
        let retry = writer.world_mut().save().unwrap();
        assert_eq!((retry.generation, retry.upserts), (2, 2));
        reader.world_mut().load().unwrap();
        assert_eq!(
            positions(reader.world_mut()),
            [Position { name: 1, value: 2 }]
        );
        assert_eq!(
            reader
                .world_mut()
                .query::<&Label>()
                .single(reader.world())
                .unwrap()
                .0,
            "new"
        );
    }
}

#[derive(Component, Serialize, Deserialize)]
struct Fallible {
    #[serde(serialize_with = "serialize_fallible")]
    value: u64,
}

fn serialize_fallible<S: serde::Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    if *value == u64::MAX {
        return Err(serde::ser::Error::custom("injected encoding failure"));
    }
    serializer.serialize_u64(*value)
}

#[test]
fn encoding_failure_does_not_acknowledge_dirty_marks() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(directory.path().join("save.sqlite")))
        .persist(component::<Fallible>("fallible").capture(Capture::Dirty));
    let entity = app.world_mut().spawn(Fallible { value: 1 }).id();
    app.world_mut().save().unwrap();
    app.world_mut().get_mut::<Fallible>(entity).unwrap().value = u64::MAX;
    mark::<Fallible>(app.world_mut(), entity);
    assert!(matches!(app.world_mut().save(), Err(Error::Encode { .. })));
    app.world_mut().get_mut::<Fallible>(entity).unwrap().value = 2;
    assert_eq!(app.world_mut().save().unwrap().upserts, 1);
    app.world_mut().load().unwrap();
    assert_eq!(
        app.world_mut()
            .query::<&Fallible>()
            .single(app.world())
            .unwrap()
            .value,
        2
    );
}

#[test]
fn concurrent_writer_requires_reloading_before_incremental_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut a = app(&path, Capture::Changed);
    a.world_mut().spawn(Position { name: 1, value: 1 });
    a.world_mut().save().unwrap();
    let mut b = app(&path, Capture::Changed);
    b.world_mut().load().unwrap();
    a.world_mut().save().unwrap();
    assert!(matches!(
        b.world_mut().save(),
        Err(Error::Conflict {
            expected: 1,
            actual: 2
        })
    ));
    b.world_mut().load().unwrap();
    assert_eq!(b.world_mut().save().unwrap().generation, 3);
}

#[test]
fn wrong_manifest_and_foreign_databases_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut writer = app(&path, Capture::Snapshot);
    writer.world_mut().save().unwrap();
    let mut different = App::new();
    different
        .add_plugins(SavePlugin::sqlite(&path))
        .persist(component::<Position>("renamed"));
    assert!(matches!(different.world_mut().load(), Err(Error::Manifest)));
    assert!(matches!(
        different.world_mut().save(),
        Err(Error::LoadRequired)
    ));
    let foreign = directory.path().join("foreign.sqlite");
    let connection = Connection::open(&foreign).unwrap();
    connection
        .execute_batch("CREATE TABLE important(value INTEGER); INSERT INTO important VALUES (123);")
        .unwrap();
    let mut writer = app(&foreign, Capture::Snapshot);
    assert!(matches!(writer.world_mut().save(), Err(Error::Format)));
    let value: i64 = connection
        .query_row("SELECT value FROM important", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 123);
}

#[test]
fn missing_file_load_does_not_create_it_and_missing_plugin_is_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing.sqlite");
    let mut app = app(&path, Capture::Snapshot);
    assert_eq!(app.world_mut().load().unwrap(), LoadOutcome::Missing);
    assert!(!path.exists());
    assert!(matches!(World::new().save(), Err(Error::MissingPlugin)));
}

#[test]
fn first_save_in_a_new_app_cannot_replace_the_old_scope() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut first = app(&path, Capture::Dirty);
    first.world_mut().spawn(Position { name: 1, value: 1 });
    first.world_mut().save().unwrap();
    let mut second = app(&path, Capture::Dirty);
    second.world_mut().spawn(Position { name: 2, value: 2 });
    assert!(matches!(
        second.world_mut().save(),
        Err(Error::LoadRequired)
    ));
    first.world_mut().load().unwrap();
    assert_eq!(
        positions(first.world_mut()),
        [Position { name: 1, value: 1 }]
    );
    second.world_mut().load().unwrap();
    assert_eq!(second.world_mut().save().unwrap().generation, 2);
}

#[test]
fn failed_load_blocks_writes_until_a_successful_reload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut writer = app(&path, Capture::Changed);
    writer.insert_resource(Clock(7));
    writer.world_mut().save().unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE records SET payload = X'00' WHERE type_key = 'clock'",
            [],
        )
        .unwrap();
    assert!(matches!(
        writer.world_mut().load(),
        Err(Error::Decode { .. })
    ));
    for _ in 0..3 {
        assert!(matches!(
            writer.world_mut().save(),
            Err(Error::LoadRequired)
        ));
    }
    let generation: i64 = connection
        .query_row("SELECT generation FROM meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(generation, 1);
    connection
        .execute(
            "UPDATE records SET payload = ?1 WHERE type_key = 'clock'",
            [b"9".to_vec()],
        )
        .unwrap();
    writer.world_mut().load().unwrap();
    assert_eq!(writer.world().resource::<Clock>().0, 9);
    assert_eq!(writer.world_mut().save().unwrap().generation, 2);
}

#[test]
fn another_writer_creating_a_previously_missing_save_is_a_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut first = app(&path, Capture::Changed);
    assert_eq!(first.world_mut().load().unwrap(), LoadOutcome::Missing);
    let mut second = app(&path, Capture::Changed);
    second.world_mut().save().unwrap();
    assert!(matches!(
        first.world_mut().save(),
        Err(Error::Conflict {
            expected: 0,
            actual: 1
        })
    ));
}

#[test]
fn corrupt_orphan_record_is_rejected_even_if_foreign_keys_were_disabled() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut app = app(&path, Capture::Snapshot);
    app.world_mut().save().unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    connection
        .execute(
            "INSERT INTO records VALUES (123, 'position', ?1)",
            params![br#"{"name":1,"value":2}"#.to_vec()],
        )
        .unwrap();
    assert!(matches!(app.world_mut().load(), Err(Error::InvalidData(_))));
}

#[test]
fn empty_files_and_directories_are_failed_loads_not_missing_saves() {
    let directory = tempfile::tempdir().unwrap();
    let empty = directory.path().join("empty.sqlite");
    std::fs::write(&empty, []).unwrap();
    for path in [&empty, directory.path()] {
        let mut app = app(path, Capture::Snapshot);
        assert!(app.world_mut().load().is_err());
        assert!(matches!(app.world_mut().save(), Err(Error::LoadRequired)));
    }
    assert_eq!(std::fs::metadata(empty).unwrap().len(), 0);
}
