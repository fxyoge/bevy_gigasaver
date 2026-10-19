use crate::{
    Error, LoadOutcome, SaveStats,
    registration::{self, Registry},
    storage::{self, Storage},
};
use bevy_app::{App, AppExit, Last, PreStartup};
use bevy_ecs::{
    prelude::*,
    schedule::{ScheduleLabel, SystemSet},
};
use bevy_time::{Real, Time};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SaveSet {
    Load,
    Save,
    /// Reestablish derived gameplay invariants after resources and entities exist.
    Rebuild,
    /// Session-facing systems run after reconstruction and deferred commands.
    Ready,
}

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrepareSave;
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SaveOpened;
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SaveClosed;
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CheckReadiness;

#[derive(Resource, Default, Clone, PartialEq, Eq)]
pub struct SaveSettings {
    /// None means Never. Explicit requests and the final exit save still work.
    pub interval: Option<Duration>,
}

#[derive(Message, Clone, Debug)]
pub enum SaveRequest {
    Save,
    Load,
    Open(PathBuf),
    Close,
}

#[derive(Resource, Default)]
pub struct SaveStatus {
    path: Option<PathBuf>,
    load: Option<Result<LoadOutcome, String>>,
    last_save: Option<Result<SaveStats, String>>,
    loading: bool,
    writable: bool,
}
impl SaveStatus {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            ..Default::default()
        }
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn load(&self) -> Option<&Result<LoadOutcome, String>> {
        self.load.as_ref()
    }
    pub fn last_save(&self) -> Option<&Result<SaveStats, String>> {
        self.last_save.as_ref()
    }
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    pub fn can_save(&self) -> bool {
        self.writable
    }
}

struct Opening {
    path: PathBuf,
    storage: Storage,
}
#[derive(Resource)]
pub(crate) struct GigasaverSession {
    opening: Option<Opening>,
    requests: VecDeque<SaveRequest>,
    prepared: Option<SaveRequest>,
    exit: Option<AppExit>,
    exit_released: bool,
    deadline: Duration,
    interval: Option<Duration>,
    enabled: bool,
}

pub(crate) fn install(app: &mut App, load_on_start: bool, interval: Option<Duration>) {
    assert!(
        interval.is_none() || app.world().contains_resource::<Time<Real>>(),
        "autosave requires TimePlugin before SavePlugin"
    );
    app.init_schedule(PrepareSave)
        .init_schedule(SaveOpened)
        .init_schedule(SaveClosed)
        .init_schedule(CheckReadiness)
        .configure_sets(SaveOpened, (SaveSet::Rebuild, SaveSet::Ready).chain())
        .configure_sets(SaveClosed, (SaveSet::Rebuild, SaveSet::Ready).chain())
        .add_message::<SaveRequest>()
        .init_resource::<SaveSettings>()
        .insert_resource(GigasaverSession {
            opening: None,
            requests: VecDeque::new(),
            prepared: None,
            exit: None,
            exit_released: false,
            deadline: Duration::MAX,
            interval: None,
            enabled: true,
        })
        .add_systems(PreStartup, registration::initialize.before(SaveSet::Load))
        .add_systems(
            Last,
            (collect, process.run_if(has_work))
                .chain()
                .in_set(SaveSet::Save),
        );
    if interval.is_some() {
        app.insert_resource(SaveSettings { interval });
    }
    if load_on_start {
        app.add_systems(PreStartup, initial_load.in_set(SaveSet::Load));
    }
}

