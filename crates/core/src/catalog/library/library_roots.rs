use super::*;
use crate::catalog::CatalogCloseError;

impl LibraryRoots for Library {
    fn close(&self) -> Result<(), CatalogCloseError> {
        Library::close(self).map_err(|error| CatalogCloseError {
            message: error.to_string(),
            progress: Some(Arc::new(self.close_progress())),
        })
    }

    fn file_lease(&self, track: TrackId) -> Result<Box<dyn Send>, ContractError> {
        Ok(Box::new(
            Library::file_lease(self, mpmp_library::TrackId(track.0)).map_err(library_error)?,
        ))
    }

    fn check_root_access(&self, root: RootId) -> Result<(), ContractError> {
        if self
            .root_removed(mpmp_library::RootId(root.0))
            .map_err(library_error)?
            == Some(false)
        {
            Ok(())
        } else {
            Err(root_not_found(root))
        }
    }

    fn suspend_root(&self, root: RootId) -> Result<bool, ContractError> {
        Library::suspend_root(self, mpmp_library::RootId(root.0)).map_err(library_error)
    }

    fn audit_root_access(
        &self,
        root: RootId,
        released: bool,
        origin: &Origin,
        grant: Option<GrantId>,
    ) -> Result<ActionId, ContractError> {
        let label = origin.label();
        Library::audit_root_access(
            self,
            mpmp_library::RootId(root.0),
            released,
            action_meta(&label, grant, origin),
        )
        .map(action_id)
        .map_err(library_error)
    }

    fn wait_root_idle(
        &self,
        root: RootId,
        timeout: std::time::Duration,
    ) -> Result<bool, ContractError> {
        Library::wait_root_idle(self, mpmp_library::RootId(root.0), timeout).map_err(library_error)
    }

    fn playback_lease(&self, uri: &MediaUri) -> Result<Box<dyn Send>, ContractError> {
        Ok(Box::new(
            Library::playback_lease(self, uri).map_err(library_error)?,
        ))
    }

    fn resume_root(&self, root: RootId) -> Result<bool, ContractError> {
        Library::resume_root(self, mpmp_library::RootId(root.0)).map_err(library_error)
    }

    fn rescan_recommended(&self) -> Result<bool, ContractError> {
        Library::rescan_recommended(self).map_err(library_error)
    }

    fn artist_backfill_pending(&self) -> Result<bool, ContractError> {
        self.artists_indexing().map_err(library_error)
    }

    fn add_root(
        &self,
        path: &str,
        origin: &Origin,
        grant: Option<GrantId>,
    ) -> Result<Recorded<RootId>, ContractError> {
        let label = origin.label();
        let done = Library::add_root(self, Path::new(path), action_meta(&label, grant, origin))
            .map_err(|e| library_error(e).with_field("command.path"))?;
        Ok(recorded(done).map(|id| RootId(id.0)))
    }

    fn remove_root(
        &self,
        root: RootId,
        origin: &Origin,
        grant: Option<GrantId>,
    ) -> Result<Recorded<()>, ContractError> {
        let label = origin.label();
        let done = Library::remove_root(
            self,
            mpmp_library::RootId(root.0),
            action_meta(&label, grant, origin),
        )
        .map_err(|e| {
            let error = library_error(e);
            if error.code == ErrorCode::RootNotFound {
                error
                    .with_missing(vec![ObjectRef::Root { id: root }])
                    .with_field("command.root")
            } else {
                error
            }
        })?;
        Ok(recorded(done))
    }

    fn root_location(&self, root: RootId) -> Result<Option<String>, ContractError> {
        Library::root_path(self, mpmp_library::RootId(root.0)).map_err(library_error)
    }

    fn rescan(
        &self,
        events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        self.rescan_roots(None, events)
    }

    fn rescan_roots(
        &self,
        roots: Option<Vec<RootId>>,
        events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        scan_scoped(self, roots, None, events)
    }

    fn rescan_directories(
        &self,
        directories: Vec<PathBuf>,
        events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        scan_scoped(self, None, Some(directories), events)
    }
}
