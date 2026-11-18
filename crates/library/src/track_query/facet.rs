use super::*;

/// The values of `field` among the tracks matching `filter`, with track counts, ordered by
/// value with the unknown group last: `(values, total number of values)`.
pub(crate) fn facet(
    conn: &Connection,
    field: &Field,
    filter: &Filter,
    page: Page,
) -> Result<(Vec<FacetValue>, u64)> {
    let mut sql = Sql::new();
    let (text, kind) = match field {
        Field::Tag { key } => {
            let key = tag_key(key)?;
            // The filter appears twice (values, then the unknown group): bind it twice.
            sql.params.push(key.clone().into());
            let cond = where_clause(&mut sql, filter)?;
            let cond_params = sql.params[1..].to_vec();
            let joins = sql.joins();
            sql.params.extend(cond_params);
            sql.params.push(key.into());
            (
                format!(
                    "SELECT k, d, n, COUNT(*) OVER () FROM (
                        SELECT g.value_norm AS k, MIN(g.value) AS d,
                               COUNT(DISTINCT g.track_id) AS n
                        FROM track_tags g JOIN tracks t ON t.id = g.track_id{joins}
                        WHERE g.key = ? AND {cond}
                        GROUP BY g.value_norm
                        UNION ALL
                        SELECT NULL, NULL, COUNT(*) FROM tracks t{joins}
                        WHERE {cond} AND NOT EXISTS (
                            SELECT 1 FROM track_tags g INDEXED BY track_tags_track
                            WHERE g.track_id = t.id AND g.key = ?)
                        HAVING COUNT(*) > 0)"
                ),
                Kind::Text,
            )
        }
        _ => {
            let column = column(field).expect("non-tag field");
            sql.uses(&column);
            let cond = where_clause(&mut sql, filter)?;
            let group = if column.kind == Kind::Text {
                format!(
                    "CASE WHEN {} THEN NULL ELSE {} END",
                    column.missing, column.key
                )
            } else {
                column.value.to_owned()
            };
            (
                format!(
                    "SELECT k, d, n, COUNT(*) OVER () FROM (
                        SELECT {group} AS k, MIN({}) AS d, COUNT(*) AS n
                        FROM tracks t{} WHERE {cond} GROUP BY k)",
                    column.value,
                    sql.joins()
                ),
                column.kind,
            )
        }
    };
    let base_params = sql.params.clone();
    let mut params = sql.params;
    params.push(SqlValue::Integer(i64::from(page.limit)));
    params.push(SqlValue::Integer(i64::from(page.offset)));
    let paged = format!("{text} ORDER BY k IS NULL, k LIMIT ? OFFSET ?");
    let mut values = Vec::new();
    let mut total = 0u64;
    {
        let mut stmt = conn.prepare(&paged)?;
        let mut rows = stmt.query(params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            let key: SqlValue = row.get(0)?;
            let display: SqlValue = row.get(1)?;
            let value = match (kind, &key) {
                (_, SqlValue::Null) => None,
                (Kind::Text, _) => match display {
                    SqlValue::Text(text) => Some(Value::Text(text)),
                    _ => None,
                },
                (Kind::Int, SqlValue::Integer(n)) => Some(Value::Int(*n)),
                (Kind::Bool, SqlValue::Integer(n)) => Some(Value::Bool(*n != 0)),
                (Kind::Timestamp, SqlValue::Integer(n)) => Some(Value::Timestamp(*n)),
                _ => None,
            };
            values.push(FacetValue {
                value,
                tracks: row.get::<_, i64>(2)? as u64,
            });
            total = row.get::<_, i64>(3)? as u64;
        }
    }
    if values.is_empty() {
        total = conn
            .prepare(&format!("SELECT COUNT(*) FROM ({text})"))?
            .query_row(params_from_iter(base_params.iter()), |row| {
                row.get::<_, i64>(0)
            })? as u64;
    }
    Ok((values, total))
}
