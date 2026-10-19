//! Annotation-only persistence for Bevy. SavePlugin discovers declarations and
//! owns resource initialization, session replacement, reset and storage.
//!
//! ```no_run
//! use bevy_app::App;
//! use bevy_ecs::prelude::*;
//! use bevy_gigasaver::{save, SavePlugin};
//! use serde::{Serialize, Deserialize};
//!
//! #[save(resource, key = "game.score")]
//! #[derive(Resource, Default, Serialize, Deserialize)]
//! struct Score(u64);
//!
//! let mut app = App::new();
//! app.add_plugins(SavePlugin::sqlite("save.sqlite"));
//! ```
//!
//! Declarations are installed into each app that adds SavePlugin. Constructors
//! use ordinary Bevy FromWorld (including Default), run after plugin setup, and
//! are never called while validating a restore. Fresh open and close remove all
//! outgoing saved resources before initializing fresh values. Existing saves
//! restore recorded presence/absence exactly. Unregistered platform state stays
//! outside the session. Gameplay-specific reconstruction can use SaveOpened's
//! fixed SaveSet::Rebuild and SaveSet::Ready boundaries.
pub mod adapter;
mod checkpoint;
mod identity;
mod lifecycle;
mod migration;
mod registration;
mod storage;
pub use bevy_gigasaver_macros::save;
pub use checkpoint::Checkpoint;
extern crate self as bevy_gigasaver;

pub mod asset;
pub mod entity;

pub use lifecycle::{
    PrepareSave, SaveClosed, SaveOpened, SaveRequest, SaveSet, SaveSettings, SaveStatus,
};
pub use migration::{MigrationData, SavedType};
pub use registration::{AppSaveExt, Capture, ComponentSave, Dirty, Registration, component};
pub use storage::{Error, SaveStats};

#[doc(hidden)]
pub mod __private {
    pub use crate::registration::{Declaration, declare_component, declare_resource};
    pub use inventory;
}

pub mod prelude {
    pub use crate::{SavePlugin, SaveRequest, SaveSettings, SaveStatus, save};
}

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::SystemCondition;
use std::path::{Path, PathBuf};
use std::time::Duration;

type ConfigureReadiness = Box<dyn Fn(&mut App) + Send + Sync>;

#[derive(Default)]
pub struct SavePlugin {
    path: Option<PathBuf>,
    schema: migration::Schema,
    load_on_start: bool,
    interval: Option<Duration>,
    readiness: Option<ConfigureReadiness>,
}

impl SavePlugin {
    pub fn sqlite(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_owned()),
            schema: migration::Schema::default(),
            load_on_start: true,
            interval: None,
            readiness: None,
        }
    }

    /// Defaults to loading in `PreStartup`. Disable for sessions opened later by requests.
    pub fn load_on_start(mut self, enabled: bool) -> Self {
        self.load_on_start = enabled;
        self
    }

    /// Uses real elapsed time, unaffected by simulation pause. Install TimePlugin first.
    pub fn autosave(mut self, interval: Duration) -> Self {
        assert!(!interval.is_zero(), "autosave interval must be positive");
        self.interval = Some(interval);
        self
    }

    /// Application readiness, evaluated at automatic/requested/final save attempts.
    /// This condition cannot authorize saving after a failed load. Direct
    /// World::save calls are explicit and only enforce safety authorization.
    pub fn enabled_when<M>(
        mut self,
        condition: impl SystemCondition<M> + Clone + Send + Sync + 'static,
    ) -> Self {
        self.readiness = Some(Box::new(move |app| {
            app.add_systems(
                lifecycle::CheckReadiness,
                condition.clone().pipe(lifecycle::set_enabled),
            );
        }));
        self
    }

    /// Application schema version, independent of Gigasaver's storage format.
    /// Existing pre-versioning saves are application version 1.
    pub fn version(mut self, version: u32) -> Self {
        assert!(version > 0, "schema version must be positive");
        self.schema.version = version;
        self
    }

    /// Registers a `from` -> `from + 1` transformation. Runs on staged data;
    /// errors block saving and leave the committed save untouched.
    pub fn migrate(
        mut self,
        from: u32,
        step: impl Fn(&mut MigrationData) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        assert!(from > 0 && from < u32::MAX, "invalid migration version");
        assert!(
            self.schema
                .steps
                .insert(from, std::sync::Arc::new(step))
                .is_none(),
            "duplicate migration step"
        );
        self
    }
}

impl Plugin for SavePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<identity::Identities>()
            .init_resource::<registration::Registry>()
            .insert_resource(self.schema.clone())
            .insert_resource(SaveStatus::new(self.path.clone()))
            .insert_resource(storage::Storage::new(self.path.clone().unwrap_or_default()));
        if self.path.is_none() {
            app.world_mut().resource_mut::<storage::Storage>().close();
        }
        lifecycle::install(app, self.load_on_start, self.interval);
        app.persist(component::<bevy_ecs::name::Name>("bevy.name").attached())
            .persist(
                component::<bevy_transform::components::Transform>("bevy.transform").attached(),
            );
        let mut declarations: Vec<_> = inventory::iter::<registration::Declaration>
            .into_iter()
            .collect();
        declarations.sort_by_key(|declaration| declaration.key);
        for declaration in declarations {
            (declaration.install)(app);
        }
        if let Some(readiness) = &self.readiness {
            readiness(app);
        }
    }
}

pub trait WorldSaveExt {
    /// Apply schedule-owned deferred buffers first. Existing saves must be loaded
    /// successfully before writing. Failed commits retain dirty state.
    fn save(&mut self) -> Result<SaveStats, Error>;

    /// Replaces selected entities and registered resources. Decode errors leave
    /// live state untouched; component-hook side effects cannot be rolled back.
    fn load(&mut self) -> Result<LoadOutcome, Error>;
}

/// A missing save is a fresh session, not a load failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    Missing,
    Loaded,
}

impl WorldSaveExt for World {
    fn save(&mut self) -> Result<SaveStats, Error> {
        let result = storage::save(self);
        if self.contains_resource::<SaveStatus>() {
            lifecycle::record_save(self, &result);
        }
        result
    }

    fn load(&mut self) -> Result<LoadOutcome, Error> {
        let result = storage::load(self);
        if matches!(result, Ok(LoadOutcome::Missing)) {
            storage::clear_scope(self);
        }
        if self.contains_resource::<SaveStatus>() {
            lifecycle::record_load(self, &result);
        }
        result
    }
}
