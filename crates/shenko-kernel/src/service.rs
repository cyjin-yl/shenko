//! Service bindings — typed service locator.
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

pub struct ServiceRegistry {
    services: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self { services: RwLock::new(HashMap::new()) }
    }

    pub fn register<T: Any + Send + Sync>(&self, service: Arc<T>) {
        self.services.write().insert(TypeId::of::<T>(), service);
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.services.read()
            .get(&TypeId::of::<T>())
            .and_then(|s| Arc::clone(s).downcast::<T>().ok())
    }

    pub fn require<T: Any + Send + Sync>(&self) -> Arc<T> {
        self.get::<T>().expect("required service not registered")
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self { Self::new() }
}
