use super::*;

// ---- field mapping (the whitelist) ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Text,
    Int,
    Bool,
    Timestamp,
}

/// SQL for one column field. All strings are constants.
pub(super) struct Column {
    pub(super) kind: Kind,
    /// The value as shown (display text for text fields).
    pub(super) value: &'static str,
    /// The comparable value: the search key for text fields, else `value`.
    pub(super) key: &'static str,
    /// Condition for "no value".
    pub(super) missing: &'static str,
    stats: bool,
    user: bool,
    /// For statistics and user data: the side table, so filters can test set membership
    /// (`t.id IN (SELECT track_id...)`) instead of joining every track.
    side: Option<Side>,
}

/// A column of `track_stats` or `track_user`; tracks without a row have `default`.
#[derive(Clone, Copy)]
pub(super) struct Side {
    table: &'static str,
    column: &'static str,
    default: Option<i64>,
}

pub(super) const fn text(value: &'static str, key: &'static str, missing: &'static str) -> Column {
    Column {
        kind: Kind::Text,
        value,
        key,
        missing,
        stats: false,
        user: false,
        side: None,
    }
}

pub(super) const fn of(kind: Kind, value: &'static str, missing: &'static str) -> Column {
    Column {
        kind,
        value,
        key: value,
        missing,
        stats: false,
        user: false,
        side: None,
    }
}

pub(super) const fn stat(
    kind: Kind,
    value: &'static str,
    missing: &'static str,
    column: &'static str,
    default: Option<i64>,
) -> Column {
    Column {
        stats: true,
        side: Some(Side {
            table: "track_stats",
            column,
            default,
        }),
        ..of(kind, value, missing)
    }
}

pub(super) const fn user(
    kind: Kind,
    value: &'static str,
    missing: &'static str,
    column: &'static str,
    default: Option<i64>,
) -> Column {
    Column {
        user: true,
        side: Some(Side {
            table: "track_user",
            column,
            default,
        }),
        ..of(kind, value, missing)
    }
}

/// The whitelist. `None` for [`Field::Tag`], which goes through `track_tags`.
///
/// Text fields store an empty search key (not NULL) when the tag is missing; their `key`
/// turns it back into NULL, so a missing value sorts last either way and satisfies no
/// comparison except `ne`, exactly like `missing` and the facets' unknown group say.
///
/// Title is the exception: its search key falls back to the file name, so an untagged
/// track sorts, compares and matches text by its file name, while `Missing { Title }` (and
/// the unknown group of a Title facet) means "no title tag".
pub(super) fn column(field: &Field) -> Option<Column> {
    use Kind::*;
    Some(match field {
        Field::Title => text(
            "t.title",
            "NULLIF(t.title_norm, '')",
            "(t.title IS NULL OR t.title = '')",
        ),
        Field::Artist => text(
            "t.artist",
            "NULLIF(t.artist_norm, '')",
            "t.artist_norm = ''",
        ),
        Field::Album => text("t.album", "NULLIF(t.album_norm, '')", "t.album_norm = ''"),
        Field::AlbumArtist => text(
            "t.album_artist",
            "NULLIF(t.album_artist_norm, '')",
            "t.album_artist_norm = ''",
        ),
        Field::GroupArtist => text(
            "t.album_group_artist",
            "NULLIF(t.group_artist_norm, '')",
            "t.group_artist_norm = ''",
        ),
        Field::Genre => text("t.genre", "NULLIF(t.genre_norm, '')", "t.genre_norm = ''"),
        Field::Codec => text(
            "t.codec",
            "NULLIF(t.codec, '')",
            "(t.codec IS NULL OR t.codec = '')",
        ),
        Field::Path => text("t.rel_path", "t.rel_path_norm", "0"),
        Field::Year => of(Int, "t.year", "t.year IS NULL"),
        Field::TrackNo => of(Int, "t.track_no", "t.track_no IS NULL"),
        Field::DiscNo => of(Int, "t.disc_no", "t.disc_no IS NULL"),
        Field::DurationMs => of(Int, "t.duration_ms", "t.duration_ms IS NULL"),
        Field::SampleRate => of(Int, "t.sample_rate", "t.sample_rate IS NULL"),
        Field::BitsPerSample => of(Int, "t.bits_per_sample", "t.bits_per_sample IS NULL"),
        Field::Channels => of(Int, "t.channels", "t.channels IS NULL"),
        Field::BitrateKbps => of(Int, "t.bitrate_kbps", "t.bitrate_kbps IS NULL"),
        Field::Root => of(Int, "t.root_id", "0"),
        Field::SizeBytes => of(Int, "t.size", "0"),
        Field::Compilation => of(Bool, "t.compilation", "0"),
        Field::Lossless => of(Bool, "t.lossless", "t.lossless IS NULL"),
        Field::Available => of(Bool, "t.available", "0"),
        Field::HasCover => of(Bool, "(t.has_embedded_art OR t.has_folder_art)", "0"),
        Field::CueSheet => of(Bool, "(t.cue_sheet IS NOT NULL)", "0"),
        Field::ModifiedAt => of(Timestamp, "t.mtime_ms", "0"),
        Field::AddedAt => of(Timestamp, "t.added_at", "0"),
        Field::PlayCount => stat(Int, "COALESCE(s.play_count, 0)", "0", "play_count", Some(0)),
        Field::SkipCount => stat(Int, "COALESCE(s.skip_count, 0)", "0", "skip_count", Some(0)),
        Field::LastPlayedAt => stat(
            Timestamp,
            "s.last_played_at",
            "s.last_played_at IS NULL",
            "last_played_at",
            None,
        ),
        Field::FirstPlayedAt => stat(
            Timestamp,
            "s.first_played_at",
            "s.first_played_at IS NULL",
            "first_played_at",
            None,
        ),
        Field::Rating => user(Int, "u.rating", "u.rating IS NULL", "rating", None),
        Field::Loved => user(Bool, "COALESCE(u.loved, 0)", "0", "loved", Some(0)),
        Field::Tag { .. } => return None,
    })
}

