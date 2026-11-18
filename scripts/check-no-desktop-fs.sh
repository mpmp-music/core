#!/usr/bin/env bash
# Isolated packages and external consumers prevent workspace/dev feature union from
# making this check pass accidentally. All Cargo commands are strictly offline.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$root"
run() { timeout "${CHECK_TIMEOUT:-600}" cargo "$@"; }
assert_tree() {
    local tree
    tree=$(run tree --offline --prefix none -e normal,build "$@")
    if printf '%s\n' "$tree" | grep -E '^(mpmp-fs-desktop|mpmp-desktop-integration|mpmp-test-support|tauri(-[^ ]+)?)( |$)'; then
        echo "Unexpected desktop dependency in: $*" >&2
        exit 1
    fi
}
for package in mpmp-library mpmp-core; do
    run check --offline -p "$package" --no-default-features --lib
    run clippy --offline -p "$package" --no-default-features --lib -- -D warnings
    assert_tree -p "$package" --no-default-features
    run tree --offline -p "$package" --no-default-features -e features > /dev/null
    run test --offline -p "$package" --no-default-features
    run clippy --offline -p "$package" --no-default-features --all-targets -- -D warnings
done
run check --offline -p mpmp-agent-mcp --no-default-features --lib
bridge_tree=$(run tree --offline -p mpmp-mcp --prefix none -e normal,build)
if printf '%s\n' "$bridge_tree" | grep -E '^(mpmp-core|mpmp-library) '; then
    echo 'The bridge must not depend on core/library' >&2
    exit 1
fi
bridge_features=$(run tree --offline -p mpmp-mcp -e normal,features)
if printf '%s\n' "$bridge_features" | grep -E 'mpmp-agent-mcp feature "server"'; then
    echo 'The bridge must not enable mpmp-agent-mcp/server' >&2
    exit 1
fi
# A fresh manifest with its own workspace, no desktop/dev helper dependencies.
mkdir -p target
consumer=$(mktemp -d "$root/target/no-desktop-consumer.XXXXXX")
trap 'rm -rf "$consumer"' EXIT
# Reuse build artifacts, but never feature selection or the workspace manifest.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
for package in mpmp-library mpmp-core; do
    mkdir -p "$consumer/$package/src"
    crate_dir=library
    if [[ $package == mpmp-core ]]; then crate_dir=core; fi
    cat > "$consumer/$package/Cargo.toml" <<EOF
[package]
name = "no-desktop-$package"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
$package = { path = "$root/crates/$crate_dir", default-features = false }
mpmp-storage = { path = "$root/crates/storage" }
[dev-dependencies]
mpmp-test-support = { path = "$root/crates/test-support" }
EOF
    if [[ $package == mpmp-library ]]; then
        cat > "$consumer/$package/src/lib.rs" <<EOF
#[cfg(test)]
use mpmp_test_support::local_storage as local;
#[test]
fn injected_storage() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let missing = base.join("absent/library.db");
    for result in [
        mpmp_library::Library::open(&missing),
        mpmp_library::Library::open_with(
            &missing,
            mpmp_library::OpenOptions { storage: None, migrate: false },
        ),
    ] {
        assert!(matches!(result, Err(mpmp_library::LibraryError::Storage(mpmp_storage::StorageError::Configuration(_)))));
        assert!(!missing.parent().unwrap().exists());
    }
    let backend = std::sync::Arc::new(local::LocalStorage::default());
    let db = base.join("injected/library.db");
    let lib = mpmp_library::Library::open_with(
        &db,
        mpmp_library::OpenOptions::with_storage(backend.backends()),
    ).unwrap();
    assert!(!lib.storage_write_capabilities().durable_replace);
    lib.flush().unwrap();
}
EOF
    else
        cat >> "$consumer/$package/Cargo.toml" <<EOF
[dependencies.mpmp-engine]
path = "$root/crates/engine"
[dependencies.mpmp-media-io]
path = "$root/crates/media-io"
EOF
        cat > "$consumer/$package/src/main.rs" <<'RS'
use std::{sync::Arc, time::Duration};
use mpmp_core::{CoreConfig, CoreHandle, Locations, Doubt, PlayRecord, contract::*};
struct CatalogStub;
impl mpmp_core::catalog::Browse for CatalogStub {
    fn tracks(&self, _: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> { Ok(vec![]) }

    fn uri_of(&self, _: TrackId) -> Result<Option<mpmp_media_io::MediaUri>, ContractError> { Ok(None) }

    fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> { Ok(vec![]) }

    fn query(&self, _: &Query) -> QueryResult { QueryResult::Unknown }
}

impl mpmp_core::catalog::Playlists for CatalogStub {}

impl mpmp_core::catalog::LibraryRoots for CatalogStub {}

impl mpmp_core::catalog::Actions for CatalogStub {}

impl mpmp_core::catalog::CatalogChecks for CatalogStub {}

impl mpmp_core::catalog::AgentPermissions for CatalogStub {}

impl mpmp_core::catalog::MediaResources for CatalogStub {}

impl mpmp_core::catalog::Loudness for CatalogStub {}

impl mpmp_core::catalog::TagsAndCovers for CatalogStub {}

impl mpmp_core::catalog::PlaybackHistory for CatalogStub {
    fn record_play(&self, _: PlayRecord) {}
}
struct LocalLocations;
impl Locations for LocalLocations {
    fn normalize(&self, p: &str) -> Option<String> { Some(p.into()) }
    fn contains(&self, a: &str, b: &str, _: Doubt) -> bool { a == b }
    fn child(&self, _: &str, _: &str) -> Option<String> { None }
    fn ignores_case(&self, _: &str) -> Option<bool> { Some(false) }
    fn display(&self, p: &str) -> String { p.into() }
}
fn start(config: CoreConfig) -> std::io::Result<CoreHandle> {
    CoreHandle::start(Box::new(mpmp_engine::NullOutput::new(mpmp_engine::output::StreamFormat { sample_rate: 44_100, channels: 2, sample_type: mpmp_engine::output::SampleType::F32 })), Arc::new(mpmp_media_io::FileResolver), Arc::new(CatalogStub), config)
}
fn main() {
    let state = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("absent/state.json");
    let error = start(CoreConfig { state_file: Some(state.clone()), ..Default::default() }).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!state.parent().unwrap().exists());
    let core = start(CoreConfig { locations: Some(Arc::new(LocalLocations)), startup_scan_by_host: true, automatic_loudness_scan: Some(false), ..Default::default() }).unwrap();
    let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else { panic!("no snapshot") };
    assert!(!snapshot.features.iter().any(|f| f == "cover_edit" || f == "edit_tags"));
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
}
RS
    fi
    run build --offline --manifest-path "$consumer/$package/Cargo.toml"
    assert_tree --manifest-path "$consumer/$package/Cargo.toml"
    if [[ $package == mpmp-library ]]; then
        run test --offline --manifest-path "$consumer/$package/Cargo.toml"
    else
        run run --offline --manifest-path "$consumer/$package/Cargo.toml"
    fi
done
