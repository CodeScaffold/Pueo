//! Export results to CSV / JSON / SQL and import CSV files.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::db::{Cell, DbKind, RowChange, insert_sql, quote_ident};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    Sql,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Sql => "sql",
        }
    }

    pub fn from_name(name: &str) -> Option<Format> {
        match name {
            "csv" => Some(Format::Csv),
            "json" => Some(Format::Json),
            "sql" => Some(Format::Sql),
            _ => None,
        }
    }
}

/// Write rows to `path`. `table` is used as the target of SQL INSERT statements.
pub fn write(
    path: &Path,
    format: Format,
    kind: DbKind,
    table: &str,
    columns: &[String],
    rows: &[Vec<Cell>],
) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("Could not create {}", path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    match format {
        Format::Csv => {
            let mut w = csv::Writer::from_writer(out);
            w.write_record(columns)?;
            for row in rows {
                // NULL is written as an empty field.
                w.write_record(row.iter().map(|c| c.as_deref().unwrap_or("")))?;
            }
            w.flush()?;
        }
        Format::Json => {
            let items: Vec<serde_json::Map<String, serde_json::Value>> = rows
                .iter()
                .map(|row| {
                    columns
                        .iter()
                        .zip(row)
                        .map(|(c, v)| {
                            (c.clone(), v.as_ref().map_or(serde_json::Value::Null, |s| serde_json::Value::String(s.clone())))
                        })
                        .collect()
                })
                .collect();
            serde_json::to_writer_pretty(&mut out, &items)?;
            writeln!(out)?;
        }
        Format::Sql => {
            let table = table
                .split('.')
                .map(|part| quote_ident(kind, part))
                .collect::<Vec<_>>()
                .join(".");
            for row in rows {
                let values: Vec<(String, Cell)> = columns.iter().cloned().zip(row.iter().cloned()).collect();
                writeln!(out, "{};", insert_sql(kind, &table, &values))?;
            }
        }
    }
    Ok(())
}

/// Read a CSV file with a header row into INSERT changes.
/// Empty fields become NULL. Every header must name one of `table_columns`.
pub fn read_csv(path: &Path, table_columns: &[String]) -> Result<Vec<RowChange>> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_path(path)
        .with_context(|| format!("Could not open {}", path.display()))?;
    let headers: Vec<String> = reader.headers()?.iter().map(|h| h.trim().to_string()).collect();
    for h in &headers {
        if !table_columns.iter().any(|c| c == h) {
            bail!("Column '{h}' in the CSV header does not exist in the table");
        }
    }
    let mut changes = Vec::new();
    for (i, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("CSV line {}", i + 2))?;
        let values = headers
            .iter()
            .zip(record.iter())
            .map(|(h, v)| (h.clone(), (!v.is_empty()).then(|| v.to_string())))
            .collect();
        changes.push(RowChange::Insert { values });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_csv() {
        let dir = std::env::temp_dir().join(format!("pueo-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.csv");
        let cols = vec!["id".to_string(), "note".to_string()];
        let rows = vec![vec![Some("1".into()), Some("a,\"b\"\nc".into())], vec![Some("2".into()), None]];
        write(&path, Format::Csv, DbKind::Postgres, "t", &cols, &rows).unwrap();
        let changes = read_csv(&path, &cols).unwrap();
        assert_eq!(
            changes[0],
            RowChange::Insert { values: vec![("id".into(), Some("1".into())), ("note".into(), Some("a,\"b\"\nc".into()))] }
        );
        assert_eq!(changes[1], RowChange::Insert { values: vec![("id".into(), Some("2".into())), ("note".into(), None)] });

        write(&path, Format::Sql, DbKind::Mysql, "shop.t", &cols, &rows).unwrap();
        let sql = std::fs::read_to_string(&path).unwrap();
        assert!(sql.starts_with("INSERT INTO `shop`.`t` (`id`, `note`) VALUES ('1', 'a,\"b\"\nc');"));
        assert!(sql.contains("VALUES ('2', NULL);"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
