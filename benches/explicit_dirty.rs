use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{AppSaveExt, Capture, Dirty, SavePlugin, WorldSaveExt, component};
use divan::{Bencher, black_box};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Component, Serialize, Deserialize)]
struct Machine {
    ticks: u64,
    inventory: [u64; 8],
}

#[derive(Resource)]
struct Edits(usize);

fn simulate(
    mut machines: Query<(Entity, &mut Machine)>,
    edits: Res<Edits>,
    mut dirty: Dirty<Machine>,
) {
    for (entity, mut machine) in machines.iter_mut().take(edits.0) {
        machine.ticks += 1;
        dirty.mark(entity);
    }
}

fn simulate_without_tracking(mut machines: Query<(Entity, &mut Machine)>, edits: Res<Edits>) {
    for (_, mut machine) in machines.iter_mut().take(edits.0) {
        machine.ticks += 1;
    }
}

struct Case {
    app: App,
    _directory: TempDir,
}

fn setup(entities: usize, percent: usize) -> Case {
    let directory = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.insert_resource(Edits(entities * percent / 100));
    app.world_mut()
        .spawn_batch((0..entities).map(|index| Machine {
            ticks: 0,
            inventory: [index as u64; 8],
        }));
    app.add_plugins(SavePlugin::sqlite(directory.path().join("save.sqlite")))
        .persist(component::<Machine>("machine").capture(Capture::Dirty))
        .add_systems(Update, simulate);
    app.update();
    assert_eq!(app.world_mut().save().unwrap().upserts, entities);
    Case {
        app,
        _directory: directory,
    }
}

#[divan::bench(consts = [1_000, 100_000], args = [0, 1, 100], sample_size = 1, sample_count = 20)]
fn commit<const N: usize>(bencher: Bencher, percent: usize) {
    bencher
        .with_inputs(|| setup(N, percent))
        .bench_local_refs(|case| {
            case.app.update();
            black_box(case.app.world_mut().save().unwrap())
        });
}

#[divan::bench(args = [0, 1, 100])]
fn gameplay_with_marking(bencher: Bencher, percent: usize) {
    let mut case = setup(100_000, percent);
    bencher.bench_local(|| {
        case.app.update();
    });
}

#[divan::bench(args = [0, 1, 100])]
fn gameplay_without_saver(bencher: Bencher, percent: usize) {
    let mut app = App::new();
    app.insert_resource(Edits(100_000 * percent / 100))
        .add_systems(Update, simulate_without_tracking);
    app.world_mut()
        .spawn_batch((0..100_000).map(|index| Machine {
            ticks: 0,
            inventory: [index as u64; 8],
        }));
    app.update();
    bencher.bench_local(|| {
        app.update();
    });
}

#[divan::bench(args = [0, 1, 100], sample_size = 1, sample_count = 10)]
fn sustained(bencher: Bencher, percent: usize) {
    bencher
        .with_inputs(|| setup(100_000, percent))
        .bench_local_refs(|case| {
            for tick in 1..=120 {
                case.app.update();
                if tick % 10 == 0 {
                    black_box(case.app.world_mut().save().unwrap());
                }
            }
        });
}

fn main() {
    divan::main();
}
