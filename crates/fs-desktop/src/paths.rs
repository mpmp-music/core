//! Folder and file paths on desktop: normal forms and comparison,
//! letter case, file identity, opening a regular file. Unix and Windows differ only where
//! `#[cfg(unix)]` says so (no `#[cfg(target_os)]`).

mod normalize;
#[cfg(test)]
use normalize::probe_inside;
#[cfg(all(unix, test))]
use normalize::probe_name;
pub use normalize::*;
mod identity;
pub use identity::*;
mod open;
pub use open::*;
mod replace;
mod tag_probe;
#[cfg(test)]
use replace::seal_with;
pub use replace::*;
#[cfg(all(unix, test))]
use replace::{exchange_unsupported, publish_prepared_with, unsupported_exchange_code};
use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
pub(crate) use tag_probe::*;

#[cfg(test)]
mod tests;
