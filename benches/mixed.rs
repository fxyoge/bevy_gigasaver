use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{AppSaveExt, Capture, Dirty, SavePlugin, WorldSaveExt, component};
use divan::{Bencher, black_box};
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize)]
struct Player {
    health: u32,
}

#[derive(Component, Serialize, Deserialize)]
struct Machine {
    production: u64,
    #[entities]
    #[serde(with = "bevy_gigasaver::entity")]
    owner: Entity,
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct Research {
    tick: u64,
}

fn simulate(
    mut machines: Query<(Entity, &mut Machine)>,
    mut dirty: Dirty<Machine>,
    mut research: ResMut<Research>,
) {
    research.tick += 1;
    for (entity, mut machine) in machines.iter_mut().take(10) {
        machine.production += 1;
        dirty.mark(entity);
    }
}

#[divan::bench(args = [1_000, 100_000], sample_size = 1, sample_count = 20)]
fn commit(bencher: Bencher, entities: usize) {
    bencher
        .with_inputs(|| {
            let directory = tempfile::tempdir().unwrap();
            let mut app = App::new();
            app.add_plugins(SavePlugin::sqlite(directory.path().join("save.sqlite")))
                .persist(component::<Player>("player"))
                .persist(component::<Machine>("machine").capture(Capture::Dirty))
                .persist_resource::<Research>("research")
                .insert_resource(Research { tick: 0 })
                .add_systems(Update, simulate);
            app.update();
            let owner = app.world_mut().spawn(Player { health: 100 }).id();
            app.world_mut().spawn_batch((0..entities).map(|_| Machine {
                production: 0,
                owner,
            }));
            app.update();
            let initial = app.world_mut().save().unwrap();
            assert_eq!(initial.entities, entities + 1);
            assert_eq!(initial.upserts, entities + 2);
            (app, directory)
        })
        .bench_local_refs(|(app, _directory)| {
            app.update();
            black_box(app.world_mut().save().unwrap())
        });
}

fn main() {
    divan::main();
}
