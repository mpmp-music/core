use super::*;

/// An agent's command to check before it runs or is held, with all the checks need
/// (taken by the actor: its policy, class and candidate grants as they are now).
#[derive(Clone)]
pub(crate) struct Check {
    pub command: Command,
    pub origin: Origin,
    pub policy: Policy,
    pub class: OperationClass,
    pub candidates: Vec<Grant>,
    pub catalog: Arc<dyn Catalog>,
    pub locations: Arc<dyn Locations>,
    pub limits: Limits,
}

impl Check {
    /// The checks: the user's grants decide for an agent (a forbid refuses any
    /// command, an allow runs one that would ask); a location the agent names is
    /// normalized here, and the grants, the user and the command all see that (checked
    /// again when the user approves, and by the library right before it is used). A
    /// command that would ask is first checked to be able to run, and asks no one if it
    /// would change nothing. `spent`: with a time limit used up already (every file
    /// system call answers at once as late).
    pub fn run(self, spent: bool) -> Checked {
        let bounded = if spent {
            self.limits.spent(&self.locations)
        } else {
            self.limits.budget(&self.locations)
        };
        self.checked(&bounded).unwrap_or_else(Checked::Refused)
    }

    fn checked(self, bounded: &Bounded) -> Result<Checked, ContractError> {
        let Check {
            mut command,
            origin,
            policy,
            class,
            candidates,
            catalog,
            ..
        } = self;
        let ctx = CheckContext {
            catalog: catalog.as_ref(),
            locations: bounded,
        };
        let mut grant = None;
        let mut targets = Vec::new();
        if policy == Policy::Confirm || !candidates.is_empty() {
            // A tag edit's tracks and changes are checked before its files are looked for
            // (the time limit could end that first): the answer depends on nothing else
            if matches!(
                command,
                Command::EditTags { .. } | Command::EditCover { .. }
            ) {
                catalog.precheck(&command)?;
            }
            targets = ctx.targets(&mut command)?;
            let late = bounded.late();
            // Its location did not answer in time: not even where it is by its name is
            // known, so every forbid would take it to be inside. The answer depends on
            // no grant.
            if late && targets.iter().any(|t| t.unreadable() || t.nowhere()) {
                return Err(unreadable_location(&command, true));
            }
            match ctx.decide(&candidates, &command, class, &targets) {
                Decision::Forbid(ids) => {
                    return Err(permission_denied(
                        &origin,
                        &command,
                        &targets,
                        &candidates,
                        &ids,
                    ));
                }
                Decision::Allow(id) if policy == Policy::Confirm => grant = Some(id),
                _ => {}
            }
            if targets.iter().any(Target::unreadable) {
                return Err(unreadable_location(&command, late));
            }
            if let Some(error) = unreadable_files(&command, &targets) {
                return Err(error);
            }
        }
        if policy == Policy::Confirm {
            // Never ask the user to approve a command that cannot run, or one that would
            // change nothing (it runs right away and reports `changed: false`).
            precheck(&catalog, &command, Some(bounded))?;
            if grant.is_none() && !is_noop(&catalog, &command, Some(bounded)) {
                let mut view = ctx.view(&command, &targets);
                if let Command::EditTags { tracks, changes } = &command {
                    let (catalog, tracks, changes) =
                        (Arc::clone(&catalog), tracks.clone(), changes.clone());
                    let preview = bounded
                        .run(move || {
                            catalog
                                .query(&crate::contract::Query::TagEditPreview { tracks, changes })
                        })
                        .ok_or_else(|| unreadable_location(&command, true))?;
                    match preview {
                        crate::contract::QueryResult::TagEditPreview { files, skipped } => {
                            if files.is_empty() && !skipped.is_empty() {
                                let code = skipped
                                    .first()
                                    .map(|f| f.code)
                                    .filter(|code| *code != ErrorCode::Unknown)
                                    .unwrap_or(ErrorCode::InvalidTag);
                                return Err(ContractError::new(code.kind(), code,

                                    "every file is skipped by the tag edit preview; no file can be written"
                                    )
                                    .with_field("command.tracks"));
                            }
                            if !skipped.is_empty() {
                                use std::fmt::Write;
                                view.summary.push_str(". Preview exclusions");
                                for file in &skipped {
                                    write!(
                                        view.summary,
                                        " #{} ({})",
                                        file.track.0,
                                        file.code.as_str()
                                    )
                                    .expect("write to string");
                                }
                            }
                            let converted = files
                                .iter()
                                .filter(|f| f.encoding_conversion == Some(true))
                                .count();
                            if converted > 0 {
                                let noun = if converted == 1 { "file" } else { "files" };
                                view.summary.push_str(&format!(
                                    concat!(
"; {converted} {noun} will have legacy text converted to standard Unicode ",
"encoding",
), converted = converted, noun = noun,));
                            }
                        }
                        crate::contract::QueryResult::Err { error } => return Err(error),
                        _ => {}
                    }
                }
                if let Command::EditCover { tracks, change } = &command {
                    let (preview_catalog, tracks, change) =
                        (Arc::clone(&catalog), tracks.clone(), change.clone());
                    let preview = bounded
                        .run(move || {
                            preview_catalog
                                .query(&crate::contract::Query::CoverEditPreview { tracks, change })
                        })
                        .ok_or_else(|| unreadable_location(&command, true))?;
                    match preview {
                        crate::contract::QueryResult::CoverEditPreview { files, skipped } => {
                            if files.is_empty() && !skipped.is_empty() {
                                let code = skipped[0].code;
                                return Err(ContractError::new(
                                    code.kind(),
                                    code,
                                    "no cover target can be written",
                                ));
                            }
                            let (catalog, ids) = (
                                Arc::clone(&catalog),
                                if let Command::EditCover { tracks, .. } = &command {
                                    tracks.clone()
                                } else {
                                    vec![]
                                },
                            );
                            view.sources = Some(
                                bounded
                                    .run(move || catalog.cover_sources(&ids))
                                    .ok_or_else(|| unreadable_location(&command, true))??,
                            );
                            for image in files.iter().filter_map(|f| f.before.as_ref()).take(5) {
                                view.summary.push_str(&format!(
                                    "; old image {} bytes {}, SHA-256 {}",
                                    image.bytes,
                                    image.mime,
                                    image.sha256.chars().take(12).collect::<String>()
                                ));
                            }
                            let mp4 = files
                                .iter()
                                .filter(|f| f.scope == crate::contract::CoverScope::AllCovr)
                                .count();
                            view.summary.push_str(&format!(concat!(
"; {} files, {mp4} MP4 files: all covr items will be replaced or removed; ",
"any later change to a file makes its cover undo conflict",
),files.len(), mp4 = mp4,));
                        }
                        crate::contract::QueryResult::Err { error } => return Err(error),
                        _ => {}
                    }
                }
                return Ok(Checked::Hold {
                    command,
                    class,
                    targets,
                    view: Box::new(view),
                });
            }
        }
        Ok(Checked::Run {
            command,
            grant,
            targets,
        })
    }
}
