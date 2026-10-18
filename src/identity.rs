use bevy_ecs::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy)]
pub(crate) struct Identity {
    pub id: i64,
    pub memberships: usize,
}

#[derive(Resource)]
pub(crate) struct Identities {
    pub entries: Arc<HashMap<Entity, Identity>>,
    pub next: i64,
    pub count: usize,
    pub added: HashSet<Entity>,
    pub removed: HashSet<Entity>,
    pub suspended: bool,
}

impl Default for Identities {
    fn default() -> Self {
        Self {
            entries: Arc::default(),
            next: 1,
            count: 0,
            added: HashSet::new(),
            removed: HashSet::new(),
            suspended: false,
        }
    }
}

impl Identities {
    pub fn enter(&mut self, entity: Entity) {
        let entries = Arc::make_mut(&mut self.entries);
        let entry = entries.entry(entity).or_insert_with(|| {
            let id = self.next;
            self.next = self
                .next
                .checked_add(1)
                .expect("persistent entity IDs exhausted");
            Identity { id, memberships: 0 }
        });
        if entry.memberships == 0 {
            self.count += 1;
            self.added.insert(entity);
            self.removed.remove(&entity);
        }
        entry.memberships += 1;
    }

    pub fn leave(&mut self, entity: Entity) {
        let entry = Arc::make_mut(&mut self.entries)
            .get_mut(&entity)
            .expect("tracked identity");
        entry.memberships -= 1;
        if entry.memberships == 0 {
            self.count -= 1;
            self.removed.insert(entity);
            self.added.remove(&entity);
        }
    }

    pub fn acknowledge(&mut self) {
        let entries = Arc::make_mut(&mut self.entries);
        for entity in self.removed.drain() {
            entries.remove(&entity);
        }
        self.added.clear();
    }
}
