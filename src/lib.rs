mod identity;
mod registration;
mod storage;

pub mod entity;

pub use registration::{
    AppSaveExt, Capture, ComponentSave, Dirty, Registration, ResourceSave, component, resource,
};
pub use storage::{Error, SaveStats};

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use std::path::{Path, PathBuf};

pub struct SavePlugin {
    path: PathBuf,
}

impl SavePlugin {
    pub fn sqlite(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
        }
    }
}

impl Plugin for SavePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<identity::Identities>()
            .init_resource::<registration::Registry>()
            .insert_resource(storage::Storage::new(self.path.clone()));
    }
}

pub trait WorldSaveExt {
    /// Apply schedule-owned deferred buffers first. A new app's first save replaces
    /// the existing save; load first to resume. Failed commits retain dirty state.
    fn save(&mut self) -> Result<SaveStats, Error>;

    /// Replaces selected entities and registered resources. Decode errors leave
    /// live state untouched; component-hook side effects cannot be rolled back.
    fn load(&mut self) -> Result<(), Error>;
}

impl WorldSaveExt for World {
    fn save(&mut self) -> Result<SaveStats, Error> {
        storage::save(self)
    }

    fn load(&mut self) -> Result<(), Error> {
        storage::load(self)
    }
}
