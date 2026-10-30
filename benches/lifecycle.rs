use bevy_app::{App, Update};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{AppSaveExt, Capture, SavePlugin, SaveStatus, WorldSaveExt, component};
use bevy_time::{Real, Time};
use divan::{Bencher, black_box};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Component, Serialize, Deserialize)]
struct Record {
    ticks: u64,
    values: [u64; 8],
}

fn simulate(mut records: Query<&mut Record>) {
    for mut record in records.iter_mut().take(10) {
        record.ticks += 1;
    }
}

fn setup(entities: usize) -> (App, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.init_resource::<Time<Real>>()
        .add_plugins(
            SavePlugin::sqlite(directory.path().join("save.sqlite"))
                .autosave(Duration::from_secs(1)),
        )
        .persist(component::<Record>("record").capture(Capture::Changed));
    app.update();
    app.world_mut()
        .spawn_batch((0..entities).map(|index| Record {
            ticks: 0,
            values: [index as u64; 8],
        }));
    app.update();
    assert_eq!(
        app.world_mut().query::<&Record>().iter(app.world()).count(),
        entities
    );
    (app, directory)
}

#[divan::bench(sample_count = 100)]
fn bare_update(bencher: Bencher) {
    bencher
        .with_inputs(|| {
            let mut app = App::new();
            app.update();
            app
        })
        .bench_local_refs(|app| app.update());
}

#[divan::bench(sample_count = 100)]
fn idle_update(bencher: Bencher) {
    bencher
        .with_inputs(|| setup(0))
        .bench_local_refs(|(app, _)| app.update());
}

#[divan::bench(args = [1_000, 10_000], sample_size = 1, sample_count = 20)]
fn full_save(bencher: Bencher, entities: usize) {
    bencher
        .with_inputs(|| setup(entities))
        .bench_local_refs(|(app, _)| black_box(app.world_mut().save().unwrap()));
}

#[divan::bench(args = [1_000, 10_000], sample_size = 1, sample_count = 20)]
fn autosave_frame(bencher: Bencher, entities: usize) {
    bencher
        .with_inputs(|| {
            let (mut app, directory) = setup(entities);
            assert_eq!(app.world_mut().save().unwrap().upserts, entities);
            app.add_systems(Update, simulate);
            (app, directory)
        })
        .bench_local_refs(|(app, _)| {
            app.world_mut()
                .resource_mut::<Time<Real>>()
                .advance_by(Duration::from_secs(1));
            app.update();
            black_box(app.world().resource::<SaveStatus>().last_save());
        });
}

#[divan::bench(args = [1_000, 10_000], sample_size = 1, sample_count = 20)]
fn load(bencher: Bencher, entities: usize) {
    bencher
        .with_inputs(|| {
            let (mut writer, directory) = setup(entities);
            assert_eq!(writer.world_mut().save().unwrap().upserts, entities);
            drop(writer);
            let mut reader = App::new();
            reader
                .add_plugins(SavePlugin::sqlite(directory.path().join("save.sqlite")))
                .persist(component::<Record>("record"));
            (reader, directory)
        })
        .bench_local_refs(|(app, _)| black_box(app.world_mut().load().unwrap()));
}

// Run with `cargo bench --bench lifecycle -- --frames` for actual frame
// percentiles. Setup/full baseline writes are excluded; save frames are separate.
fn frames() {
    for entities in [1_000, 10_000] {
        let (mut app, _directory) = setup(entities);
        assert_eq!(app.world_mut().save().unwrap().upserts, entities);
        app.add_systems(Update, simulate);
        let mut idle = Vec::new();
        let mut saves = Vec::new();
        let mut generation = 1;
        for _ in 0..1_200 {
            app.world_mut()
                .resource_mut::<Time<Real>>()
                .advance_by(Duration::from_millis(20));
            let start = Instant::now();
            app.update();
            let elapsed = start.elapsed().as_nanos() as u64;
            let current = app
                .world()
                .resource::<SaveStatus>()
                .last_save()
                .unwrap()
                .as_ref()
                .unwrap()
                .generation;
            if current != generation {
                saves.push(elapsed);
            } else {
                idle.push(elapsed);
            }
            generation = current;
        }
        for (kind, mut samples) in [("idle", idle), ("save", saves)] {
            samples.sort_unstable();
            println!(
                "{}",
                serde_json::json!({"entities": entities, "frame": kind, "samples": samples.len(), "p50_ns": samples[samples.len()/2], "p95_ns": samples[(samples.len()-1)*95/100], "p99_ns": samples[(samples.len()-1)*99/100], "max_ns": samples.last().unwrap()})
            );
        }
    }
}

fn main() {
    if std::env::args().any(|arg| arg == "--frames") {
        frames();
    } else {
        divan::main();
    }
}
