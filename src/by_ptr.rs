use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

#[derive(Debug)]
pub struct ByPtr<T>(Arc<T>);

impl<T> From<T> for ByPtr<T> {
    fn from(value: T) -> Self {
        ByPtr(Arc::new(value))
    }
}

impl<T> From<Arc<T>> for ByPtr<T> {
    fn from(arc: Arc<T>) -> Self {
        ByPtr(arc)
    }
}

impl<T> Clone for ByPtr<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Deref for ByPtr<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Hash for ByPtr<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl<T> PartialEq for ByPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::as_ptr(&self.0) == Arc::as_ptr(&other.0)
    }
}

impl<T> Eq for ByPtr<T> {}

impl<T> PartialOrd for ByPtr<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for ByPtr<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        Arc::as_ptr(&self.0).cmp(&Arc::as_ptr(&other.0))
    }
}
