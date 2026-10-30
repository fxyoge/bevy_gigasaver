//! cargo run --example headless -- /tmp/gigasaver-example.sqlite
use bevy_app::{App, AppExit, Update};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{
    AppSaveExt, Capture, LoadOutcome, SaveOpened, SavePlugin, SaveStatus, component,
};
use bevy_time::TimePlugin;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

#[derive(Component, Serialize, Deserialize)]
struct Node {
    ticks: u64,
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    peer: Entity,
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct Account {
    credits: u64,
}

#[derive(Component)]
struct Transient;

fn fresh(status: Res<SaveStatus>) -> bool {
    status.load() == Some(&Ok(LoadOutcome::Missing))
}

fn initialize(mut commands: Commands) {
    let a = commands.spawn_empty().id();
    let b = commands.spawn_empty().id();
    commands.entity(a).insert(Node { ticks: 0, peer: b });
    commands.entity(b).insert(Node { ticks: 0, peer: a });
}

fn simulate(mut nodes: Query<&mut Node>) {
    for mut node in &mut nodes {
        node.ticks += 1;
    }
}

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "save.sqlite".into());
    let mut app = App::new();
    app.add_plugins(TimePlugin);
    let plugin = SavePlugin::sqlite(&path).autosave(Duration::from_millis(250));
    app.add_plugins(plugin)
        .persist_resource::<Account>("account")
        .add_systems(
            SaveOpened,
            (|mut commands: Commands| {
                commands.insert_resource(Account { credits: 10 });
            })
            .run_if(fresh),
        );
    app.persist(component::<Node>("node").capture(Capture::Changed))
        .add_systems(SaveOpened, initialize.run_if(fresh))
        .add_systems(Update, simulate);
    app.world_mut().spawn(Transient);
    app.update();
    let status = app.world().resource::<SaveStatus>();
    println!("{:?}: {:?}", status.path(), status.load());
    if !status.can_save() {
        std::process::exit(1);
    }
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(10));
        app.update();
    }
    app.world_mut().write_message(AppExit::Success);
    app.update();
    let status = app.world().resource::<SaveStatus>();
    println!("Final save: {:?}", status.last_save());
    if app.should_exit().is_some_and(|exit| exit.is_error()) {
        std::process::exit(1);
    }
}