pub(super) fn invalid(message: String) -> LibraryError {
    LibraryError::InvalidQuery(message)
}

/// Deepest filter nesting accepted (protects the stack and SQLite's parser).
const MAX_DEPTH: usize = 64;

/// Most values a compiled query may bind (facets bind the filter twice; SQLite allows
/// 32766).
const MAX_PARAMS: usize = 10_000;

/// Checks the size limits that keep a filter within what SQLite can prepare: at most
/// [`MAX_WIDTH`] parts per `And` / `Or`, [`MAX_NODES`] nodes, [`MAX_DEPTH`] levels.
fn check_size(filter: &Filter) -> Result<()> {
    fn walk(filter: &Filter, depth: usize, nodes: &mut usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(invalid(format!("filter nested deeper than {MAX_DEPTH}")));
        }
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(invalid(format!(
                "filter has more than {MAX_NODES} conditions and groups"
            )));
        }
        match filter {
            Filter::And { all: parts } | Filter::Or { any: parts } => {
                if parts.len() > MAX_WIDTH {
                    let name = if matches!(filter, Filter::And { .. }) {
                        "and"
                    } else {
                        "or"
                    };
                    return Err(invalid(format!(
                        "{name} with {} parts (at most {MAX_WIDTH}); split it or narrow it",
                        parts.len()
                    )));
                }
                parts.iter().try_for_each(|p| walk(p, depth + 1, nodes))
            }
            Filter::Not { filter } => walk(filter, depth + 1, nodes),
            _ => Ok(()),
        }
    }
    walk(filter, 0, &mut 0)
}

/// SQL under construction: text plus positional (`?`) parameters in text order.
pub(super) struct Sql {
    pub(super) text: String,
    pub(super) params: Vec<SqlValue>,
    stats: bool,
    user: bool,
    now: i64,
}

impl Sql {
    pub(super) fn new() -> Self {
        Self {
            text: String::new(),
            params: Vec::new(),
            stats: false,
            user: false,
            now: now_ms(),
        }
    }

