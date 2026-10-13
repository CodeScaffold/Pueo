use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, Connection, InterruptHandle, Statement};

use super::{
    log,
    Cell, Choices, ColumnChoices, DbKind, DbObject, Driver, FetchOptions, ObjectKind, ObjectList, QueryResult, RowChange,
    bytes_to_string, change_statements, quote_double, quote_literal, select_tail, where_clause,
};
use crate::config::ConnectionConfig;

/// Find `CHECK (col IN ('a', 'b'))` for `col` in a CREATE TABLE statement.
fn check_in_list(create_sql: &str, col: &str) -> Option<Vec<String>> {
    let lower = create_sql.to_lowercase();
    let col = col.to_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("check") {
        let start = from + i;
        let body = &lower[start..];
        let open = body.find('(')?;
        let after = body[open + 1..].trim_start();
        let after = after.trim_start_matches(['"', '`', '[']);
        if let Some(rest) = after.strip_prefix(col.as_str()) {
            let rest = rest.trim_start_matches(['"', '`', ']']).trim_start();
            if rest.starts_with("in") {
                let list_start = start + open + 1 + (body[open + 1..].len() - rest.len()) + 2;
                let list = &create_sql[list_start..];
                let open_paren = list.find('(')?;
                let close = list[open_paren..].find(')')? + open_paren;
                let values = super::parse_quoted_list(&list[open_paren + 1..close]);
                return (!values.is_empty()).then_some(values);
            }
        }
        from = start + 5;
    }
    None
}

pub struct SqliteDriver {
    conn: Arc<Mutex<Connection>>,
    interrupt: InterruptHandle,
}

fn value_to_cell(v: ValueRef<'_>) -> Cell {
    Some(match v {
        ValueRef::Null => return None,
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => bytes_to_string(b),
    })
}

fn run_statement(conn: &Connection, stmt: &mut Statement<'_>) -> rusqlite::Result<QueryResult> {
    let ncols = stmt.column_count();
    if ncols == 0 {
        stmt.raw_execute()?;
        return Ok(QueryResult { affected: Some(conn.changes()), ..Default::default() });
    }
    let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = Vec::new();
    let mut q = stmt.raw_query();
    while let Some(row) = q.next()? {
        rows.push((0..ncols).map(|i| row.get_ref(i).map(value_to_cell)).collect::<rusqlite::Result<_>>()?);
    }
    Ok(QueryResult { columns, rows, ..Default::default() })
}

