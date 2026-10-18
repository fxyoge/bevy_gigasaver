use crate::{Error, LoadOutcome, SaveStats, WorldSaveExt, registration::Registry, storage};
use bevy_app::{App, AppExit, Last, PreStartup};
use bevy_ecs::{
    prelude::*,
    schedule::{ScheduleLabel, SystemSet},
};
use bevy_time::{Real, Time};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Ordering boundaries: initial load in `PreStartup`, requests/autosaves in `Last`.
/// Put teardown after `SaveSet::Save` so final saves precede entity removal.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SaveSet {
    Load,
    Save,
}

/// Runs after every successful load, including a missing destination on open.
/// Configure existing fresh-session initialization here, conditioned on
/// `SaveStatus::load() == Some(&Ok(LoadOutcome::Missing))`.
/// This also runs for direct `World::load()` calls; configure before loading.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SaveLoaded;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CheckReadiness;

/// Requests are processed in order in `Last`, at `SaveSet::Save`.
#[derive(Message, Clone, Debug)]
pub enum SaveRequest {
    Save,
    Load,
    /// Saves the outgoing session before opening another path. Failure to save
    /// retains the outgoing session. Destination decode failures preserve live
    /// state but block writes to the destination until a successful load.
    Open(PathBuf),
    /// Saves, then removes selected entities/resources and disables persistence.
    Close,
}

/// Observational results. Error strings retain the operation's diagnostic;
/// direct World methods also return the structured `Error`.
#[derive(Resource)]
pub struct SaveStatus {
    path: PathBuf,
    load: Option<Result<LoadOutcome, String>>,
    last_save: Option<Result<SaveStats, String>>,
    writable: bool,
}

impl SaveStatus {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            load: None,
            last_save: None,
            writable: false,
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn load(&self) -> Option<&Result<LoadOutcome, String>> {
        self.load.as_ref()
    }
    pub fn last_save(&self) -> Option<&Result<SaveStats, String>> {
        self.last_save.as_ref()
    }
    /// Safety authorization, independent of the configured application readiness condition.
    pub fn can_save(&self) -> bool {
        self.writable
    }
}

#[derive(Resource)]
pub(crate) struct Automation {
    pub enabled: bool,
    interval: Option<Duration>,
    deadline: Duration,
    pending: Vec<SaveRequest>,
    exiting: bool,
    due: bool,
}

pub(crate) fn install(app: &mut App, load_on_start: bool, interval: Option<Duration>) {
    if interval.is_some() {
        assert!(
            app.world().contains_resource::<Time<Real>>(),
            "autosave requires TimePlugin before SavePlugin"
        );
    }
    app.init_schedule(SaveLoaded)
        .init_schedule(CheckReadiness)
        .add_message::<SaveRequest>()
        .insert_resource(Automation {
            enabled: true,
            interval,
            deadline: interval.unwrap_or(Duration::MAX),
            pending: Vec::new(),
            exiting: false,
            due: false,
        })
        .add_systems(
            Last,
            (prepare, process.run_if(has_work))
                .chain()
                .in_set(SaveSet::Save),
        );
    if load_on_start {
        app.add_systems(PreStartup, initial_load.in_set(SaveSet::Load));
    }
}

fn initial_load(world: &mut World) {
    // Explicit World calls before the first update already established the session.
    if !world.resource::<Registry>().started {
        let _ = world.load();
    }
}

pub(crate) fn set_enabled(In(enabled): In<bool>, mut automation: ResMut<Automation>) {
    if automation.enabled != enabled {
        automation.enabled = enabled;
    }
}

fn prepare(
    mut requests: MessageReader<SaveRequest>,
    mut exits: MessageReader<AppExit>,
    time: Option<Res<Time<Real>>>,
    mut automation: ResMut<Automation>,
) {
    let automation = &mut *automation;
    automation.pending.extend(requests.read().cloned());
    automation.exiting = exits.read().count() > 0;
    let now = time.as_ref().map_or(Duration::ZERO, |time| time.elapsed());
    automation.due = automation.interval.is_some() && now >= automation.deadline;
    if automation.due {
        automation.deadline = now.saturating_add(automation.interval.unwrap());
    }
}