fn initial_load(world: &mut World) {
    if !world.resource::<Registry>().started
        && let Some(path) = world.resource::<SaveStatus>().path.clone()
    {
        begin_open(world, path);
        finish_open(world);
    }
}
pub(crate) fn set_enabled(In(enabled): In<bool>, mut session: ResMut<GigasaverSession>) {
    session.enabled = enabled;
}
fn ready(world: &mut World) -> bool {
    world.run_schedule(CheckReadiness);
    world.resource::<GigasaverSession>().enabled
}
fn reset_deadline(world: &mut World) {
    let now = world
        .get_resource::<Time<Real>>()
        .map_or(Duration::ZERO, |time| time.elapsed());
    let mut session = world.resource_mut::<GigasaverSession>();
    session.deadline = session
        .interval
        .map_or(Duration::MAX, |interval| now.saturating_add(interval));
}
fn collect(
    mut requests: MessageReader<SaveRequest>,
    mut session: ResMut<GigasaverSession>,
    settings: Res<SaveSettings>,
    time: Option<Res<Time<Real>>>,
    status: Res<SaveStatus>,
    mut exits: ResMut<Messages<AppExit>>,
) {
    let now = time.as_ref().map_or(Duration::ZERO, |time| time.elapsed());
    assert!(
        settings.interval.is_none() || time.is_some(),
        "autosave requires TimePlugin"
    );
    if session.interval != settings.interval {
        assert!(
            !settings.interval.is_some_and(|interval| interval.is_zero()),
            "autosave interval must be positive"
        );
        session.interval = settings.interval;
        session.deadline = settings
            .interval
            .map_or(Duration::MAX, |interval| now.saturating_add(interval));
    }
    session.requests.extend(requests.read().cloned());
    if status.writable && session.exit.is_none() && !session.exit_released && !exits.is_empty() {
        session.exit = exits.drain().max_by_key(|exit| u8::from(exit.is_error()));
        if !session
            .requests
            .iter()
            .any(|request| matches!(request, SaveRequest::Save))
        {
            session.requests.push_back(SaveRequest::Save);
        }
    }
}
fn has_work(
    session: Res<GigasaverSession>,
    status: Res<SaveStatus>,
    time: Option<Res<Time<Real>>>,
) -> bool {
    let now = time.as_ref().map_or(Duration::ZERO, |time| time.elapsed());
    session.opening.is_some()
        || session.prepared.is_some()
        || !session.requests.is_empty()
        || (status.writable && now >= session.deadline)
}
fn save(world: &mut World) -> Result<(), Error> {
    let result = if ready(world) {
        storage::save(world)
    } else {
        Err(Error::NotReady)
    };
    record_save(world, &result);
    if result.is_ok()
        && let Some(exit) = world.resource_mut::<GigasaverSession>().exit.take()
    {
        world.resource_mut::<GigasaverSession>().exit_released = true;
        world.write_message(exit);
    }
    result.map(|_| ())
}
fn begin_open(world: &mut World, path: PathBuf) {
    world.resource_mut::<GigasaverSession>().opening = Some(Opening {
        storage: Storage::new(path.clone()),
        path,
    });
    let mut status = world.resource_mut::<SaveStatus>();
    status.loading = true;
    status.load = None;
}
fn finish_open(world: &mut World) {
    if !ready(world) {
        return;
    }
    let Some(opening) = world.resource_mut::<GigasaverSession>().opening.take() else {
        return;
    };
    let outgoing = world
        .remove_resource::<Storage>()
        .expect("installed save storage");
    world.insert_resource(opening.storage);
    let result = storage::load(world);
    match result {
        Ok(outcome) => {
            if outcome == LoadOutcome::Missing {
                storage::clear_scope(world);
            }
            *world.resource_mut::<SaveStatus>() = SaveStatus {
                path: Some(opening.path),
                load: Some(Ok(outcome)),
                writable: true,
                ..Default::default()
            };
            world.run_schedule(SaveOpened);
            reset_deadline(world);
        }
        Err(Error::AssetNotReady(_)) => {
            let storage = world.remove_resource::<Storage>().unwrap();
            world.insert_resource(outgoing);
            world.resource_mut::<GigasaverSession>().opening = Some(Opening {
                path: opening.path,
                storage,
            });
        }
        Err(error) => {
            world.insert_resource(outgoing);
            let writable = storage::can_save(world);
            let mut status = world.resource_mut::<SaveStatus>();
            status.loading = false;
            status.load = Some(Err(error.to_string()));
            status.writable = writable;
        }
    }
}
fn process(world: &mut World) {
    if world.resource::<GigasaverSession>().opening.is_some() {
        finish_open(world);
        if world.resource::<GigasaverSession>().opening.is_some() {
            return;
        }
    }
    let mut prepared = world.resource_mut::<GigasaverSession>().prepared.take();
    let requested = prepared.is_some() || !world.resource::<GigasaverSession>().requests.is_empty();
    loop {
        let already_prepared = prepared.is_some();
        let Some(request) = prepared.take().or_else(|| {
            world
                .resource_mut::<GigasaverSession>()
                .requests
                .pop_front()
        }) else {
            break;
        };
        // A preparation schedule is optional. Ordinary games save immediately;
        // gameplay settlement commands get a normal cleanup frame when installed.
        let has_preparation = world
            .resource::<Schedules>()
            .get(PrepareSave)
            .is_some_and(|schedule| schedule.systems_len() > 0);
        if !already_prepared
            && !matches!(request, SaveRequest::Load)
            && world.resource::<SaveStatus>().writable
            && has_preparation
        {
            world.run_schedule(PrepareSave);
            world.flush();
            world.resource_mut::<GigasaverSession>().prepared = Some(request);
            return;
        }
        match request {
            SaveRequest::Save => {
                let _ = save(world);
            }
            SaveRequest::Load => {
                if let Some(path) = world.resource::<SaveStatus>().path.clone() {
                    begin_open(world, path);
                }
            }
            SaveRequest::Open(path) => {
                if outgoing(world) && save(world).is_err() {
                    continue;
                }
                begin_open(world, path);
            }
            SaveRequest::Close => {
                if outgoing(world) {
                    if save(world).is_err() {
                        continue;
                    }
                    storage::clear_scope(world);
                    world.resource_mut::<Storage>().close();
                    *world.resource_mut::<SaveStatus>() = SaveStatus::default();
                    world.run_schedule(SaveClosed);
                }
            }
        }
        if world.resource::<GigasaverSession>().opening.is_some() {
            finish_open(world);
            if world.resource::<GigasaverSession>().opening.is_some() {
                return;
            }
        }
    }
    let now = world
        .get_resource::<Time<Real>>()
        .map_or(Duration::ZERO, |time| time.elapsed());
    if !requested
        && world.resource::<SaveStatus>().writable
        && now >= world.resource::<GigasaverSession>().deadline
    {
        if ready(world) {
            let result = storage::save(world);
            record_save(world, &result);
        } else {
            record_save(world, &Err(Error::NotReady));
        }
        reset_deadline(world);
    }
}
fn outgoing(world: &World) -> bool {
    !world.resource::<Storage>().closed
        && (world.resource::<SaveStatus>().writable
            || (world.resource::<Registry>().started
                && world.resource::<SaveStatus>().load.is_none()))
}
pub(crate) fn record_save(world: &mut World, result: &Result<SaveStats, Error>) {
    let mut status = world.resource_mut::<SaveStatus>();
    status.last_save = Some(result.as_ref().copied().map_err(ToString::to_string));
    if result.is_ok() {
        status.writable = true;
        reset_deadline(world);
    }
}
pub(crate) fn record_load(world: &mut World, result: &Result<LoadOutcome, Error>) {
    let writable = storage::can_save(world);
    let mut status = world.resource_mut::<SaveStatus>();
    status.load = Some(result.as_ref().copied().map_err(ToString::to_string));
    status.writable = writable;
    if result.is_ok() {
        world.run_schedule(SaveOpened);
        reset_deadline(world);
    }
}
