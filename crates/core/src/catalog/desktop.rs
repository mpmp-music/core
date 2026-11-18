//! Desktop location rules used by core permissions.

use crate::permissions::{Doubt, Locations};

/// The desktop's location rules (file system paths) for core's permission scopes.
#[cfg(feature = "desktop-fs")]
impl Locations for mpmp_fs_desktop::DesktopLocations {
    fn normalize(&self, location: &str) -> Option<String> {
        mpmp_fs_desktop::DesktopLocations::normalize(self, location)
    }

    fn contains(&self, scope: &str, location: &str, doubt: Doubt) -> bool {
        mpmp_fs_desktop::DesktopLocations::contains(self, scope, location, doubt == Doubt::Include)
    }

    fn child(&self, location: &str, relative: &str) -> Option<String> {
        mpmp_fs_desktop::DesktopLocations::child(self, location, relative)
    }

    fn ignores_case(&self, location: &str) -> Option<bool> {
        mpmp_fs_desktop::DesktopLocations::ignores_case(self, location)
    }

    fn display(&self, location: &str) -> String {
        mpmp_fs_desktop::DesktopLocations::display(self, location)
    }

    fn well_formed(&self, location: &str) -> bool {
        mpmp_fs_desktop::DesktopLocations::well_formed(self, location)
    }

    fn nearest(&self, location: &str) -> Option<String> {
        mpmp_fs_desktop::DesktopLocations::nearest(self, location)
    }
}