fn has_work(automation: Res<Automation>) -> bool {
    !automation.pending.is_empty() || automation.exiting || automation.due
}

fn process(world: &mut World) {
    let (pending, exiting, due) = {
        let mut automation = world.resource_mut::<Automation>();
        (
            std::mem::take(&mut automation.pending),
            automation.exiting,
            automation.due,
        )
    };
    let mut saved_current = None;
    let suppress_autosave = !pending.is_empty();
    for request in pending {
        match request {
            SaveRequest::Save => {
                saved_current = Some(ready_save(world).is_ok());
            }
            SaveRequest::Load => {
                let _ = world.load();
                saved_current = None;
            }
            SaveRequest::Open(path) => {
                if outgoing_save(world).is_err() {
                    saved_current = Some(false);
                    continue;
                }
                world.insert_resource(storage::Storage::new(path.clone()));
                world.insert_resource(SaveStatus::new(path));
                world.resource_mut::<Registry>().generation = None;
                let result = storage::load(world);
                if matches!(result, Ok(LoadOutcome::Missing)) {
                    storage::clear_scope(world);
                }
                record_load(world, &result);
                saved_current = None; // outgoing commit belongs to the previous file
            }
            SaveRequest::Close => {
                if outgoing_save(world).is_err() {
                    saved_current = Some(false);
                    continue;
                }
                storage::clear_scope(world);
                world.resource_mut::<storage::Storage>().close();
                world.resource_mut::<SaveStatus>().writable = false;
            }
        }
    }
    if exiting {
        if !world.resource::<storage::Storage>().closed
            && world.resource::<Registry>().started
            && !saved_current.unwrap_or_else(|| ready_save(world).is_ok())
        {
            world.write_message(AppExit::error());
        }
    } else if due && !suppress_autosave && world.resource::<SaveStatus>().can_save() {
        let _ = ready_save(world);
    }
}

fn outgoing_save(world: &mut World) -> Result<(), Error> {
    // A never-opened or failed-to-load destination owns no writable session.
    // A failed *first save* still owns its unsaved live state and must be retried.
    if world.resource::<storage::Storage>().closed
        || !world.resource::<Registry>().started
        || world
            .resource::<SaveStatus>()
            .load()
            .is_some_and(Result::is_err)
    {
        return Ok(());
    }
    ready_save(world).map(|_| ())
}

fn ready_save(world: &mut World) -> Result<SaveStats, Error> {
    world.run_schedule(CheckReadiness);
    if !world.resource::<Automation>().enabled {
        let result = Err(Error::NotReady);
        record_save(world, &result);
        return result;
    }
    world.save()
}

pub(crate) fn record_save(world: &mut World, result: &Result<SaveStats, Error>) {
    let writable = storage::can_save(world);
    let mut status = world.resource_mut::<SaveStatus>();
    status.last_save = Some(result.as_ref().copied().map_err(ToString::to_string));
    status.writable = writable;
    if result.is_ok() {
        reset_deadline(world);
    }
}

pub(crate) fn record_load(world: &mut World, result: &Result<LoadOutcome, Error>) {
    let writable = storage::can_save(world);
    {
        let mut status = world.resource_mut::<SaveStatus>();
        status.load = Some(result.as_ref().copied().map_err(ToString::to_string));
        status.writable = writable;
    }
    if result.is_ok() {
        world.run_schedule(SaveLoaded);
        reset_deadline(world);
    }
}

fn reset_deadline(world: &mut World) {
    if let Some(time) = world.get_resource::<Time<Real>>() {
        let now = time.elapsed();
        let mut automation = world.resource_mut::<Automation>();
        automation.deadline = now.saturating_add(automation.interval.unwrap_or(Duration::MAX));
    }
}
