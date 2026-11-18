use super::*;

impl CheckContext<'_> {
    /// What a command acts on, for grant scopes (the table on `contract::COMMANDS`). The
    /// location a command names is put in the platform layer's normal form, in the command
    /// itself too (it then runs with that); one the platform cannot read is
    /// [`Target::Unreadable`] and stays as it was. Music folders (added, removed, or by an
    /// undo) are folder targets; the files whose tags are edited (or written back by an
    /// undo) file targets, one per track in the order of [`unique`]. A location that does
    /// not answer in time cannot be read.
    pub fn targets(&self, command: &mut Command) -> Result<Vec<Target>, ContractError> {
        let locations = self.locations;
        // A folder's location in normal form, as for other targets (a symlink put in its
        // place leads elsewhere); unknown when it has none.
        let root = |root| -> Result<Target, ContractError> {
            let stored = self.catalog.root_location(root)?;
            let location = stored.as_deref().and_then(|s| locations.normalize(s));
            // Not there now (a drive that is not plugged in): where it is by its name.
            let named = match (&location, &stored) {
                (None, Some(stored)) => locations.nearest(stored),
                _ => None,
            };
            Ok(Target::Library {
                root,
                rel: String::new(),
                location,
                named,
                folder: true,
            })
        };
        let folder = matches!(command, Command::AddRoot { .. });
        Ok(match command {
            Command::AddRoot { path }
            | Command::ImportM3u { path, .. }
            | Command::ExportM3u { path, .. } => {
                // Told from the text alone, before any grant is looked at: the answer
                // depends on nothing else, so it tells nothing of them.
                if path.chars().count() > MAX_LOCATION || !locations.well_formed(path) {
                    return Err(malformed_location(path));
                }
                match locations.normalize(path) {
                    Some(normal) => {
                        path.clone_from(&normal);
                        vec![Target::Location {
                            location: normal,
                            folder,
                        }]
                    }
                    None => vec![Target::Unreadable {
                        named: locations.nearest(path),
                        folder,
                    }],
                }
            }
            Command::RemoveRoot { root: id }
            | Command::ReleaseRoot { root: id }
            | Command::ResumeRoot { root: id } => vec![root(*id)?],
            Command::UndoAction { action } => {
                let roots = self.catalog.action_roots(*action)?;
                if roots.is_empty() {
                    self.files(&self.catalog.tag_undo_tracks(*action)?, "command.action")?
                } else {
                    roots.into_iter().map(root).collect::<Result<_, _>>()?
                }
            }
            Command::ResolveCoverRecovery { track, .. } => {
                self.files(&[*track], "command.track")?
            }
            Command::EditTags { tracks, .. } | Command::EditCover { tracks, .. } => {
                self.files(&unique(tracks), "command.tracks")?
            }
            _ => Vec::new(),
        })
    }

    /// The files of `tracks` (each once), as file targets in a library root, each at the
    /// location of its root's location and its path there, in normal form (where it really
    /// is). A file that cannot be read (missing) has none (no `allow` contains it), and is
    /// where it is by its name for a `forbid`. Unknown tracks: `not_found`.
    fn files(&self, tracks: &[TrackId], field: &str) -> Result<Vec<Target>, ContractError> {
        let infos = self.catalog.tracks(tracks)?;
        if infos.len() != tracks.len() {
            check_tracks_exist(self.catalog, tracks, field)?;
        }
        let locations = self.locations;
        infos
            .iter()
            .map(|track| {
                let stored = self.catalog.root_location(track.root)?;
                let place = stored
                    .as_deref()
                    .and_then(|root| locations.child(root, &track.path));
                let location = place.as_deref().and_then(|p| locations.normalize(p));
                let named = match (&location, &place) {
                    (None, Some(place)) => locations.nearest(place),
                    _ => None,
                };
                Ok(Target::Library {
                    root: track.root,
                    rel: track.path.clone(),
                    location,
                    named,
                    folder: false,
                })
            })
            .collect()
    }

    /// What an agent's candidate grants say about its command.
    pub fn decide(
        &self,
        candidates: &[Grant],
        command: &Command,
        class: OperationClass,
        targets: &[Target],
    ) -> Decision {
        let request = permissions::Request {
            command: command.name(),
            class,
            targets,
        };
        permissions::decide(candidates, &request, |scope, target, doubt| {
            permissions::scope_reach(scope, target, doubt, self.locations, |root| {
                self.catalog.root_location(root).ok().flatten()
            })
        })
    }

    /// What a confirmation of the command shows: its summary, the smallest scope with all
    /// it acts on, and that place as the user should read it (best effort: it only
    /// informs).
    pub fn view(&self, command: &Command, targets: &[Target]) -> View {
        let Placed {
            targets: placed,
            elsewhere,
            unread,
            offline_folders,
        } = self.placed(targets);
        // A file that cannot be read now is in no scope short of `all` (no `allow`
        // contains it), so none would cover the command.
        let scope = match unread + offline_folders {
            0 => permissions::common_scope(&placed),
            _ => None,
        };
        let mut summary = confirmation_summary(command, targets);
        let files = targets.iter().filter(|t| !t.folder()).count();
        let which = |n: usize| match (n, n == files) {
            (1, true) => "the file is".to_owned(),
            (_, true) => "the files are".to_owned(),
            (1, false) => "1 of the files is".to_owned(),
            (n, false) => format!("{n} of the files are"),
        };
        if let Some(first) = elsewhere.first() {
            let more = match elsewhere.len() {
                1 => String::new(),
                n => format!(", and {} more", n - 1),
            };
            summary.push_str(&format!(
                "; {} not in the music folder but where a link from it leads: {}{more}",
                which(elsewhere.len()),
                self.locations.display(first)
            ));
        }
        if unread > 0 {
            summary.push_str(&format!(
                "; {} not there now (gone, or on a drive that is not plugged in) and \
                 skipped",
                which(unread)
            ));
        }
        if offline_folders > 0 {
            summary.push_str(concat!(
                "; the library folder is not reachable now (gone, or on a drive that is ",
                "not plugged in)",
            ));
        }
        let place = match &scope {
            Some(Scope::Location { location }) => Some(location.clone()),
            Some(Scope::Library { root, prefix }) => {
                let root = self.catalog.root_location(*root).ok().flatten();
                match prefix {
                    None => root,
                    Some(prefix) => root.and_then(|r| self.locations.child(&r, prefix)),
                }
            }
            _ => None,
        };
        View {
            sources: None,
            summary,
            scope,
            display_location: place.map(|place| self.locations.display(&place)),
        }
    }

    /// The targets as a confirmation shows where they are: a
    /// file whose location is not where its path in its music folder leads (a symlink out
    /// of the folder, or to elsewhere in it) where it really is, so that the scope offered
    /// is one a grant for covers the command (a library scope holds a file only where
    /// both its path and its location are inside); the others as they are.
    fn placed(&self, targets: &[Target]) -> Placed {
        let mut roots: HashMap<RootId, Option<String>> = HashMap::new();
        let mut elsewhere = Vec::new();
        let mut unread = 0;
        let mut offline_folders = 0;
        let placed = targets
            .iter()
            .map(|target| {
                if matches!(
                    target,
                    Target::Library {
                        location: None,
                        folder: true,
                        ..
                    }
                ) {
                    offline_folders += 1;
                    return target.clone();
                }
                let Target::Library {
                    root,
                    rel,
                    location,
                    folder: false,
                    ..
                } = target
                else {
                    return target.clone();
                };
                let Some(location) = location else {
                    unread += 1;
                    return target.clone();
                };
                let real = roots.entry(*root).or_insert_with(|| {
                    let stored = self.catalog.root_location(*root).ok().flatten()?;
                    self.locations.normalize(&stored)
                });
                let by_path = real.as_deref().and_then(|r| self.locations.child(r, rel));
                if by_path.as_deref().is_some_and(|path| {
                    self.locations.contains(path, location, Doubt::Exclude)
                        && self.locations.contains(location, path, Doubt::Exclude)
                }) {
                    return target.clone();
                }
                elsewhere.push(location.clone());
                Target::Location {
                    location: location.clone(),
                    folder: false,
                }
            })
            .collect();
        Placed {
            targets: placed,
            elsewhere,
            unread,
            offline_folders,
        }
    }
}

/// [`CheckContext::placed`].
struct Placed {
    targets: Vec<Target>,
    /// The real locations of the files that are not where their paths lead.
    elsewhere: Vec<String>,
    /// How many files cannot be read now.
    unread: usize,
    offline_folders: usize,
}
