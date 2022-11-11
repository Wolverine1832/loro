use std::sync::{Arc, Mutex, RwLock};

use crate::{
    container::{registry::ContainerInstance, ContainerID},
    LogStore, LoroCore,
};

pub trait Context {
    fn log_store(&self) -> Arc<RwLock<LogStore>>;
    fn get_container(&self, id: ContainerID) -> Option<Arc<Mutex<ContainerInstance>>>;
}

impl Context for LoroCore {
    fn log_store(&self) -> Arc<RwLock<LogStore>> {
        self.log_store.clone()
    }

    fn get_container(&self, id: ContainerID) -> Option<Arc<Mutex<ContainerInstance>>> {
        self.log_store
            .write()
            .unwrap()
            .get_container(&id)
            .map(|x| x.clone())
    }
}
