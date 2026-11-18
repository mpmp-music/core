//! Production schema inspection, or explicit relabel-only acceptance on a stopped copy.
use mpmp_library::Library;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const USAGE: &str = concat!(
        "usage: schema_probe [--relabel-only] DATABASE ",
        "(writes only with --relabel-only; use a stopped copy)"
    );
    let mut args = std::env::args_os().skip(1);
    let first = args.next().ok_or(USAGE)?;
    let relabel = first == "--relabel-only";
    let path = PathBuf::from(if relabel {
        args.next().ok_or(USAGE)?
    } else {
        first
    });
    if args.next().is_some() {
        return Err(USAGE.into());
    }
    if relabel {
        #[cfg(feature = "desktop-fs")]
        {
            Library::relabel_schema_copy(&path, &mpmp_fs_desktop::DesktopStorage::backends())?;
            println!("relabel-only complete: published schema 1; normal startup has not run");
        }
        #[cfg(not(feature = "desktop-fs"))]
        return Err("--relabel-only requires the desktop-fs feature".into());
    } else {
        println!("{}", Library::inspect_schema(&path)?);
    }
    Ok(())
}
