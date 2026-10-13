use std::time::Instant;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use redis::aio::MultiplexedConnection;
use redis::{Arg, Cmd, FromRedisValue, Pipeline, RedisResult, Value, cmd};

use super::{
    log,
    Cell, DbKind, DbObject, Driver, FetchOptions, ObjectKind, ObjectList, QueryResult, RowChange,
    bytes_to_string,
};
use crate::config::ConnectionConfig;

/// Maximum number of keys listed in the sidebar.
const MAX_KEYS: usize = 50_000;

/// Placeholder used to delete list elements by index (LSET + LREM).
const LIST_TOMBSTONE: &str = "__pueo_deleted__";

/// Sends a command or pipeline and records it in the query log.
trait Logged {
    fn text(&self) -> String;
    fn logged<T: FromRedisValue + Send>(
        &self,
        c: &mut MultiplexedConnection,
    ) -> impl Future<Output = RedisResult<T>> + Send;
}

/// A command as it would be typed in redis-cli.
fn command_text(command: &Cmd) -> String {
    command
        .args_iter()
        .map(|arg| match arg {
            Arg::Simple(bytes) => {
                let s = String::from_utf8_lossy(bytes);
                if s.is_empty() || s.contains(char::is_whitespace) || s.contains('"') {
                    format!("{s:?}")
                } else {
                    s.into_owned()
                }
            }
            _ => "0".into(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Logged for Cmd {
    fn text(&self) -> String {
        command_text(self)
    }
    async fn logged<T: FromRedisValue + Send>(&self, c: &mut MultiplexedConnection) -> RedisResult<T> {
        log::timed(&self.text(), self.query_async(c)).await
    }
}

impl Logged for Pipeline {
    fn text(&self) -> String {
        self.cmd_iter().map(command_text).collect::<Vec<_>>().join("; ")
    }
    async fn logged<T: FromRedisValue + Send>(&self, c: &mut MultiplexedConnection) -> RedisResult<T> {
        log::timed(&self.text(), self.query_async(c)).await
    }
}

pub struct RedisDriver {
    conn: MultiplexedConnection,
    db: String,
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Nil => "(nil)".into(),
        Value::Int(i) => i.to_string(),
        Value::BulkString(b) => bytes_to_string(b),
        Value::SimpleString(s) => s.clone(),
        Value::Okay => "OK".into(),
        Value::Double(d) => d.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::VerbatimString { text, .. } => text.clone(),
        Value::Array(items) | Value::Set(items) => {
            format!("[{}]", items.iter().map(value_to_string).collect::<Vec<_>>().join(", "))
        }
        Value::Map(pairs) => format!(
            "{{{}}}",
            pairs
                .iter()
                .map(|(k, v)| format!("{}: {}", value_to_string(k), value_to_string(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Attribute { data, .. } => value_to_string(data),
        other => format!("{other:?}"),
    }
}

fn value_to_cell(v: &Value) -> Cell {
    match v {
        Value::Nil => None,
        other => Some(value_to_string(other)),
    }
}

/// Turn a reply into a grid.
fn reply_to_result(v: Value) -> QueryResult {
    match v {
        Value::Array(items) | Value::Set(items) => QueryResult::from_rows(
            &["#", "value"],
            items
                .iter()
                .enumerate()
                .map(|(i, v)| vec![Some((i + 1).to_string()), value_to_cell(v)])
                .collect(),
        ),
        Value::Map(pairs) => QueryResult::from_rows(
            &["key", "value"],
            pairs.iter().map(|(k, v)| vec![value_to_cell(k), value_to_cell(v)]).collect(),
        ),
        Value::Attribute { data, .. } => reply_to_result(*data),
        other => QueryResult::from_rows(&["result"], vec![vec![value_to_cell(&other)]]),
    }
}

/// Split a command line into arguments, honouring single and double quotes.
fn tokenize(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_token = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_token {
                    args.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        args.push(cur);
    }
    args
}

impl RedisDriver {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<String>) -> Result<Self> {
        let db = if cfg.database.trim().is_empty() { "0".to_string() } else { cfg.database.trim().to_string() };
        if db.parse::<u32>().is_err() {
            bail!("Redis database must be a number, got '{db}'");
        }
        let mut auth = String::new();
        let pw = password.unwrap_or_default();
        if !cfg.user.is_empty() || !pw.is_empty() {
            auth = format!("{}:{}@", percent_encode(&cfg.user), percent_encode(&pw));
        }
        let host = if cfg.host.is_empty() { "localhost" } else { &cfg.host };
        let scheme = if cfg.use_tls { "rediss" } else { "redis" };
        let suffix = if cfg.use_tls { "#insecure" } else { "" };
        let url = format!("{scheme}://{auth}{host}:{}/{db}{suffix}", cfg.port);

        let client = redis::Client::open(url).context("Invalid Redis connection settings")?;
        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.get_multiplexed_async_connection(),
        )
        .await
        .context("Timed out connecting to Redis")?
        .context("Could not connect to Redis")?;
        Ok(RedisDriver { conn, db })
    }

    async fn key_type(&self, key: &str) -> Result<String> {
        let mut c = self.conn.clone();
        Ok(cmd("TYPE").arg(key).logged(&mut c).await?)
    }
}

fn field<'a>(pairs: &'a [(String, Cell)], name: &str) -> Option<&'a str> {
    pairs.iter().find(|(c, _)| c == name).and_then(|(_, v)| v.as_deref())
}

fn required<'a>(pairs: &'a [(String, Cell)], name: &str) -> Result<&'a str> {
    field(pairs, name).with_context(|| format!("'{name}' cannot be NULL in Redis"))
}

#[async_trait]
impl Driver for RedisDriver {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }

    async fn server_info(&self) -> Result<String> {
        let mut c = self.conn.clone();
        let info: String = cmd("INFO").arg("server").logged(&mut c).await?;
        let version = info
            .lines()
            .find_map(|l| l.strip_prefix("redis_version:"))
            .unwrap_or("?")
            .trim()
            .to_string();
        Ok(format!("db{} · Redis {version}", self.db))
    }

    async fn list_objects(&self, pattern: Option<&str>) -> Result<ObjectList> {
        let mut c = self.conn.clone();
        let pattern = match pattern.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) if p.contains(['*', '?', '[']) => p.to_string(),
            Some(p) => format!("*{p}*"),
            None => "*".to_string(),
        };
        let mut keys: Vec<String> = Vec::new();
        let mut cursor: u64 = 0;
        let truncated = loop {
            let (next, batch): (u64, Vec<String>) = cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(1000)
                .logged(&mut c)
                .await?;
            keys.extend(batch);
            cursor = next;
            if cursor == 0 {
                break false;
            }
            if keys.len() >= MAX_KEYS {
                break true;
            }
        };
        keys.truncate(MAX_KEYS);
        keys.sort();
        keys.dedup();

        let mut types: Vec<String> = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(5000) {
            let mut pipe = redis::pipe();
            for k in chunk {
                pipe.cmd("TYPE").arg(k);
            }
            let t: Vec<String> = pipe.logged(&mut c).await?;
            types.extend(t);
        }

        let mut objects: Vec<DbObject> = keys
            .into_iter()
            .zip(types)
            .map(|(name, t)| DbObject { schema: t, name, kind: ObjectKind::Key, signature: String::new() })
            .collect();
        objects.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.name.cmp(&b.name)));
        Ok(ObjectList { objects, truncated })
    }

    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult> {
        let start = Instant::now();
        let mut c = self.conn.clone();
        let key = obj.name.as_str();
        let (off, lim) = (opts.offset, opts.limit);
        let stop = (off + lim).saturating_sub(1);
        let t = self.key_type(key).await?;
        let mut r = match t.as_str() {
            "string" => {
                let v: Value = cmd("GET").arg(key).logged(&mut c).await?;
                QueryResult::from_rows(&["value"], vec![vec![value_to_cell(&v)]])
            }
            "list" => {
                let items: Vec<Value> = cmd("LRANGE").arg(key).arg(off).arg(stop).logged(&mut c).await?;
                QueryResult::from_rows(
                    &["index", "value"],
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, v)| vec![Some((off + i).to_string()), value_to_cell(v)])
                        .collect(),
                )
            }
            "hash" => {
                let mut pairs: Vec<(String, Value)> = cmd("HGETALL").arg(key).logged(&mut c).await?;
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                QueryResult::from_rows(
                    &["field", "value"],
                    pairs
                        .iter()
                        .skip(off)
                        .take(lim)
                        .map(|(k, v)| vec![Some(k.clone()), value_to_cell(v)])
                        .collect(),
                )
            }
            "set" => {
                let mut members: Vec<Value> = cmd("SMEMBERS").arg(key).logged(&mut c).await?;
                members.sort_by_key(value_to_string);
                QueryResult::from_rows(
                    &["member"],
                    members.iter().skip(off).take(lim).map(|v| vec![value_to_cell(v)]).collect(),
                )
            }
            "zset" => {
                let items: Vec<(Value, String)> = cmd("ZRANGE")
                    .arg(key)
                    .arg(off)
                    .arg(stop)
                    .arg("WITHSCORES")
                    .logged(&mut c)
                    .await?;
                QueryResult::from_rows(
                    &["member", "score"],
                    items.iter().map(|(m, s)| vec![value_to_cell(m), Some(s.clone())]).collect(),
                )
            }
            "stream" => {
                let entries: Vec<(String, Vec<(String, Value)>)> = cmd("XRANGE")
                    .arg(key)
                    .arg("-")
                    .arg("+")
                    .arg("COUNT")
                    .arg(off + lim)
                    .logged(&mut c)
                    .await?;
                QueryResult::from_rows(
                    &["id", "fields"],
                    entries
                        .iter()
                        .skip(off)
                        .map(|(id, fields)| {
                            let f = fields
                                .iter()
                                .map(|(k, v)| format!("{k}={}", value_to_string(v)))
                                .collect::<Vec<_>>()
                                .join("  ");
                            vec![Some(id.clone()), Some(f)]
                        })
                        .collect(),
                )
            }
            "none" => bail!("Key '{key}' no longer exists"),
            other => QueryResult::from_rows(
                &["info"],
                vec![vec![Some(format!("Viewing keys of type '{other}' is not supported yet"))]],
            ),
        };
        r.elapsed = start.elapsed();
        Ok(r)
    }

    async fn count_rows(&self, obj: &DbObject, _filter: Option<&str>) -> Result<u64> {
        let mut c = self.conn.clone();
        let command = match self.key_type(&obj.name).await?.as_str() {
            "list" => "LLEN",
            "hash" => "HLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            "stream" => "XLEN",
            "none" => return Ok(0),
            _ => return Ok(1),
        };
        Ok(cmd(command).arg(&obj.name).logged(&mut c).await?)
    }

    async fn structure(&self, obj: &DbObject) -> Result<QueryResult> {
        let mut c = self.conn.clone();
        let key = obj.name.as_str();
        let t = self.key_type(key).await?;
        let ttl: i64 = cmd("TTL").arg(key).logged(&mut c).await?;
        let encoding: Option<String> = cmd("OBJECT").arg("ENCODING").arg(key).logged(&mut c).await.ok();
        let memory: Option<i64> = cmd("MEMORY").arg("USAGE").arg(key).logged(&mut c).await.ok();
        let len = self.count_rows(obj, None).await.ok();
        let ttl = match ttl {
            -1 => "no expiry".to_string(),
            -2 => "key does not exist".to_string(),
            s => format!("{s} s"),
        };
        Ok(QueryResult::from_rows(
            &["property", "value"],
            vec![
                vec![Some("key".into()), Some(key.to_string())],
                vec![Some("type".into()), Some(t)],
                vec![Some("length".into()), len.map(|l| l.to_string())],
                vec![Some("ttl".into()), Some(ttl)],
                vec![Some("encoding".into()), encoding],
                vec![Some("memory".into()), memory.map(|m| format!("{m} bytes"))],
            ],
        ))
    }

    async fn execute(&self, text: &str) -> Result<Vec<QueryResult>> {
        let mut c = self.conn.clone();
        let mut results = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("--") {
                continue;
            }
            let args = tokenize(line);
            let Some((name, rest)) = args.split_first() else { continue };
            let start = Instant::now();
            let mut command = cmd(name);
            for a in rest {
                command.arg(a);
            }
            let reply: Value = command.logged(&mut c).await.with_context(|| format!("{line}"))?;
            let mut r = reply_to_result(reply);
            r.elapsed = start.elapsed();
            results.push(r);
        }
        Ok(results)
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let mut c = self.conn.clone();
        let count: usize = cmd("CONFIG")
            .arg("GET")
            .arg("databases")
            .logged::<Vec<String>>(&mut c)
            .await
            .ok()
            .and_then(|v| v.get(1).and_then(|n| n.parse().ok()))
            .unwrap_or(16);
        Ok((0..count).map(|i| i.to_string()).collect())
    }

    async fn list_columns(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    async fn primary_key(&self, obj: &DbObject) -> Result<Option<Vec<String>>> {
        Ok(match self.key_type(&obj.name).await?.as_str() {
            "string" => Some(Vec::new()),
            "hash" => Some(vec!["field".into()]),
            "list" => Some(vec!["index".into()]),
            "set" | "zset" => Some(vec!["member".into()]),
            _ => None,
        })
    }

    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64> {
        let key = obj.name.as_str();
        let t = self.key_type(key).await?;
        let mut pipe = redis::pipe();
        pipe.atomic();
        let mut list_deletes = false;
        for change in changes {
            match (t.as_str(), change) {
                ("string", RowChange::Update { row, .. }) => {
                    pipe.cmd("SET").arg(key).arg(required(row, "value")?).arg("KEEPTTL");
                }
                ("hash", RowChange::Insert { values }) => {
                    pipe.cmd("HSET").arg(key).arg(required(values, "field")?).arg(field(values, "value").unwrap_or(""));
                }
                ("hash", RowChange::Update { key: k, row, .. }) => {
                    let old = required(k, "field")?;
                    let new = required(row, "field")?;
                    if old != new {
                        pipe.cmd("HDEL").arg(key).arg(old);
                    }
                    pipe.cmd("HSET").arg(key).arg(new).arg(field(row, "value").unwrap_or(""));
                }
                ("hash", RowChange::Delete { key: k }) => {
                    pipe.cmd("HDEL").arg(key).arg(required(k, "field")?);
                }
                ("list", RowChange::Insert { values }) => {
                    pipe.cmd("RPUSH").arg(key).arg(field(values, "value").unwrap_or(""));
                }
                ("list", RowChange::Update { key: k, row, .. }) => {
                    pipe.cmd("LSET").arg(key).arg(required(k, "index")?).arg(field(row, "value").unwrap_or(""));
                }
                ("list", RowChange::Delete { key: k }) => {
                    pipe.cmd("LSET").arg(key).arg(required(k, "index")?).arg(LIST_TOMBSTONE);
                    list_deletes = true;
                }
                ("set", RowChange::Insert { values }) => {
                    pipe.cmd("SADD").arg(key).arg(required(values, "member")?);
                }
                ("set", RowChange::Update { key: k, row, .. }) => {
                    pipe.cmd("SREM").arg(key).arg(required(k, "member")?);
                    pipe.cmd("SADD").arg(key).arg(required(row, "member")?);
                }
                ("set", RowChange::Delete { key: k }) => {
                    pipe.cmd("SREM").arg(key).arg(required(k, "member")?);
                }
                ("zset", RowChange::Insert { values: row } | RowChange::Update { row, .. }) => {
                    if let RowChange::Update { key: k, .. } = change {
                        let old = required(k, "member")?;
                        if old != required(row, "member")? {
                            pipe.cmd("ZREM").arg(key).arg(old);
                        }
                    }
                    let score: f64 = field(row, "score").unwrap_or("0").trim().parse().context("Score must be a number")?;
                    pipe.cmd("ZADD").arg(key).arg(score).arg(required(row, "member")?);
                }
                ("zset", RowChange::Delete { key: k }) => {
                    pipe.cmd("ZREM").arg(key).arg(required(k, "member")?);
                }
                (t, _) => bail!("This change is not supported for Redis {t} values"),
            }
        }
        if list_deletes {
            pipe.cmd("LREM").arg(key).arg(0).arg(LIST_TOMBSTONE);
        }
        let mut c = self.conn.clone();
        pipe.logged::<()>(&mut c).await?;
        Ok(changes.len() as u64)
    }

    async fn cancel(&self) -> Result<()> {
        bail!("Redis commands cannot be cancelled")
    }
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn tokenizes_quotes() {
        assert_eq!(tokenize(r#"SET "a b" 'c d' e"#), vec!["SET", "a b", "c d", "e"]);
        assert_eq!(tokenize(r#"SET k """#), vec!["SET", "k", ""]);
        assert_eq!(tokenize(r#"SET k "x\"y""#), vec!["SET", "k", "x\"y"]);
    }
}
