use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, Row, SslOpts, Value};
use tokio::sync::Mutex;

use super::{
    log,
    Cell, Choices, ColumnChoices, DbKind, DbObject, Driver, FetchOptions, ObjectKind, ObjectList, QueryResult, RowChange,
    bytes_to_string, change_statements, quote_backtick, quote_value, select_tail, where_clause,
};
use crate::config::ConnectionConfig;

/// A single session so that `USE db`, variables and transactions behave like a terminal client.
/// A string literal with MySQL's backslash escaping.
fn lit(s: &str) -> String {
    quote_value(DbKind::Mysql, &Some(s.to_string()))
}

/// `enum('a','it''s')` → ["a", "it's"].
pub(crate) fn parse_enum_list(column_type: &str) -> Vec<String> {
    let inner = column_type
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(inner, _)| inner)
        .unwrap_or("");
    super::parse_quoted_list(inner)
}

pub struct MyDriver {
    conn: Mutex<Conn>,
    /// Used to open a second connection for `KILL QUERY`.
    opts: Opts,
    connection_id: u32,
}

fn value_to_cell(v: Value) -> Cell {
    Some(match v {
        Value::NULL => return None,
        Value::Bytes(b) => bytes_to_string(&b),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Date(y, mo, d, h, mi, s, us) => {
            if us > 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
            }
        }
        Value::Time(neg, days, h, mi, s, _us) => {
            let hours = days * 24 + h as u32;
            format!("{}{hours:02}:{mi:02}:{s:02}", if neg { "-" } else { "" })
        }
    })
}

impl MyDriver {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<String>) -> Result<Self> {
        let mut opts = OptsBuilder::default()
            .ip_or_hostname(if cfg.host.is_empty() { "localhost".to_string() } else { cfg.host.clone() })
            .tcp_port(cfg.port)
            .user(Some(if cfg.user.is_empty() { "root".to_string() } else { cfg.user.clone() }))
            .pass(password.filter(|p| !p.is_empty()))
            .db_name((!cfg.database.is_empty()).then(|| cfg.database.clone()));
        if cfg.use_tls {
            opts = opts.ssl_opts(
                SslOpts::default()
                    .with_danger_accept_invalid_certs(true)
                    .with_danger_skip_domain_validation(true),
            );
        }
        let opts = Opts::from(opts);
        let conn = Conn::new(opts.clone()).await.context("Could not connect to MySQL")?;
        let connection_id = conn.id();
        Ok(MyDriver { conn: Mutex::new(conn), opts, connection_id })
    }

    fn qualified(obj: &DbObject) -> String {
        format!("{}.{}", quote_backtick(&obj.schema), quote_backtick(&obj.name))
    }

    async fn run(&self, sql: &str) -> Result<Vec<QueryResult>> {
        let mut conn = self.conn.lock().await;
        log::timed(sql, Self::collect(&mut conn, sql)).await
    }

    /// Run `sql` and read every result set it produces.
    async fn collect(conn: &mut Conn, sql: &str) -> Result<Vec<QueryResult>> {
        let start = Instant::now();
        let mut res = conn.query_iter(sql).await?;
        let mut results = Vec::new();
        loop {
            let columns: Vec<String> = res
                .columns()
                .map(|cols| cols.iter().map(|c| c.name_str().into_owned()).collect())
                .unwrap_or_default();
            let rows: Vec<Row> = res.collect().await?;
            let affected = res.affected_rows();
            results.push(QueryResult {
                affected: columns.is_empty().then_some(affected),
                rows: rows
                    .into_iter()
                    .map(|r| r.unwrap().into_iter().map(value_to_cell).collect())
                    .collect(),
                columns,
                elapsed: start.elapsed(),
            });
            if res.is_empty() {
                break;
            }
        }
        Ok(results)
    }

    async fn run_one(&self, sql: &str) -> Result<QueryResult> {
        Ok(self.run(sql).await?.into_iter().next().unwrap_or_default())
    }

    fn first_column(r: QueryResult) -> Vec<String> {
        r.rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect()
    }
}