    fn push(&mut self, sql: &str) {
        self.text.push_str(sql);
    }

    fn bind(&mut self, value: impl Into<SqlValue>) {
        self.text.push('?');
        self.params.push(value.into());
    }

    pub(super) fn uses(&mut self, column: &Column) {
        self.stats |= column.stats;
        self.user |= column.user;
    }

    pub(super) fn joins(&self) -> &'static str {
        match (self.stats, self.user) {
            (false, false) => "",
            (true, false) => " LEFT JOIN track_stats s ON s.track_id = t.id",
            (false, true) => " LEFT JOIN track_user u ON u.track_id = t.id",
            (true, true) => {
                " LEFT JOIN track_stats s ON s.track_id = t.id \
                 LEFT JOIN track_user u ON u.track_id = t.id"
            }
        }
    }

    /// Compiles `filter` into a condition that is never NULL (so `NOT` behaves).
    pub(super) fn filter(&mut self, filter: &Filter, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(invalid(format!("filter nested deeper than {MAX_DEPTH}")));
        }
        match filter {
            Filter::All => self.push("1"),
            Filter::And { all } => self.join(all, " AND ", "1", depth)?,
            Filter::Or { any } => self.join(any, " OR ", "0", depth)?,
            Filter::Not { filter } => {
                self.push("NOT (");
                self.filter(filter, depth + 1)?;
                self.push(")");
            }
            Filter::Compare { field, op, value } => self.compare(field, *op, value)?,
            Filter::Text { field, op, text } => self.text_match(field, *op, text)?,
            Filter::ArtistPerson { name } => {
                self.push("t.id IN (SELECT track_id FROM track_artists WHERE artist_key=");
                self.bind(crate::person::person_key(&normalize(name)));
                self.push(")");
            }
            Filter::Search { text } => self.search(text),
            Filter::Missing { field } => match field {
                Field::Tag { key } => {
                    // Empty values are never stored; `(track_id, key)` covers this probe.
                    self.push(
                        "NOT EXISTS (SELECT 1 FROM track_tags g INDEXED BY track_tags_track \
                         WHERE g.track_id = t.id AND g.key = ",
                    );
                    self.bind(tag_key(key)?);
                    self.push(")");
                }
                _ => {
                    let column = column(field).expect("non-tag field");
                    self.uses(&column);
                    self.push("(");
                    self.push(column.missing);
                    self.push(")");
                }
            },
        }
        Ok(())
    }

    fn join(&mut self, parts: &[Filter], sep: &str, empty: &str, depth: usize) -> Result<()> {
        match parts {
            [] => {
                self.push(empty);
                Ok(())
            }
            [part] => self.filter(part, depth + 1),
            // A balanced tree: SQLite parses `a OR b OR c...` as a chain as deep as it is
            // long, and refuses expressions deeper than 1000.
            _ => {
                let (left, right) = parts.split_at(parts.len() / 2);
                self.push("(");
                self.join(left, sep, empty, depth)?;
                self.push(sep);
                self.join(right, sep, empty, depth)?;
                self.push(")");
                Ok(())
            }
        }
    }

    fn compare(&mut self, field: &Field, op: CompareOp, value: &Value) -> Result<()> {
        let sql_op = match op {
            CompareOp::Eq => "=",
            CompareOp::Ne => "IS NOT",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        };
        let mismatch = || {
            invalid(format!(
                "{op:?} on field {field:?} cannot take value {value:?}"
            ))
        };
        if let Field::Tag { key } = field {
            let Value::Text(text) = value else {
                return Err(mismatch());
            };
            // Ne: no value of the tag equals the text.
            let (negate, sql_op) = if op == CompareOp::Ne {
                (true, "=")
            } else {
                (false, sql_op)
            };
            // A set from the `(key, value_norm, track_id)` index: one range scan instead of a
            // probe per track.
            self.push(if negate { "t.id NOT IN" } else { "t.id IN" });
            self.push(" (SELECT g.track_id FROM track_tags g WHERE g.key = ");
            self.bind(tag_key(key)?);
            self.push(" AND g.value_norm ");
            self.push(sql_op);
            self.push(" ");
            self.bind(normalize(text));
            self.push(")");
            return Ok(());
        }
        let column = column(field).expect("non-tag field");
        let bound: SqlValue = match (column.kind, value) {
            (Kind::Text, Value::Text(text)) => normalize(text).into(),
            (Kind::Int, Value::Int(n)) => (*n).into(),
            (Kind::Bool, Value::Bool(b)) if matches!(op, CompareOp::Eq | CompareOp::Ne) => {
                i64::from(*b).into()
            }
            (Kind::Timestamp, Value::Timestamp(ms)) => (*ms).into(),
            (Kind::Timestamp, Value::DaysAgo(days)) => {
                (self.now - i64::from(*days) * 86_400_000).into()
            }
            _ => return Err(mismatch()),
        };
        if let Some(side) = column.side {
            // Tracks without a side row hold `default` (or nothing): the predicate is true
            // for them exactly when it holds for that value.
            let holds_without_row = match (side.default, &bound) {
                (Some(default), SqlValue::Integer(value)) => match op {
                    CompareOp::Eq => default == *value,
                    CompareOp::Ne => default != *value,
                    CompareOp::Lt => default < *value,
                    CompareOp::Le => default <= *value,
                    CompareOp::Gt => default > *value,
                    CompareOp::Ge => default >= *value,
                },
                // NULL IS NOT x is true; other comparisons with NULL are false.
                _ => op == CompareOp::Ne,
            };
            // Selective predicates ("loved", "rating >= 4", "played more than 5 times") are a
            // set of side rows; broad ones ("never played", "not loved") hold for most
            // tracks, and a join is cheaper than a large NOT IN set.
            if !holds_without_row {
                self.side_set(side, sql_op, bound);
                return Ok(());
            }
        }
        self.uses(&column);
        self.push("IFNULL(");
        self.push(column.key);
        self.push(" ");
        self.push(sql_op);
        self.push(" ");
        self.text.push('?');
        self.params.push(bound);
        self.push(", 0)");
        Ok(())
    }

    /// `t.id IN (SELECT track_id FROM <side table> WHERE <column> <op> ?)`.
    fn side_set(&mut self, side: Side, op: &str, value: SqlValue) {
        self.push("t.id IN (SELECT track_id FROM ");
        self.push(side.table);
        self.push(" WHERE ");
        self.push(side.column);
        self.push(" ");
        self.push(op);
        self.push(" ?)");
        self.params.push(value);
    }

    fn text_match(&mut self, field: &Field, op: TextOp, text: &str) -> Result<()> {
        let needle = normalize(text);
        let key = match field {
            Field::Tag { key } => {
                self.push("t.id IN (SELECT g.track_id FROM track_tags g WHERE g.key = ");
                self.bind(tag_key(key)?);
                self.push(" AND ");
                "g.value_norm"
            }
            _ => {
                let column = column(field).expect("non-tag field");
                if column.kind != Kind::Text {
                    return Err(invalid(format!(
                        "text match {op:?} needs a text field, not {field:?}"
                    )));
                }
                self.uses(&column);
                self.push("(");
                column.key
            }
        };
        self.push("IFNULL(");
        match op {
            TextOp::Equals => {
                self.push(key);
                self.push(" = ");
                self.bind(needle);
            }
            TextOp::Contains => {
                self.push("instr(");
                self.push(key);
                self.push(", ");
                self.bind(needle);
                self.push(") > 0");
            }
            TextOp::StartsWith => {
                let len = needle.chars().count() as i64;
                self.push("substr(");
                self.push(key);
                self.push(", 1, ");
                self.bind(len);
                self.push(") = ");
                self.bind(needle);
            }
        }
        self.push(", 0))");
        Ok(())
    }

    fn search(&mut self, text: &str) {
        let Some(query) = SearchQuery::parse(text) else {
            self.push("1");
            return;
        };
        self.push("(1");
        if let Some(expression) = query.fts_expression(&[]) {
            self.push(" AND t.id IN (SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ");
            self.bind(expression);
            self.push(")");
        }
        for term in &query.like_terms {
            self.push(" AND (");
            for (i, column) in reads::TRACK_SEARCH_COLUMNS.iter().enumerate() {
                if i > 0 {
                    self.push(" OR ");
                }
                self.push("instr(t.");
                self.push(column);
                self.push(", ");
                self.bind(term.clone());
                self.push(") > 0");
            }
            self.push(")");
        }
        self.push(")");
    }

    /// `ORDER BY` terms: the keys, then the stable tail.
    fn order(&mut self, sort: &[SortKey]) -> Result<()> {
        for key in sort {
            let direction = if key.descending { " DESC" } else { "" };
            match &key.field {
                Field::Tag { key: tag } => {
                    let tag = tag_key(tag)?;
                    // Twice: once for "missing last", once for the value.
                    for suffix in [" IS NULL, ", direction] {
                        self.push(
                            "(SELECT MIN(g.value_norm) FROM track_tags g \
                             INDEXED BY track_tags_track \
                             WHERE g.track_id = t.id AND g.key = ",
                        );
                        self.bind(tag.clone());
                        self.push(")");
                        self.push(suffix);
                    }
                    self.push(", ");
                }
                field => {
                    let column = column(field).expect("non-tag field");
                    self.uses(&column);
                    self.push(column.key);
                    self.push(" IS NULL, ");
                    self.push(column.key);
                    self.push(direction);
                    self.push(", ");
                }
            }
        }
        self.push("t.group_artist_norm, t.album_norm, t.disc_no, t.track_no, t.rel_path, t.id");
        Ok(())
    }
}

