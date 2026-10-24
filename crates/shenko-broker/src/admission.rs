use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 参考部署的名义值，不代表当前部署已经验证四流。
pub const NOMINAL_FLOWS: u64 = 4;

pub struct AdmissionController {
    active: Arc<AtomicU64>,
    max: u64,
}

impl AdmissionController {
    pub fn new(max: u64) -> Self {
        Self {
            active: Arc::new(AtomicU64::new(0)),
            max,
        }
    }
    pub fn nominal() -> Self {
        Self::new(NOMINAL_FLOWS)
    }
    pub fn try_admit(&self) -> Option<AdmissionGuard> {
        self.active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                if cur < self.max { Some(cur + 1) } else { None }
            })
            .ok()
            .map(|_| AdmissionGuard {
                counter: Arc::clone(&self.active),
                armed: false,
            })
    }
    pub fn max_count(&self) -> u64 {
        self.max
    }
    pub fn active_count(&self) -> u64 {
        self.active.load(Ordering::SeqCst)
    }
}

pub struct AdmissionGuard {
    counter: Arc<AtomicU64>,
    armed: bool,
}
impl AdmissionGuard {
    /// 外发前切换到保守占用；之后本地 Drop 不再代表远端终止。
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }
    pub(crate) fn release_confirmed(mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
        // 消费 guard，避免 Drop 重复释放；仅 broker 的终态路径可以调用。
        self.armed = true;
    }
}
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if !self.armed {
            self.counter.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