#[async_trait]
impl Driver for MyDriver {
    fn kind(&self) -> DbKind {
        DbKind::Mysql
    }

    async fn server_info(&self) -> Result<String> {
        let r = self.run_one("SELECT DATABASE(), VERSION()").await?;
        let row = r.rows.first().cloned().unwrap_or_default();
        Ok(format!(
            "{} · MySQL {}",
            row.first().cloned().flatten().unwrap_or_else(|| "(no database)".into()),
            row.get(1).cloned().flatten().unwrap_or_default()
        ))
    }

    async fn list_objects(&self, _pattern: Option<&str>) -> Result<ObjectList> {
        let r = self
            .run_one(
                "SELECT table_schema, table_name, table_type FROM information_schema.tables \
                 WHERE (DATABASE() IS NULL AND table_schema NOT IN \
                        ('mysql', 'information_schema', 'performance_schema', 'sys')) \
                    OR table_schema = DATABASE() \
                 UNION ALL \
                 SELECT routine_schema, routine_name, routine_type FROM information_schema.routines \
                 WHERE (DATABASE() IS NULL AND routine_schema NOT IN \
                        ('mysql', 'information_schema', 'performance_schema', 'sys')) \
                    OR routine_schema = DATABASE() \
                 ORDER BY 1, 2",
            )
            .await?;
        let objects = r
            .rows
            .into_iter()
            .map(|row| DbObject {
                schema: row[0].clone().unwrap_or_default(),
                name: row[1].clone().unwrap_or_default(),
                kind: match row[2].as_deref() {
                    Some("VIEW" | "SYSTEM VIEW") => ObjectKind::View,
                    Some("FUNCTION") => ObjectKind::Function,
                    Some("PROCEDURE") => ObjectKind::Procedure,
                    _ => ObjectKind::Table,
                },
                signature: String::new(),
            })
            .collect();
        Ok(ObjectList { objects, truncated: false })
    }

    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult> {
        let sql = format!("SELECT * FROM {}{}", Self::qualified(obj), select_tail(opts, quote_backtick));
        self.run_one(&sql).await
    }