/// `FROM... WHERE...` for `filter` (joins added as the filter needs them).
pub(super) fn where_clause(sql: &mut Sql, filter: &Filter) -> Result<String> {
    let mut cond = Sql {
        text: String::new(),
        params: Vec::new(),
        stats: sql.stats,
        user: sql.user,
        now: sql.now,
    };
    check_size(filter)?;
    if mentions_available(filter) {
        // A file cut into tracks by a CUE sheet is not a track of its own, available or not.
        cond.push("t.hidden = 0 AND ");
    } else {
        cond.push("t.available = 1 AND ");
    }
    cond.filter(filter, 0)?;
    if cond.params.len() > MAX_PARAMS {
        return Err(invalid(format!(
            "query too large: {} values (at most {MAX_PARAMS})",
            cond.params.len()
        )));
    }
    sql.stats |= cond.stats;
    sql.user |= cond.user;
    sql.params.extend(cond.params);
    Ok(cond.text)
}

/// A query compiled to SQL pieces: `FROM tracks t{joins} WHERE {cond} ORDER BY {order}`.
pub(super) struct Compiled {
    pub(super) cond: String,
    /// Parameters of `cond`.
    pub(super) where_params: Vec<SqlValue>,
    /// Joins `cond` alone needs (for counting).
    pub(super) count_joins: &'static str,
    /// Joins `cond` and `order` need.
    pub(super) joins: &'static str,
    pub(super) order: String,
    order_params: Vec<SqlValue>,
}

impl Compiled {
    pub(super) fn new(query: &TrackQuery) -> Result<Self> {
        let mut sql = Sql::new();
        let cond = where_clause(&mut sql, &query.filter)?;
        let count_joins = sql.joins();
        let mut order = Sql::new();
        order.now = sql.now;
        order.order(&query.sort)?;
        sql.stats |= order.stats;
        sql.user |= order.user;
        Ok(Self {
            cond,
            where_params: sql.params.clone(),
            count_joins,
            joins: sql.joins(),
            order: order.text,
            order_params: order.params,
        })
    }

    pub(super) fn select_sql(&self) -> String {
        format!(
            "SELECT t.id FROM tracks t{} WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
            self.joins, self.cond, self.order
        )
    }

    /// Parameters of the ordered select: condition, then order.
    pub(super) fn select_params(&self) -> Vec<SqlValue> {
        let mut params = self.where_params.clone();
        params.extend(self.order_params.iter().cloned());
        params
    }
}
