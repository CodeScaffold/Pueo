//! Visual table filters ("column operator value" rules) turned into a SQL WHERE body.

use crate::db::{DbKind, quote_ident, quote_value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    IsNull,
    IsNotNull,
    In,
}

impl Op {
    pub const ALL: [Op; 13] = [
        Op::Eq,
        Op::Ne,
        Op::Lt,
        Op::Gt,
        Op::Le,
        Op::Ge,
        Op::Contains,
        Op::NotContains,
        Op::StartsWith,
        Op::EndsWith,
        Op::IsNull,
        Op::IsNotNull,
        Op::In,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Gt => ">",
            Op::Le => "<=",
            Op::Ge => ">=",
            Op::Contains => "contains",
            Op::NotContains => "does not contain",
            Op::StartsWith => "starts with",
            Op::EndsWith => "ends with",
            Op::IsNull => "is NULL",
            Op::IsNotNull => "is not NULL",
            Op::In => "in list",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::IsNotNull)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub column: String,
    pub op: Op,
    pub value: String,
}

fn condition_sql(kind: DbKind, c: &Condition) -> Option<String> {
    let col = quote_ident(kind, &c.column);
    let lit = |v: &str| quote_value(kind, &Some(v.to_string()));
    // Case-insensitive text matching; PostgreSQL needs a cast for non-text columns.
    let (like_col, like, not_like) = match kind {
        DbKind::Postgres => (format!("CAST({col} AS TEXT)"), "ILIKE", "NOT ILIKE"),
        _ => (col.clone(), "LIKE", "NOT LIKE"),
    };
    let value = c.value.trim();
    if c.op.needs_value() && value.is_empty() {
        return None;
    }
    Some(match c.op {
        Op::Eq => format!("{col} = {}", lit(value)),
        Op::Ne => format!("{col} <> {}", lit(value)),
        Op::Lt => format!("{col} < {}", lit(value)),
        Op::Gt => format!("{col} > {}", lit(value)),
        Op::Le => format!("{col} <= {}", lit(value)),
        Op::Ge => format!("{col} >= {}", lit(value)),
        Op::Contains => format!("{like_col} {like} {}", lit(&format!("%{value}%"))),
        Op::NotContains => format!("{like_col} {not_like} {}", lit(&format!("%{value}%"))),
        Op::StartsWith => format!("{like_col} {like} {}", lit(&format!("{value}%"))),
        Op::EndsWith => format!("{like_col} {like} {}", lit(&format!("%{value}"))),
        Op::IsNull => format!("{col} IS NULL"),
        Op::IsNotNull => format!("{col} IS NOT NULL"),
        Op::In => {
            let items: Vec<String> =
                value.split(',').map(str::trim).filter(|v| !v.is_empty()).map(lit).collect();
            if items.is_empty() {
                return None;
            }
            format!("{col} IN ({})", items.join(", "))
        }
    })
}

/// The WHERE body for the given rules, or `None` when no rule is complete.
pub fn build_where(kind: DbKind, conditions: &[Condition], match_all: bool) -> Option<String> {
    let parts: Vec<String> = conditions.iter().filter_map(|c| condition_sql(kind, c)).collect();
    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => Some(
            parts.iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(if match_all { " AND " } else { " OR " }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(column: &str, op: Op, value: &str) -> Condition {
        Condition { column: column.into(), op, value: value.into() }
    }

    #[test]
    fn builds_sql() {
        let pg = DbKind::Postgres;
        assert_eq!(build_where(pg, &[c("id", Op::Gt, "10")], true).unwrap(), r#""id" > '10'"#);
        assert_eq!(
            build_where(pg, &[c("name", Op::Contains, "o'b")], true).unwrap(),
            r#"CAST("name" AS TEXT) ILIKE '%o''b%'"#
        );
        assert_eq!(
            build_where(DbKind::Mysql, &[c("code", Op::StartsWith, "PHF")], true).unwrap(),
            "`code` LIKE 'PHF%'"
        );
        assert_eq!(
            build_where(DbKind::Sqlite, &[c("a", Op::IsNull, ""), c("b", Op::In, "1, 2,,3")], false).unwrap(),
            r#"("a" IS NULL) OR ("b" IN ('1', '2', '3'))"#
        );
        // Incomplete rules are ignored.
        assert_eq!(build_where(pg, &[c("a", Op::Eq, "  ")], true), None);
        assert_eq!(build_where(pg, &[c("a", Op::Eq, ""), c("b", Op::Ne, "x")], true).unwrap(), r#""b" <> 'x'"#);
    }
}
