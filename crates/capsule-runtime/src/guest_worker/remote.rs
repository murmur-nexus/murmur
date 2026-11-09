//! Supervisor-side handles on session-long instances a worker holds.

use std::sync::Arc;

use wasmtime::component::Component;

use super::GuestWorkers;
use crate::errors::RuntimeError;
use crate::limits::CallDeadlines;
use crate::process_driver::{DriverAnswer, DriverCall};

/// A process driver instance the driver worker holds. Dropping it drops the instance there.
pub(crate) struct RemoteProcessDriver {
    workers: Arc<GuestWorkers>,
    instance: u64,
    name: String,
    version: String,
}

impl RemoteProcessDriver {
    /// Instantiates `component` in the driver worker, as `ProcessDriver::instantiate` does in
    /// process, with the same errors.
    pub(crate) async fn open(
        workers: &Arc<GuestWorkers>,
        component: &Component,
        name: &str,
        version: &str,
        call_deadlines: &CallDeadlines,
    ) -> Result<Self, RuntimeError> {
        let (instance, opened) = workers
            .open_driver(name, version, component, call_deadlines)
            .await
            .map_err(|error| workers.runtime_error(error))?;
        let driver = Self {
            workers: Arc::clone(workers),
            instance,
            name: name.to_string(),
            version: version.to_string(),
        };
        opened.map_err(|error| error.into_runtime(name, version))?;
        Ok(driver)
    }

    /// Runs `call` on the instance; its error is the one the same call raises in process.
    pub(crate) async fn call(
        &self,
        call: DriverCall,
        call_deadlines: &CallDeadlines,
    ) -> Result<DriverAnswer, RuntimeError> {
        self.workers
            .call_driver(self.instance, &self.name, call, call_deadlines)
            .await
            .map_err(|error| self.workers.runtime_error(error))?
            .map_err(|error| error.into_runtime(&self.name, &self.version))
    }
}

impl Drop for RemoteProcessDriver {
    fn drop(&mut self) {
        self.workers.close_driver(self.instance);
    }
}
