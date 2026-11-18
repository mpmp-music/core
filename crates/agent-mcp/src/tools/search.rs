use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    text: Option<String>,
    root: Option<i64>,
    folder: Option<String>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

pub(super) async fn search_tracks(session: &Session, args: &JsonObject) -> Outcome {
    let a: SearchArgs = parse(args)?;
    // An empty text or folder is none; a folder of only separators is none too.
    let text = a.text.filter(|t| !t.trim().is_empty());
    let folder = a
        .folder
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.chars().all(|c| c == '/' || c == '\\'));
    let (root, folder) = match folder {
        Some(f) if is_absolute(f) => {
            let roots = match session.query(Query::Roots).await? {
                QueryResult::Roots { roots } => roots,
                other => return Err(unexpected(&other)),
            };
            let roots: Vec<(i64, &str)> = roots.iter().map(|r| (r.id.0, r.path.as_str())).collect();
            let (found, inside) = split_root(&roots, f, cfg!(windows)).ok_or_else(|| {
                invalid(
                    concat!(
"folder should be a place inside a music folder, or an absolute path under ",
"one; see library_folders list for the music folders",
),
                    "folder",
                )
            })?;
            if a.root.is_some_and(|given| given != found) {
                return Err(invalid(
                    &format!(
                        "folder is inside music folder {found}, but root is {}",
                        a.root.unwrap_or_default()
                    ),
                    "folder",
                ));
            }
            (Some(found), Some(inside))
        }
        other => (a.root, other.map(str::to_owned)),
    };
    let query = Query::Tracks {
        query: TrackQuery {
            filter: search_filter(text, root, folder.as_deref())?,
            sort: Vec::new(),
        },
        offset: a.offset,
        limit: page_limit(a.limit, 20),
        tags: Vec::new(),
    };
    match session.query(query).await? {
        QueryResult::Tracks {
            tracks,
            offset,
            total,
            next_offset,
        } => Ok(success(json!({
            "tracks": tracks.iter().map(track).collect::<Vec<_>>(),
            "offset": offset, "total": total, "next_offset": next_offset,
        }))),
        other => Err(unexpected(&other)),
    }
}

/// Whether a folder is written as an absolute path: a drive (`D:`), or a leading `/` or `\`.
pub(super) fn is_absolute(folder: &str) -> bool {
    let b = folder.as_bytes();
    folder.starts_with(['/', '\\']) || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// The parts of a path, `/` or `\` separated, without empty ones (so repeated separators
/// merge).
fn parts(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|p| !p.is_empty()).collect()
}

/// The music folder that holds the absolute `folder`, and the place in it, `/`-separated.
/// The deepest one wins. Windows compares without letter case (`ignore_case`); `\` and `/`
/// are the same everywhere in a folder given by an agent.
pub(super) fn split_root(
    roots: &[(i64, &str)],
    folder: &str,
    ignore_case: bool,
) -> Option<(i64, String)> {
    let same = |a: &str, b: &str| match ignore_case {
        true => a.to_lowercase() == b.to_lowercase(),
        false => a == b,
    };
    let want = parts(folder);
    roots
        .iter()
        .filter_map(|&(id, path)| {
            let have = parts(path);
            let inside =
                want.len() >= have.len() && have.iter().zip(&want).all(|(h, w)| same(h, w));
            inside.then(|| (have.len(), id, want[have.len()..].join("/")))
        })
        .max_by_key(|&(depth, ..)| depth)
        .map(|(_, id, rest)| (id, rest))
}

/// The contract's filter for a search: the text (`search`), and the place: the music folder
/// (`root` equals) and a folder in it (`path` starts with `<folder>/`; `/`-separated as
/// `TrackInfo.path`, compared as the contract compares text, ignoring letter case). Only
/// what the contract has: nothing here reads the file system. An empty text or folder is
/// none; with none of the three, nothing is listed.
pub(super) fn search_filter(
    text: Option<String>,
    root: Option<i64>,
    folder: Option<&str>,
) -> Result<Filter, ContractError> {
    let text = text.filter(|t| !t.trim().is_empty());
    // Backslashes as a Windows path writes them; repeated separators merge; no leading or
    // trailing one.
    let folder = folder.map(|f| parts(f).join("/")).filter(|f| !f.is_empty());
    if text.is_none() && root.is_none() && folder.is_none() {
        return Err(invalid("give text, root, folder, or some of them", "text"));
    }
    let mut all = Vec::new();
    if let Some(text) = text {
        all.push(Filter::Search { text });
    }
    if let Some(root) = root {
        all.push(Filter::Compare {
            field: Field::Root,
            op: CompareOp::Eq,
            value: mpmp_core::contract::Value::Int(root),
        });
    }
    if let Some(folder) = folder {
        all.push(Filter::Text {
            field: Field::Path,
            op: TextOp::StartsWith,
            text: format!("{folder}/"),
        });
    }
    Ok(match all.len() {
        1 => all.remove(0),
        _ => Filter::And { all },
    })
}