    async fn count_rows(&self, obj: &DbObject, filter: Option<&str>) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {}{}", Self::qualified(obj), where_clause(filter));
        let r = self.run_one(&sql).await?;
        Ok(r.rows
            .first()
            .and_then(|row| row[0].as_deref())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    async fn structure(&self, obj: &DbObject) -> Result<QueryResult> {
        let sql = format!(
            "SELECT c.column_name AS `column`, c.column_type AS `type`, c.is_nullable AS `nullable`, \
                    c.column_default AS `default`, c.column_key AS `key`, \
                    (SELECT GROUP_CONCAT(CONCAT(k.referenced_table_schema, '.', k.referenced_table_name, \
                                                '(', k.referenced_column_name, ')') SEPARATOR ', ') \
                     FROM information_schema.key_column_usage k \
                     WHERE k.table_schema = c.table_schema AND k.table_name = c.table_name \
                       AND k.column_name = c.column_name AND k.referenced_table_name IS NOT NULL) AS `references`, \
                    c.extra AS `extra`, c.column_comment AS `comment` \
             FROM information_schema.columns c WHERE c.table_schema = {} AND c.table_name = {} \
             ORDER BY c.ordinal_position",
            lit(&obj.schema),
            lit(&obj.name)
        );
        self.run_one(&sql).await
    }

    async fn execute(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.run(sql).await
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let r = self
            .run_one(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('information_schema', 'performance_schema', 'sys') \
                 ORDER BY 1",
            )
            .await?;
        Ok(Self::first_column(r))
    }

    async fn list_columns(&self) -> Result<Vec<(String, String)>> {
        let r = self
            .run_one(
                "SELECT table_name, column_name FROM information_schema.columns \
                 WHERE (DATABASE() IS NULL AND table_schema NOT IN \
                        ('mysql', 'information_schema', 'performance_schema', 'sys')) \
                    OR table_schema = DATABASE() \
                 ORDER BY table_name, ordinal_position",
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
            .run_one(&format!(
                "SELECT column_name FROM information_schema.key_column_usage \
                 WHERE table_schema = {} AND table_name = {} AND constraint_name = 'PRIMARY' \
                 ORDER BY ordinal_position",
                lit(&obj.schema),
                lit(&obj.name)
            ))
            .await?;
        let cols = Self::first_column(r);
        Ok((!cols.is_empty()).then_some(cols))
    }

    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64> {
        let statements = change_statements(DbKind::Mysql, obj, changes)?;
        let mut conn = self.conn.lock().await;
        log::timed("START TRANSACTION", conn.query_drop("START TRANSACTION")).await?;
        let mut affected = 0;
        for stmt in &statements {
            let run = async {
                let res = conn.query_iter(stmt.as_str()).await?;
                let n = res.affected_rows();
                res.drop_result().await?;
                Ok::<_, mysql_async::Error>(n)
            };
            match log::timed(stmt, run).await {
                Ok(n) => affected += n,
                Err(e) => {
                    let _ = log::timed("ROLLBACK", conn.query_drop("ROLLBACK")).await;
                    return Err(e.into());
                }
            }
        }
        log::timed("COMMIT", conn.query_drop("COMMIT")).await?;
        Ok(affected)
    }

    async fn indexes(&self, obj: &DbObject) -> Result<QueryResult> {
        self.run_one(&format!(
            "SELECT index_name AS `name`, \
                    GROUP_CONCAT(column_name ORDER BY seq_in_index SEPARATOR ', ') AS `columns`, \
                    CASE WHEN MIN(non_unique) = 0 THEN 'YES' END AS `unique`, \
                    CASE WHEN index_name = 'PRIMARY' THEN 'YES' END AS `primary`, \
                    MIN(index_type) AS `type` \
             FROM information_schema.statistics WHERE table_schema = {} AND table_name = {} \
             GROUP BY index_name ORDER BY index_name = 'PRIMARY' DESC, index_name",
            lit(&obj.schema),
            lit(&obj.name)
        ))
        .await
    }

    async fn definition(&self, obj: &DbObject) -> Result<String> {
        let what = if obj.kind == ObjectKind::Procedure { "PROCEDURE" } else { "FUNCTION" };
        let r = self.run_one(&format!("SHOW CREATE {what} {}", Self::qualified(obj))).await?;
        // The statement column is NULL without the privilege to read it.
        r.rows
            .into_iter()
            .next()
            .and_then(|row| row.get(2).cloned().flatten())
            .context("The definition is not visible to this user")
    }

    async fn column_choices(&self, obj: &DbObject) -> Result<Choices> {
        let r = self
            .run_one(&format!(
                "SELECT column_name, is_nullable, column_type FROM information_schema.columns \
                 WHERE table_schema = {} AND table_name = {} \
                   AND (data_type = 'enum' OR column_type = 'tinyint(1)')",
                lit(&obj.schema),
                lit(&obj.name)
            ))
            .await?;
        let mut choices = Choices::new();
        for row in r.rows {
            let (Some(col), Some(nullable), Some(ty)) = (row[0].clone(), row[1].clone(), row[2].clone()) else {
                continue;
            };
            let values = if ty == "tinyint(1)" { vec!["0".into(), "1".into()] } else { parse_enum_list(&ty) };
            choices.insert(col, ColumnChoices { values, nullable: nullable == "YES" });
        }
        Ok(choices)
    }

    async fn cancel(&self) -> Result<()> {
        let mut killer = Conn::new(self.opts.clone()).await?;
        killer.query_drop(format!("KILL QUERY {}", self.connection_id)).await?;
        killer.disconnect().await?;
        Ok(())
    }
}
