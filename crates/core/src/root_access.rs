//! Trusted native participants: prepare watches before scanning and close handles for
//! the portable root release barrier. Callbacks must never execute commands on this core.
use crate::contract::{ContractError, ErrorCode};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Called before scanning, on the actor. Return registration generations to acknowledge
/// after a successful scan start/join. Must not call back into core.
pub type ScanPreparationHandler =
    dyn Fn(&[(crate::contract::RootId, String)]) -> Vec<Arc<ScanCoverage>> + Send + Sync;

/// One native registration generation. A scan ticket acknowledges its preparation;
/// closing/replacing the registration permanently invalidates this generation.
#[derive(Default)]
pub struct ScanCoverage {
    covered: AtomicBool,
    invalid: AtomicBool,
}

impl ScanCoverage {
    pub fn is_covered(&self) -> bool {
        self.covered.load(Ordering::Acquire) && !self.invalid.load(Ordering::Acquire)
    }
    pub fn invalidate(&self) {
        self.invalid.store(true, Ordering::Release);
    }
    pub(crate) fn acknowledge(&self) {
        self.covered.store(true, Ordering::Release);
    }
}

pub type RootAccessHandler = dyn Fn(&str, bool) -> Result<(), ContractError> + Send + Sync;

#[derive(Default)]
pub(crate) struct RootAccess {
    scan_handlers: Mutex<BTreeMap<u64, Arc<ScanPreparationHandler>>>,
    handlers: Mutex<BTreeMap<u64, Arc<RootAccessHandler>>>,
    next: std::sync::atomic::AtomicU64,
    operations: Mutex<BTreeMap<String, Notification>>,
}

const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type Completion =
    Result<std::collections::BTreeSet<u64>, (ContractError, std::collections::BTreeSet<u64>)>;

struct Notification {
    released: bool,
    result: Arc<Mutex<Option<Completion>>>,
}

/// Keep alive while a trusted adapter participates in root access or scan preparation.
/// Drop unregisters it.
pub struct RootAccessRegistration {
    owner: Weak<RootAccess>,
    id: u64,
    scan: bool,
}

impl RootAccess {
    pub fn register_scan(
        self: &Arc<Self>,
        handler: Arc<ScanPreparationHandler>,
    ) -> RootAccessRegistration {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.scan_handlers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, handler);
        RootAccessRegistration {
            owner: Arc::downgrade(self),
            id,
            scan: true,
        }
    }
    pub fn prepare_scan(
        &self,
        roots: &[(crate::contract::RootId, String)],
    ) -> Vec<Arc<ScanCoverage>> {
        let handlers: Vec<_> = self
            .scan_handlers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        handlers
            .into_iter()
            .flat_map(|handler| handler(roots))
            .collect()
    }
    pub fn register(self: &Arc<Self>, handler: Arc<RootAccessHandler>) -> RootAccessRegistration {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.handlers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, handler);
        RootAccessRegistration {
            owner: Arc::downgrade(self),
            id,
            scan: false,
        }
    }
    /// A timed-out callback still owns its operation. Opposite transitions cannot
    /// overtake it; completion is consumed by a retry, never by a late actor message.
    pub fn in_progress(&self, path: &str) -> bool {
        self.operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(path)
            .is_some_and(|n| n.result.lock().unwrap_or_else(|p| p.into_inner()).is_none())
    }
    fn notification(
        &self,
        path: &str,
        released: bool,
        pending: Option<&std::collections::BTreeSet<u64>>,
    ) -> Result<Completion, ContractError> {
        let result = {
            let mut operations = self.operations.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(old) = operations.get(path) {
                let done = old.result.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if done.is_none() {
                    return Err(ContractError::failed(
                        ErrorCode::JobBusy,
                        "native root notification is still pending",
                    )
                    .retryable());
                }
                if old.released == released {
                    operations.remove(path);
                    return Ok(done.unwrap());
                }
                operations.remove(path);
            }
            let handlers = self
                .handlers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter(|(id, _)| pending.is_none_or(|ids| ids.contains(id)))
                .map(|(id, h)| (*id, Arc::clone(h)))
                .collect::<Vec<_>>();
            let result = Arc::new(Mutex::new(None));
            operations.insert(
                path.into(),
                Notification {
                    released,
                    result: Arc::clone(&result),
                },
            );
            let completion = Arc::clone(&result);
            let callback_path = path.to_owned();
            if let Err(error) = std::thread::Builder::new()
                .name("root-notify".into())
                .spawn(move || {
                    let mut failed = std::collections::BTreeSet::new();
                    let mut error = None;
                    for (id, handler) in handlers {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                handler(&callback_path, released)
                            }))
                            .unwrap_or_else(|_| {
                                Err(ContractError::failed(
                                    ErrorCode::Io,
                                    "root participant panicked",
                                )
                                .retryable())
                            });
                        if let Err(e) = outcome {
                            failed.insert(id);
                            error.get_or_insert(e);
                        }
                    }
                    *completion.lock().unwrap_or_else(|p| p.into_inner()) = Some(match error {
                        Some(error) => Err((error, failed)),
                        None => Ok(failed),
                    });
                })
            {
                operations.remove(path);
                return Err(ContractError::failed(ErrorCode::Io, error.to_string()).retryable());
            }
            result
        };
        let deadline = std::time::Instant::now() + NOTIFY_TIMEOUT;
        loop {
            let done = result.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(done) = done {
                self.operations
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(path);
                return Ok(done);
            }
            if std::time::Instant::now() >= deadline {
                return Err(ContractError::failed(
                    ErrorCode::Io,
                    concat!(
                        "native root notification did not finish within 5 seconds; access remains ",
                        "suspended",
                    ),
                )
                .retryable());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    /// Retry only participants that did not acknowledge the committed removal.
    pub fn notify_recovery(
        &self,
        path: &str,
        pending: &mut Option<std::collections::BTreeSet<u64>>,
    ) -> Result<(), ContractError> {
        match self.notification(path, false, pending.as_ref())? {
            Ok(failed) => {
                *pending = Some(failed);
                Ok(())
            }
            Err((error, failed)) => {
                *pending = Some(failed);
                Err(error)
            }
        }
    }
    pub fn notify(&self, path: &str, released: bool) -> Result<(), ContractError> {
        match self.notification(path, released, None)? {
            Ok(_) => Ok(()),
            Err((error, _)) => Err(error),
        }
    }
}

impl Drop for RootAccessRegistration {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            if self.scan {
                owner
                    .scan_handlers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&self.id);
            } else {
                owner
                    .handlers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&self.id);
            }
        }
    }
}
