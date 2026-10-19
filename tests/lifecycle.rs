use bevy_app::{App, AppExit, Last, Startup, Update};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{
    AppSaveExt, Capture, Error, LoadOutcome, SaveOpened, SavePlugin, SaveRequest, SaveSet,
    SaveStatus, WorldSaveExt, component,
};
use bevy_time::{Real, Time};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Resource, Default, Serialize, Deserialize)]
struct Counter(u64);

#[derive(Resource)]
struct Ready(bool);

#[derive(Component, Serialize, Deserialize)]
struct Peer {
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    target: Entity,
}

#[derive(Component)]
struct Transient;

#[derive(Resource, Default, Serialize, Deserialize)]
struct Fallible(#[serde(serialize_with = "fallible")] bool);

fn fallible<S: serde::Serializer>(value: &bool, serializer: S) -> Result<S::Ok, S::Error> {
    if !value {
        return Err(serde::ser::Error::custom("injected encoding failure"));
    }
    serializer.serialize_bool(*value)
}

fn fresh(status: Res<SaveStatus>) -> bool {
    status.load() == Some(&Ok(LoadOutcome::Missing))
}

fn initialize(mut commands: Commands) {
    commands.insert_resource(Counter(10));
    let a = commands.spawn_empty().id();
    let b = commands.spawn_empty().id();
    commands.entity(a).insert(Peer { target: b });
    commands.entity(b).insert(Peer { target: a });
}

fn app(path: &Path) -> App {
    let mut app = App::new();
    app.init_resource::<Time<Real>>()
        .insert_resource(Ready(true))
        .add_plugins(
            SavePlugin::sqlite(path)
                .autosave(Duration::from_secs(1))
                .enabled_when(|ready: Res<Ready>| ready.0),
        )
        .persist_resource::<Counter>("counter")
        .persist(component::<Peer>("peer").capture(Capture::Changed))
        .add_systems(SaveOpened, initialize.run_if(fresh));
    app
}

fn tick(app: &mut App, duration: Duration) {
    app.world_mut()
        .resource_mut::<Time<Real>>()
        .advance_by(duration);
    app.update();
}

fn request(app: &mut App, request: SaveRequest) {
    app.world_mut().write_message(request);
    app.update();
}

fn generation(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT generation FROM meta", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn plugin_alone_restores_before_simulation_and_autosaves_deferred_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut first = app(&path);
    let transient = first.world_mut().spawn(Transient).id();
    first.update();
    assert!(first.world().resource::<SaveStatus>().can_save());
    assert!(!path.exists());
    first.add_systems(Update, |mut commands: Commands| {
        commands.insert_resource(Counter(42));
    });
    tick(&mut first, Duration::from_secs(1));
    assert_eq!(generation(&path), 1);
    assert!(first.world().get::<Transient>(transient).is_some());
    let mut second = app(&path);
    second.add_systems(Startup, |counter: Res<Counter>| assert_eq!(counter.0, 42));
    second.update();
    assert_eq!(
        second.world().resource::<SaveStatus>().load(),
        Some(&Ok(LoadOutcome::Loaded))
    );
    let pairs: Vec<_> = second
        .world_mut()
        .query::<(Entity, &Peer)>()
        .iter(second.world())
        .map(|(entity, peer)| (entity, peer.target))
        .collect();
    assert_eq!(pairs.len(), 2); // fresh initialization did not run again
    for (entity, target) in pairs {
        assert_eq!(second.world().get::<Peer>(target).unwrap().target, entity);
    }
    assert_eq!(second.world().resource::<Counter>().0, 42);
}

#[test]
fn readiness_and_real_elapsed_deadlines_control_saves_without_frame_retries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut app = app(&path);
    app.update();
    app.world_mut().resource_mut::<Ready>().0 = false;
    tick(&mut app, Duration::from_secs(1));
    assert!(!path.exists());
    assert!(
        app.world()
            .resource::<SaveStatus>()
            .last_save()
            .unwrap()
            .is_err()
    );
    app.world_mut().resource_mut::<Ready>().0 = true;
    for _ in 0..100 {
        app.update();
    }
    assert!(!path.exists());
    tick(&mut app, Duration::from_secs(10));
    assert_eq!(generation(&path), 1); // one commit, not ten catch-up commits
    for _ in 0..100 {
        app.update();
    }
    assert_eq!(generation(&path), 1);
}