impl SqliteDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        if cfg.file_path.is_empty() {
            bail!("No database file selected");
        }
        let path = cfg.file_path.clone();
        let conn = tokio::task::spawn_blocking({
            let path = path.clone();
            move || Connection::open(&path)
        })
        .await?
        .with_context(|| format!("Could not open {path}"))?;
        let interrupt = conn.get_interrupt_handle();
        Ok(SqliteDriver { conn: Arc::new(Mutex::new(conn)), interrupt })
    }

    async fn with_conn<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut conn)
        })
        .await?
    }

    async fn run(&self, sql: String) -> Result<Vec<QueryResult>> {
        let logged = sql.clone();
        let run = self.with_conn(move |conn| {
            let start = Instant::now();
            let mut results = Vec::new();
            let mut batch = Batch::new(conn, &sql);
            while let Some(mut stmt) = batch.next()? {
                let mut r = run_statement(conn, &mut stmt)?;
                r.elapsed = start.elapsed();
                results.push(r);
            }
            Ok(results)
        });
        log::timed(&logged, run).await
    }

    async fn run_one(&self, sql: String) -> Result<QueryResult> {
        Ok(self.run(sql).await?.into_iter().next().unwrap_or_default())
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    async fn server_info(&self) -> Result<String> {
        let r = self.run_one("SELECT sqlite_version()".into()).await?;
        let version = r.rows.first().and_then(|row| row[0].clone()).unwrap_or_default();
        Ok(format!("SQLite {version}"))
    }

    async fn list_objects(&self, _pattern: Option<&str>) -> Result<ObjectList> {
        let r = self
            .run_one(
                "SELECT type, name FROM sqlite_master WHERE type IN ('table', 'view') \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name"
                    .into(),
            )
            .await?;
        let objects = r
            .rows
            .into_iter()
            .map(|row| DbObject {
                schema: "main".into(),
                name: row[1].clone().unwrap_or_default(),
                kind: if row[0].as_deref() == Some("view") { ObjectKind::View } else { ObjectKind::Table },
                signature: String::new(),
            })
            .collect();
        Ok(ObjectList { objects, truncated: false })
    }

    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult> {
        self.run_one(format!("SELECT * FROM {}{}", quote_double(&obj.name), select_tail(opts, quote_double)))
            .await
    }

    async fn count_rows(&self, obj: &DbObject, filter: Option<&str>) -> Result<u64> {
        let r = self
            .run_one(format!("SELECT COUNT(*) FROM {}{}", quote_double(&obj.name), where_clause(filter)))
            .await?;
        Ok(r.rows.first().and_then(|row| row[0].as_deref()).and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    async fn structure(&self, obj: &DbObject) -> Result<QueryResult> {
        self.run_one(format!(
            "SELECT ti.name AS \"column\", ti.type AS \"type\", \
                    CASE ti.\"notnull\" WHEN 1 THEN 'NO' ELSE 'YES' END AS \"nullable\", \
                    ti.dflt_value AS \"default\", CASE WHEN ti.pk > 0 THEN 'PRI' END AS \"key\", \
                    (SELECT group_concat(fk.\"table\" || '(' || coalesce(fk.\"to\", '?') || ')', ', ') \
                     FROM pragma_foreign_key_list({t}) fk WHERE fk.\"from\" = ti.name) AS \"references\" \
             FROM pragma_table_info({t}) ti",
            t = quote_literal(&obj.name)
        ))
        .await
    }

    async fn execute(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.run(sql.to_owned()).await
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_columns(&self) -> Result<Vec<(String, String)>> {
        let r = self
            .run_one(
                "SELECT m.name, p.name FROM sqlite_master m JOIN pragma_table_info(m.name) p \
                 WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, p.cid"
                    .into(),
            )
            .await?;
        Ok(r.rows
            .into_iter()
            .map(|row| (row[0].clone().unwrap_or_default(), row[1].clone().unwrap_or_default()))
            .collect())
    }

    async fn primary_key(&self, obj: &DbObject) -> Result<Option<Vec<String>>> {
        if obj.kind != ObjectKind::Table {
            return Ok(None);
        }
        let r = self
            .run_one(format!(
                "SELECT name FROM pragma_table_info({}) WHERE pk > 0 ORDER BY pk",
                quote_literal(&obj.name)
            ))
            .await?;
        let cols: Vec<String> = r.rows.into_iter().filter_map(|row| row[0].clone()).collect();
        Ok((!cols.is_empty()).then_some(cols))
    }

    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64> {
        // SQLite has a single schema here; qualify with "main".
        let obj = DbObject { schema: "main".into(), ..obj.clone() };
        let statements = change_statements(DbKind::Sqlite, &obj, changes)?;
        // Runs on a blocking thread, so the log sink is handed over explicitly.
        let sink = log::current();
        self.with_conn(move |conn| {
            let start = Instant::now();
            let tx = conn.transaction();
            log::note(&sink, "BEGIN", start, &tx);
            let tx = tx?;
            let mut affected = 0u64;
            for stmt in &statements {
                let start = Instant::now();
                let r = tx.execute(stmt, []);
                log::note(&sink, stmt, start, &r);
                affected += r? as u64;
            }
            let start = Instant::now();
            let r = tx.commit();
            log::note(&sink, "COMMIT", start, &r);
            r?;
            Ok(affected)
        })
        .await
    }

    async fn indexes(&self, obj: &DbObject) -> Result<QueryResult> {
        self.run_one(format!(
            "SELECT il.name AS \"name\", \
                    (SELECT group_concat(ii.name, ', ') FROM pragma_index_info(il.name) ii) AS \"columns\", \
                    CASE WHEN il.\"unique\" THEN 'YES' END AS \"unique\", \
                    CASE WHEN il.origin = 'pk' THEN 'YES' END AS \"primary\", \
                    CASE il.origin WHEN 'c' THEN 'index' WHEN 'u' THEN 'unique constraint' ELSE 'primary key' END AS \"type\", \
                    m.sql AS \"definition\" \
             FROM pragma_index_list({t}) il LEFT JOIN sqlite_master m ON m.type = 'index' AND m.name = il.name \
             ORDER BY il.origin = 'pk' DESC, il.name",
            t = quote_literal(&obj.name)
        ))
        .await
    }

    async fn column_choices(&self, obj: &DbObject) -> Result<Choices> {
        let r = self
            .run_one(format!(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = {}",
                quote_literal(&obj.name)
            ))
            .await?;
        let Some(sql) = r.rows.first().and_then(|row| row[0].clone()) else { return Ok(Choices::new()) };
        let columns = self
            .run_one(format!("SELECT name, \"notnull\" FROM pragma_table_info({})", quote_literal(&obj.name)))
            .await?;
        let mut choices = Choices::new();
        for row in columns.rows {
            let (Some(col), notnull) = (row[0].clone(), row[1].clone()) else { continue };
            if let Some(values) = check_in_list(&sql, &col) {
                choices.insert(col, ColumnChoices { values, nullable: notnull.as_deref() != Some("1") });
            }
        }
        Ok(choices)
    }

    async fn cancel(&self) -> Result<()> {
        self.interrupt.interrupt();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::check_in_list;

    #[test]
    fn finds_check_in() {
        let sql = "CREATE TABLE t(id INTEGER PRIMARY KEY, source TEXT NOT NULL CHECK (source IN ('web', 'app')), \"kind\" TEXT, CHECK(\"kind\" in ('a','b')))";
        assert_eq!(check_in_list(sql, "source"), Some(vec!["web".into(), "app".into()]));
        assert_eq!(check_in_list(sql, "kind"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(check_in_list(sql, "id"), None);
    }
}
