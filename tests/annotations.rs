use bevy_app::App;
use bevy_ecs::prelude::*;
use bevy_gigasaver::{
    Checkpoint, LoadOutcome, SavePlugin, SaveRequest, SaveStatus, WorldSaveExt, save,
};
use bevy_transform::components::Transform;
use serde::{Deserialize, Serialize};

#[save(resource, key = "tiny.score")]
#[derive(Resource, Default, Serialize, Deserialize)]
struct Score(u64);

#[derive(Resource)]
struct Platform(u64, usize);

#[save(resource, key = "tiny.session")]
#[derive(Resource, Serialize, Deserialize)]
struct SessionSeed(u64);
impl FromWorld for SessionSeed {
    fn from_world(world: &mut World) -> Self {
        let mut platform = world.resource_mut::<Platform>();
        platform.1 += 1;
        Self(platform.0)
    }
}

#[save(component, key = "tiny.actor")]
#[derive(Component, Serialize, Deserialize)]
struct Actor {
    #[serde(with = "bevy_gigasaver::entity")]
    peer: Entity,
}

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(Platform(17, 0))
        .add_plugins(SavePlugin::default());
    app.update();
    app
}
fn request(app: &mut App, request: SaveRequest) {
    app.world_mut().write_message(request);
    app.update();
    app.update();
}

#[test]
fn annotated_game_needs_only_the_plugin_for_fresh_restore_switch_and_close() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a.sqlite");
    let b = directory.path().join("b.sqlite");
    let mut first = app();
    assert_eq!(first.world().resource::<SessionSeed>().0, 17);
    request(&mut first, SaveRequest::Open(a.clone()));
    first.world_mut().resource_mut::<Score>().0 = 42;
    let actor = first
        .world_mut()
        .spawn((Name::new("actor"), Transform::from_xyz(1., 2., 3.)))
        .id();
    first
        .world_mut()
        .entity_mut(actor)
        .insert(Actor { peer: actor });
    let camera = first
        .world_mut()
        .spawn((Name::new("camera"), Transform::default()))
        .id();
    request(&mut first, SaveRequest::Save);
    assert!(
        first
            .world()
            .resource::<SaveStatus>()
            .last_save()
            .unwrap()
            .is_ok()
    );

    let mut second = app();
    request(&mut second, SaveRequest::Open(a.clone()));
    assert_eq!(second.world().resource::<Score>().0, 42);
    let (entity, restored_actor, name, transform) = second
        .world_mut()
        .query::<(Entity, &Actor, &Name, &Transform)>()
        .single(second.world())
        .unwrap();
    assert_eq!(entity, restored_actor.peer);
    assert_eq!(name.as_str(), "actor");
    assert_eq!(transform.translation.x, 1.);
    request(&mut first, SaveRequest::Open(b));
    assert_eq!(first.world().resource::<Score>().0, 0);
    assert!(first.world().get_entity(actor).is_err());
    assert!(first.world().get_entity(camera).is_ok());
    first.world_mut().resource_mut::<Score>().0 = 99;
    request(&mut first, SaveRequest::Close);
    assert_eq!(first.world().resource::<Score>().0, 0);
    assert_eq!(first.world().resource::<SessionSeed>().0, 17);
    request(&mut first, SaveRequest::Open(a));
    assert_eq!(first.world().resource::<Score>().0, 42);
}

#[test]
fn failed_restore_leaves_resources_and_entities_intact_without_running_constructors() {
    let mut app = app();
    app.world_mut().resource_mut::<Score>().0 = 29;
    app.world_mut().resource_mut::<SessionSeed>().0 = 123;
    let actor = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(actor)
        .insert(Actor { peer: actor });
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    let count = app.world().entities().len();
    let constructors = app.world().resource::<Platform>().1;
    let mut wire = serde_json::to_value(checkpoint).unwrap();
    wire["data"]["tiny.score"][0][1] = serde_json::json!([123]);
    let corrupt: Checkpoint = serde_json::from_value(wire).unwrap();
    assert!(corrupt.restore(app.world_mut()).is_err());
    assert_eq!(app.world().resource::<Score>().0, 29);
    assert_eq!(app.world().resource::<SessionSeed>().0, 123);
    assert!(app.world().get::<Actor>(actor).is_some());
    assert_eq!(app.world().entities().len(), count);
    assert_eq!(app.world().resource::<Platform>().1, constructors);
}

#[test]
fn finishing_a_deferred_open_never_creates_an_unscheduled_save() {
    #[derive(Resource)]
    struct AssetsReady(bool);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("deferred.sqlite");
    let mut app = App::new();
    app.insert_resource(Platform(17, 0))
        .insert_resource(AssetsReady(false))
        .add_plugins(SavePlugin::default().enabled_when(|ready: Res<AssetsReady>| ready.0));
    request(&mut app, SaveRequest::Open(path.clone()));
    assert!(app.world().resource::<SaveStatus>().is_loading());
    app.world_mut().resource_mut::<AssetsReady>().0 = true;
    app.update();
    assert!(app.world().resource::<SaveStatus>().can_save());
    assert!(app.world().resource::<SaveStatus>().last_save().is_none());
    assert!(!path.exists());
}

#[test]
fn direct_load_uses_the_same_fresh_resource_contract_as_requests() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct.sqlite");
    let mut app = App::new();
    app.insert_resource(Platform(17, 0))
        .add_plugins(SavePlugin::sqlite(&path).load_on_start(false))
        .insert_resource(Score(99));
    assert_eq!(app.world_mut().load().unwrap(), LoadOutcome::Missing);
    assert_eq!(app.world().resource::<Score>().0, 0);
    assert_eq!(app.world().resource::<SessionSeed>().0, 17);
    assert_eq!(app.world().resource::<Platform>().1, 1);
    assert!(!path.exists());
}