#[test]
fn failed_load_stays_protected_across_autosave_deadlines_and_explicit_requests() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut first = app(&path);
    first.update();
    request(&mut first, SaveRequest::Save);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE records SET payload = X'00' WHERE type_key = 'counter'",
            [],
        )
        .unwrap();
    let mut second = app(&path);
    second.insert_resource(Counter(99));
    second.update();
    assert!(
        second
            .world()
            .resource::<SaveStatus>()
            .load()
            .unwrap()
            .is_err()
    );
    for _ in 0..10 {
        tick(&mut second, Duration::from_secs(1));
    }
    request(&mut second, SaveRequest::Save);
    assert_eq!(generation(&path), 1);
    assert_eq!(second.world().resource::<Counter>().0, 99);
    assert!(!second.world().resource::<SaveStatus>().can_save());
    connection
        .execute(
            "UPDATE records SET payload = ?1 WHERE type_key = 'counter'",
            [b"23".to_vec()],
        )
        .unwrap();
    request(&mut second, SaveRequest::Load);
    assert_eq!(second.world().resource::<Counter>().0, 23);
    tick(&mut second, Duration::from_secs(1));
    assert_eq!(generation(&path), 2);
}

#[test]
fn session_switches_and_close_isolate_files_and_preserve_unrelated_entities() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut app = app(&a);
    let transient = app.world_mut().spawn(Transient).id();
    app.update();
    app.world_mut().resource_mut::<Counter>().0 = 31;
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(generation(&a), 1);
    assert_eq!(app.world().resource::<Counter>().0, 10); // fresh, not copied from A
    app.world_mut().resource_mut::<Counter>().0 = 52;
    request(&mut app, SaveRequest::Open(a.clone()));
    assert_eq!(app.world().resource::<Counter>().0, 31);
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(app.world().resource::<Counter>().0, 52);
    request(&mut app, SaveRequest::Close);
    assert_eq!(app.world().resource::<Counter>().0, 0);
    assert_eq!(
        app.world_mut().query::<&Peer>().iter(app.world()).count(),
        0
    );
    assert!(app.world().get::<Transient>(transient).is_some());
    assert!(matches!(app.world_mut().save(), Err(Error::Closed)));
    request(&mut app, SaveRequest::Open(b)); // reopening the same path works
    assert_eq!(app.world().resource::<Counter>().0, 52);
}

#[test]
fn failed_outgoing_save_keeps_session_and_pending_changes_for_retry() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut app = app(&a);
    app.update();
    request(&mut app, SaveRequest::Save);
    app.world_mut().resource_mut::<Counter>().0 = 88;
    let connection = Connection::open(&a).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_commit BEFORE UPDATE ON meta BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(a.as_path())
    );
    assert_eq!(app.world().resource::<Counter>().0, 88);
    assert!(!b.exists());
    assert_eq!(generation(&a), 1);
    // Frames before the next deadline must not retry the failed operation.
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(generation(&a), 1);
    connection
        .execute_batch("DROP TRIGGER reject_commit")
        .unwrap();
    request(&mut app, SaveRequest::Open(b));
    assert_eq!(generation(&a), 2);
    let mut restored = self::app(&a);
    restored.update();
    assert_eq!(restored.world().resource::<Counter>().0, 88);
}

#[test]
fn corrupt_destination_keeps_original_session_and_never_writes_destination() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut target = app(&b);
    target.update();
    request(&mut target, SaveRequest::Save);
    let connection = Connection::open(&b).unwrap();
    connection
        .execute(
            "UPDATE records SET payload = X'00' WHERE type_key = 'counter'",
            [],
        )
        .unwrap();
    let mut app = app(&a);
    app.update();
    app.world_mut().resource_mut::<Counter>().0 = 77;
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(a.as_path())
    );
    assert!(app.world().resource::<SaveStatus>().can_save());
    assert_eq!(app.world().resource::<Counter>().0, 77);
    for _ in 0..3 {
        tick(&mut app, Duration::from_secs(1));
    }
    assert_eq!(generation(&b), 1);
    assert_eq!(generation(&a), 4);
}

