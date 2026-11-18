use super::*;

/// A held agent command the user approved, to check again before it runs: what it acts
/// on is worked out again and must be what was checked and shown (a symlink put in its
/// way while it waited: `location_changed`), and the grants are asked again (a `forbid`
/// given meanwhile refuses it: `permission_denied`; an `allow` changes nothing, the user
/// has answered). The same check serves any command and any number of targets.
#[derive(Clone)]
pub(crate) struct Recheck {
    pub sources: Option<Vec<mpmp_library::CoverSource>>,
    pub command: Command,
    pub origin: Origin,
    pub shown: Vec<Target>,
    pub class: OperationClass,
    pub candidates: Vec<Grant>,
    pub catalog: Arc<dyn Catalog>,
    pub locations: Arc<dyn Locations>,
    pub limits: Limits,
}

impl Recheck {
    /// With the same time limit as when the command came. `spent`: as for [`Check::run`].
    pub fn run(self, spent: bool) -> Result<(), ContractError> {
        let bounded = if spent {
            self.limits.spent(&self.locations)
        } else {
            self.limits.budget(&self.locations)
        };
        let ctx = CheckContext {
            catalog: self.catalog.as_ref(),
            locations: &bounded,
        };
        let command = &self.command;
        let mut again = command.clone();
        let targets = ctx.targets(&mut again)?;
        if targets != self.shown || again != *command {
            if bounded.late() {
                return Err(unreadable_location(command, true));
            }
            let changed = |message| ContractError::invalid(ErrorCode::LocationChanged, message);
            return Err(match command_path(command) {
                Some(path) => changed(format!(
                    "{path} no longer leads where it did when the user was asked: it changed \
                     while the command waited"
                ))
                .with_field("command.path"),
                None => changed("what the command acts on changed while it waited".into()),
            });
        }
        if let (Some(expected), Command::EditCover { tracks, .. }) = (&self.sources, command) {
            let (catalog, ids) = (Arc::clone(&self.catalog), tracks.clone());
            let current = bounded
                .run(move || catalog.cover_sources(&ids))
                .ok_or_else(|| unreadable_location(command, true))??;
            if current != *expected {
                return Err(ContractError::new(
                    crate::contract::ErrorKind::Conflict,
                    ErrorCode::FileChanged,
                    "cover source identity or contents changed while awaiting confirmation",
                ));
            }
        }
        match ctx.decide(&self.candidates, command, self.class, &targets) {
            Decision::Forbid(ids) => Err(permission_denied(
                &self.origin,
                command,
                &targets,
                &self.candidates,
                &ids,
            )),
            _ => Ok(()),
        }
    }
}
