//! No-default startup and capability tests; no desktop test dependency.
use super::*;
use crate::contract::*;
use crate::{Doubt, Locations, PlayRecord, StoredPermissions};
use mpmp_engine::output::OutputBackend;
use mpmp_engine::{
    NullOutput,
    output::{SampleType, StreamFormat},
};
use std::sync::atomic::{AtomicUsize, Ordering};

use mpmp_test_support::local_storage as local;
#[derive(Default)]
struct CatalogProbe {
    calls: AtomicUsize,
    caps: Mutex<Option<mpmp_storage::WriteCapabilities>>,
}

impl crate::catalog::Browse for CatalogProbe {
    fn tracks(&self, _: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }

    fn uri_of(&self, _: TrackId) -> Result<Option<mpmp_media_io::MediaUri>, ContractError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(vec![])
    }

    fn query(&self, _: &Query) -> QueryResult {
        QueryResult::Unknown
    }
}

impl crate::catalog::Playlists for CatalogProbe {}

impl crate::catalog::LibraryRoots for CatalogProbe {}

impl crate::catalog::Actions for CatalogProbe {}

impl crate::catalog::CatalogChecks for CatalogProbe {
    fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        self.caps
            .lock()
            .unwrap()
            .unwrap_or(mpmp_storage::WriteCapabilities {
                durable_replace: false,
                legacy_tag_write: false,
            })
    }
}

impl crate::catalog::AgentPermissions for CatalogProbe {
    fn load_permissions(&self) -> Result<StoredPermissions, ContractError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(StoredPermissions::default())
    }
}

impl crate::catalog::MediaResources for CatalogProbe {}

impl crate::catalog::Loudness for CatalogProbe {}

impl crate::catalog::TagsAndCovers for CatalogProbe {}

impl crate::catalog::PlaybackHistory for CatalogProbe {
    fn record_play(&self, _: PlayRecord) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct InjectedLocations(AtomicUsize);

impl Locations for InjectedLocations {
    fn normalize(&self, location: &str) -> Option<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        (location == "test:music").then(|| location.into())
    }
    fn contains(&self, scope: &str, location: &str, _: Doubt) -> bool {
        scope == location
    }
    fn child(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn ignores_case(&self, _: &str) -> Option<bool> {
        Some(false)
    }
    fn display(&self, location: &str) -> String {
        location.into()
    }
    fn well_formed(&self, location: &str) -> bool {
        location == "test:music"
    }
    fn nearest(&self, location: &str) -> Option<String> {
        self.normalize(location)
    }
}

fn output() -> Box<dyn OutputBackend> {
    Box::new(NullOutput::new(StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        sample_type: SampleType::F32,
    }))
}

fn config() -> CoreConfig {
    CoreConfig {
        locations: Some(Arc::new(InjectedLocations::default())),
        automatic_loudness_scan: Some(false),
        startup_scan_by_host: true,
        ..Default::default()
    }
}

fn start(catalog: Arc<dyn Catalog>, config: CoreConfig) -> CoreHandle {
    CoreHandle::start(
        output(),
        Arc::new(mpmp_media_io::FileResolver),
        catalog,
        config,
    )
    .unwrap()
}