#[test]
fn exit_saves_before_ordered_teardown_and_keeps_failed_saves_live() {
    for fail in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("save.sqlite");
        let mut app = app(&path);
        app.update();
        request(&mut app, SaveRequest::Save);
        app.world_mut().resource_mut::<Counter>().0 = 64;
        let connection = Connection::open(&path).unwrap();
        if fail {
            connection.execute_batch("CREATE TRIGGER reject_commit BEFORE UPDATE ON meta BEGIN SELECT RAISE(ABORT, 'failure'); END;").unwrap();
        }
        app.add_systems(
            Last,
            (|mut commands: Commands| {
                commands.remove_resource::<Counter>();
            })
            .after(SaveSet::Save)
            .run_if(bevy_ecs::schedule::common_conditions::on_message::<AppExit>),
        );
        app.world_mut().write_message(AppExit::Success);
        app.update();
        assert_eq!(
            app.should_exit(),
            if fail { None } else { Some(AppExit::Success) }
        );
        assert_eq!(
            app.world()
                .resource::<SaveStatus>()
                .last_save()
                .unwrap()
                .is_err(),
            fail
        );
        let mut reader = self::app(&path);
        reader.update();
        assert_eq!(
            reader.world().resource::<Counter>().0,
            if fail { 10 } else { 64 }
        );
    }
}

#[test]
fn opening_later_and_request_order_need_no_custom_persistence_system() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(&a).load_on_start(false))
        .persist_resource::<Counter>("counter")
        .add_systems(
            SaveOpened,
            (|mut commands: Commands| {
                commands.insert_resource(Counter(5));
            })
            .run_if(fresh),
        );
    app.update();
    assert!(app.world().resource::<SaveStatus>().load().is_none());
    app.world_mut().write_message(SaveRequest::Open(b.clone()));
    app.world_mut().write_message(SaveRequest::Save);
    app.world_mut().write_message(SaveRequest::Close);
    app.update();
    assert!(!a.exists());
    assert_eq!(generation(&b), 2);
    assert!(!app.world().resource::<SaveStatus>().can_save());
}

#[test]
fn exit_without_opening_a_session_does_not_create_a_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(&path).load_on_start(false));
    app.world_mut().write_message(AppExit::Success);
    app.update();
    assert!(!path.exists());
    assert_eq!(app.should_exit(), Some(AppExit::Success));
}

#[test]
fn readiness_blocks_session_switch_and_a_save_plus_exit_commits_once() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut app = app(&a);
    app.update();
    app.world_mut().resource_mut::<Ready>().0 = false;
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(a.as_path())
    );
    assert!(!b.exists());
    assert!(!a.exists());
    app.world_mut().resource_mut::<Ready>().0 = true;
    app.world_mut().write_message(SaveRequest::Save);
    app.world_mut().write_message(AppExit::Success);
    tick(&mut app, Duration::from_secs(1));
    assert_eq!(generation(&a), 1); // request, due interval, and exit coalesce
}

#[test]
fn a_failed_first_save_cannot_be_discarded_by_switching_files() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut app = App::new();
    app.add_plugins(SavePlugin::sqlite(&a).load_on_start(false))
        .persist_resource::<Fallible>("fallible")
        .insert_resource(Fallible(false));
    assert!(matches!(app.world_mut().save(), Err(Error::Encode { .. })));
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(a.as_path())
    );
    assert!(!app.world().resource::<Fallible>().0);
    assert!(!b.exists());
    app.world_mut().resource_mut::<Fallible>().0 = true;
    request(&mut app, SaveRequest::Open(b.clone()));
    assert_eq!(generation(&a), 1);
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(b.as_path())
    );
}
