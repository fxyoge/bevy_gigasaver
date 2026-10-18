// Setup/baseline/destruction are untimed; filesystem caches are warm.

use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{AppSaveExt, SavePlugin, WorldSaveExt, component};
use divan::{Bencher, black_box};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Component, Serialize, Deserialize)]
struct Machine {
    ticks: u64,
    inventory: [u64; 8],
}

#[derive(Resource)]
struct Edits(usize);

fn simulate(mut machines: Query<&mut Machine>, edits: Res<Edits>) {
    for mut machine in machines.iter_mut().take(edits.0) {
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
    app.insert_resource(Edits(entities * percent / 100))
        .add_systems(Update, simulate);
    app.world_mut()
        .spawn_batch((0..entities).map(|index| Machine {
            ticks: 0,
            inventory: [index as u64; 8],
        }));
    app.add_plugins(SavePlugin::sqlite(directory.path().join("save.sqlite")))
        .persist(component::<Machine>("machine"));
    app.update();
    let initial = app.world_mut().save().unwrap();
    assert_eq!(initial.upserts, entities);
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

struct Direct {
    app: App,
    query: QueryState<(Entity, &'static Machine)>,
    connection: Connection,
    _directory: TempDir,
}

impl Direct {
    fn write(&mut self) -> usize {
        let rows: Vec<_> = self
            .query
            .iter(self.app.world())
            .map(|(entity, machine)| (entity.index_u32(), serde_json::to_vec(machine).unwrap()))
            .collect();
        let transaction = self.connection.transaction().unwrap();
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO records VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET payload = excluded.payload").unwrap();
            for (id, payload) in &rows {
                insert.execute(params![id, payload]).unwrap();
            }
        }
        transaction.commit().unwrap();
        rows.len()
    }
}

// Smaller schema: no manifest, persistent identity, reference edges, or generation.
#[divan::bench(consts = [1_000, 100_000], args = [0, 1, 100], sample_size = 1, sample_count = 20)]
fn direct_sqlite<const N: usize>(bencher: Bencher, percent: usize) {
    bencher
        .with_inputs(|| {
            let directory = tempfile::tempdir().unwrap();
            let connection = Connection::open(directory.path().join("reference.sqlite")).unwrap();
            connection
                .execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
                     CREATE TABLE records(id INTEGER PRIMARY KEY, payload BLOB NOT NULL) STRICT;",
                )
                .unwrap();
            let mut app = App::new();
            app.insert_resource(Edits(N * percent / 100))
                .add_systems(Update, simulate);
            app.world_mut().spawn_batch((0..N).map(|index| Machine {
                ticks: 0,
                inventory: [index as u64; 8],
            }));
            app.update();
            let query = app.world_mut().query();
            let mut case = Direct {
                app,
                query,
                connection,
                _directory: directory,
            };
            case.write();
            case
        })
        .bench_local_refs(|case| {
            case.app.update();
            black_box(case.write())
        });
}

#[divan::bench(consts = [1_000, 100_000], sample_size = 1, sample_count = 20)]
fn restore<const N: usize>(bencher: Bencher) {
    bencher
        .with_inputs(|| setup(N, 0))
        .bench_local_refs(|case| {
            case.app.world_mut().load().unwrap();
            black_box(case.app.world().entities().len());
        });
}

fn main() {
    divan::main();
}
