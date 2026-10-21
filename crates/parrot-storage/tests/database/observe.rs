//! Test-only structural observer; matches the separately captured .NET observer.
use super::{Connection, DatabaseError};
use rusqlite::types::ValueRef;
use serde_json::{Map, Value, json};
use std::path::Path;

fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        result.push(DIGITS[(a >> 2) as usize] as char);
        result.push(DIGITS[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            DIGITS[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            DIGITS[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn query(connection: &Connection, sql: &str) -> Result<Vec<Value>, DatabaseError> {
    let mut statement = connection.prepare(sql)?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Map::new();
        for (index, name) in columns.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
                ValueRef::Blob(value) => json!({"base64": base64(value)}),
            };
            values.insert(name.clone(), value);
        }
        result.push(Value::Object(values));
    }
    Ok(result)
}

pub fn sidecars(path: &Path) -> Value {
    json!({"wal_exists": path.with_extension("db-wal").exists(), "shm_exists": path.with_extension("db-shm").exists()})
}

pub fn inspect(connection: &Connection, path: &Path) -> Result<Value, DatabaseError> {
    let schema = query(
        connection,
        "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name;",
    )?;
    let mut tables = Map::new();
    for row in schema.iter().filter(|row| row["type"] == "table") {
        let name = row["name"].as_str().expect("schema table name");
        let indices = query(connection, &format!("PRAGMA index_list({});", quote(name)))?;
        let mut index_columns = Map::new();
        for index in &indices {
            let index = index["name"].as_str().expect("schema index name");
            index_columns.insert(
                index.into(),
                Value::Array(query(
                    connection,
                    &format!("PRAGMA index_xinfo({});", quote(index)),
                )?),
            );
        }
        tables.insert(name.into(), json!({
            "columns": query(connection, &format!("PRAGMA table_info({});", quote(name)))?,
            "foreign_keys": query(connection, &format!("PRAGMA foreign_key_list({});", quote(name)))?,
            "indices": indices, "index_columns": index_columns,
            "rows": query(connection, &format!("SELECT * FROM {} ORDER BY rowid;", quote(name)))?
        }));
    }
    Ok(json!({"schema": schema, "tables": tables,
        "journal_mode":query(connection,"PRAGMA journal_mode;")?,
        "foreign_keys":query(connection,"PRAGMA foreign_keys;")?,
        "foreign_key_check":query(connection,"PRAGMA foreign_key_check;")?,
        "integrity_check":query(connection,"PRAGMA integrity_check;")?,
        "sidecars":sidecars(path)
    }))
}

/// Observe the raw fixture connection's actual native defaults, just as the
/// independent .NET observer does. Production SessionDatabase explicitly enables FK.
pub fn inspect_raw(path: &Path) -> Result<Value, DatabaseError> {
    let connection = Connection::open(path)?;
    inspect(&connection, path)
}
