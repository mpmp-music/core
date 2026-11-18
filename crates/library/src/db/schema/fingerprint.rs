//! Exact DDL and streaming, typed logical data evidence. No page hashes or sampling.
use crate::Result;
use rusqlite::{Connection, types::ValueRef};
use sha2::{Digest, Sha256};

pub(super) fn schema(conn: &Connection) -> Result<String> {
    let mut hash = Sha256::new();
    let mut stmt = conn.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let mut rows = stmt.query([])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            hash.update(b"\n");
        }
        first = false;
        for column in 0..4 {
            if column != 0 {
                hash.update(b"|");
            }
            hash.update(row.get::<_, String>(column)?.as_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn bytes(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn value(hash: &mut Sha256, v: ValueRef<'_>) {
    match v {
        ValueRef::Null => hash.update([0]),
        ValueRef::Integer(n) => {
            hash.update([1]);
            hash.update(n.to_be_bytes());
        }
        ValueRef::Real(n) => {
            hash.update([2]);
            hash.update(n.to_bits().to_be_bytes());
        }
        ValueRef::Text(s) => {
            hash.update([3]);
            bytes(hash, s);
        }
        ValueRef::Blob(b) => {
            hash.update([4]);
            bytes(hash, b);
        }
    }
}

/// Both streams include duplicates and order. The second also includes implicit rowids:
/// VACUUM may renumber them, but must preserve their relative order and all values.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Data {
    pub content: String,
    pub rowids: String,
}

pub(super) fn data(conn: &Connection) -> Result<Data> {
    let tables = conn
        .prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut content = Sha256::new();
    let mut rowids = Sha256::new();
    for (name, sql) in tables {
        bytes(&mut content, name.as_bytes());
        bytes(&mut rowids, name.as_bytes());
        let quoted = quote(&name);
        let without_rowid = sql.to_uppercase().contains("WITHOUT ROWID");
        let query = if without_rowid {
            let columns = conn
                .prepare(&format!("PRAGMA table_info({quoted})"))?
                .query_map([], |r| Ok((r.get::<_, i64>(5)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut keys: Vec<_> = columns.into_iter().filter(|(pk, _)| *pk > 0).collect();
            keys.sort();
            let order = keys
                .iter()
                .map(|(_, n)| quote(n))
                .collect::<Vec<_>>()
                .join(",");
            format!("SELECT * FROM {quoted} ORDER BY {order}")
        } else {
            format!("SELECT rowid,* FROM {quoted} ORDER BY rowid")
        };
        let mut stmt = conn.prepare(&query)?;
        let columns = stmt.column_count();
        let mut rows = stmt.query([])?;
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            content.update([0xfe]);
            rowids.update([0xfe]);
            for col in 0..columns {
                let v = row.get_ref(col)?;
                value(&mut rowids, v);
                if without_rowid || col != 0 {
                    value(&mut content, v);
                }
            }
            count += 1;
        }
        content.update(count.to_be_bytes());
        rowids.update(count.to_be_bytes());
    }
    Ok(Data {
        content: format!("{:x}", content.finalize()),
        rowids: format!("{:x}", rowids.finalize()),
    })
}

pub(super) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