#[cfg(not(feature = "desktop-fs"))]
#[test]
fn missing_locations_fails_before_workers_start() {
    struct UntouchedOutput;
    impl OutputBackend for UntouchedOutput {
        fn watch_output(
            &mut self,
            _: mpmp_engine::output::OutputEventSink,
        ) -> Result<(), mpmp_engine::output::OutputError> {
            panic!("engine started before validating Locations")
        }
        fn devices(
            &self,
        ) -> Result<Vec<mpmp_engine::output::DeviceInfo>, mpmp_engine::output::OutputError>
        {
            panic!("output accessed")
        }
        fn capabilities(
            &self,
            _: Option<&mpmp_engine::output::DeviceId>,
        ) -> Result<mpmp_engine::output::OutputCaps, mpmp_engine::output::OutputError> {
            panic!("output accessed")
        }
        fn open(
            &mut self,
            _: mpmp_engine::output::StreamRequest,
            _: Box<dyn mpmp_engine::output::Renderer>,
        ) -> Result<Box<dyn mpmp_engine::output::OutputStream>, mpmp_engine::output::OutputError>
        {
            panic!("output accessed")
        }
    }
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let state_file = base.join("absent/state.json");
    let catalog = Arc::new(CatalogProbe::default());
    let result = CoreHandle::start(
        Box::new(UntouchedOutput),
        Arc::new(mpmp_media_io::FileResolver),
        catalog.clone(),
        CoreConfig {
            state_file: Some(state_file.clone()),
            ..Default::default()
        },
    );
    let error = result.err().expect("missing Locations must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("Locations"));
    assert_eq!(catalog.calls.load(Ordering::SeqCst), 0);
    assert!(!state_file.parent().unwrap().exists());
}

#[test]
fn injected_locations_override_desktop_default() {
    let locations = Arc::new(InjectedLocations::default());
    let core = start(
        Arc::new(CatalogProbe::default()),
        CoreConfig {
            locations: Some(locations.clone()),
            ..config()
        },
    );
    let response = core.execute(
        Request::new(
            1,
            Command::GrantPermission {
                agent: "test:agent".into(),
                session: None,
                operation: Operation::Class {
                    class: OperationClass::LocalPath,
                },
                scope: Scope::Location {
                    location: "test:music".into(),
                },
                until: Expiry::Session,
                effect: Effect::Allow,
            },
        ),
        Origin::Ui,
    );
    assert!(
        matches!(response.result, CommandResult::Ok { .. }),
        "{response:?}"
    );
    assert!(locations.0.load(Ordering::SeqCst) > 0);
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
}

fn assert_features(snapshot: &Snapshot, cover: bool, tags: bool) {
    assert_eq!(snapshot.features.iter().any(|f| f == "cover_edit"), cover);
    assert_eq!(snapshot.features.iter().any(|f| f == "edit_tags"), tags);
    assert!(snapshot.features.iter().any(|f| f == "permissions"));
}

#[test]
fn snapshot_filters_cover_edit_and_edit_tags_by_catalog() {
    let catalog = Arc::new(CatalogProbe::default());
    let core = start(catalog.clone(), config());
    assert_features(&core.inner.published.load().snapshot, false, false);
    for (cover, tags) in [(true, true), (true, false), (false, true), (false, false)] {
        *catalog.caps.lock().unwrap() = Some(mpmp_storage::WriteCapabilities {
            durable_replace: cover,
            legacy_tag_write: tags,
        });
        core.execute(Request::new(2, Command::Stop), Origin::Ui);
        let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
            panic!("snapshot missing")
        };
        assert_features(&snapshot, cover, cover && tags);
        let subscription = core
            .subscribe(Origin::Ui, SubscribeOptions::default())
            .unwrap();
        assert_features(&subscription.snapshot, cover, cover && tags);
    }
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
    // The real Library adapter must also forward its backend, rather than a fixed list.
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let db = dir.path().canonicalize().unwrap().join("library.db");
    let backend = Arc::new(local::LocalStorage::default());
    let lib = mpmp_library::Library::open_with(
        &db,
        mpmp_library::OpenOptions {
            storage: Some(backend.backends()),
            migrate: true,
        },
    )
    .unwrap();
    assert_eq!(
        crate::catalog::CatalogChecks::storage_write_capabilities(&lib),
        mpmp_storage::WriteCapabilities {
            durable_replace: false,
            legacy_tag_write: false,
        }
    );
    let core = start(Arc::new(lib), config());
    assert_features(&core.inner.published.load().snapshot, false, false);
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
}

#[test]
fn unsupported_tag_storage_maps_to_existing_not_implemented_contract_error() {
    let error = crate::catalog::library_error(mpmp_library::LibraryError::from(
        mpmp_storage::StorageError::Unsupported {
            operation: mpmp_storage::Operation::TagWrite,
        },
    ));
    assert_eq!(error.code, ErrorCode::NotImplemented);
    assert_eq!(error.kind, ErrorKind::Unsupported);
}
