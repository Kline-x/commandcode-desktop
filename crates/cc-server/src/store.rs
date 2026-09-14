//! 本地持久化：账号、配额快照、请求流水、路由规则与设置。
//!
//! 用 rusqlite 的 bundled feature（自带 SQLite 源码），因此三平台行为一致、
//! 不依赖系统库——这与本项目的单二进制目标一致。
//!
//! **加密边界**：本模块只存**密文**（key_cipher）与明文提示（key_hint，
//! 形如 user_…xxxx）。密钥的加解密由宿主层（Tauri 侧，见 `src-tauri/src/secrets.rs`
//! 的 AES-256-GCM 实现）完成，cc-server 不碰明文密钥——这样即使数据库文件泄露，
//! 也拿不到可用的凭据。见 docs/ARCHITECTURE.md 第 7 节的「安全边界」。
//!
//! **保留策略**：请求流水是唯一会无限增长的表，写入时按 retention 清理，
//! 避免长期运行把磁盘吃满。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::CcError;

/// 数据库 schema 版本。每次结构变更都要 +1 并补一条迁移。
pub const SCHEMA_VERSION: i64 = 2;

/// 一条账号记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountRow {
    /// 主键。
    pub id: i64,
    /// 展示名。
    pub label: String,
    /// 加密后的 API key（明文永不入库）。
    pub key_cipher: Vec<u8>,
    /// 明文提示，形如 user_…ab12（仅用于 UI 展示）。
    pub key_hint: String,
    /// 是否参与轮换。
    pub enabled: bool,
    /// 创建时刻（epoch 毫秒）。
    pub created_at_ms: i64,
    /// 最近一次配额快照（JSON）。
    pub quota_json: Option<String>,
    /// 最近一次错误。
    pub last_error: Option<String>,
    /// 最近一次成功刷新的时刻。
    pub last_checked_ms: Option<i64>,
}

/// 一条请求流水。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRow {
    /// 主键。
    pub id: i64,
    /// 发生时刻（epoch 毫秒）。
    pub at_ms: i64,
    /// 账号 id。
    pub account_id: String,
    /// 账号展示名（备注）。
    pub account_label: Option<String>,
    /// 模型。
    pub model: String,
    /// 上游通道（cli / openai）。
    pub protocol: String,
    /// 客户端协议（openai_chat / anthropic / openai_responses）。
    pub client_protocol: String,
    /// 是否流式。
    pub stream: bool,
    /// HTTP 状态。
    pub status: u16,
    /// 错误码。
    pub error_code: Option<String>,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 缓存命中 token。
    pub cached_tokens: i64,
    /// 到首字节耗时（毫秒）。
    pub ttft_ms: Option<i64>,
    /// 总耗时（毫秒）。
    pub total_ms: i64,
    /// 尝试过的账号次数。
    pub attempts: i64,
    /// 预估消耗金额（美元）。
    pub cost_usd: f64,
}

/// 一条模型→账号路由规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRuleRow {
    /// 主键（也是优先级顺序）。
    pub id: i64,
    /// 匹配的模型 id 列表（JSON 数组）。
    pub models_json: String,
    /// 目标账号 id。
    pub account_id: String,
}

fn default_client_protocol() -> String {
    "openai_chat".to_string()
}

/// 落库用的请求记录（id 由数据库分配）。
///
/// 需要 Deserialize：控制面接收代理侧 POST 过来的记录（两者可能不在同一进程）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRequest {
    /// 发生时刻（epoch 毫秒）。
    pub at_ms: i64,
    /// 账号 id。
    pub account_id: String,
    /// 账号展示名（备注）。
    #[serde(default)]
    pub account_label: Option<String>,
    /// 模型。
    pub model: String,
    /// 上游通道。
    pub protocol: String,
    /// 客户端协议。
    #[serde(default = "default_client_protocol")]
    pub client_protocol: String,
    /// 是否流式。
    pub stream: bool,
    /// HTTP 状态。
    pub status: u16,
    /// 错误码。
    pub error_code: Option<String>,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 缓存命中 token。
    pub cached_tokens: i64,
    /// 到首字节耗时（毫秒）。
    pub ttft_ms: Option<i64>,
    /// 总耗时（毫秒）。
    pub total_ms: i64,
    /// 尝试过的账号次数。
    pub attempts: i64,
    /// 预估消耗金额。
    #[serde(default)]
    pub cost_usd: f64,
}

/// 存储句柄。
///
/// 用 Mutex<Connection> 而非连接池：本应用写入量极低（每次请求一行），
/// SQLite 在单写者模型下最省心；Connection 本身不是 Sync，必须串行化。
pub struct Store {
    conn: Mutex<Connection>,
    /// 请求流水的保留条数上限（0 表示不清理）。
    ///
    /// 用 `AtomicI64` 而不是普通字段：设置页可以在运行时改它，而 [Store] 是
    /// 以 `Arc<Store>` 共享的（方法都收 `&self`）。此前这里是只读字段且被
    /// `bootstrap` 硬编码成 5000，于是设置页保存的「保留条数」能读能显示、
    /// 却对实际裁剪**毫无影响**——一个会骗人的假设置。
    retention: std::sync::atomic::AtomicI64,
}

impl Store {
    /// 打开（或创建）数据库并跑迁移。
    pub fn open(path: impl AsRef<Path>, retention: i64) -> Result<Self, CcError> {
        let conn = Connection::open(path).map_err(db_err)?;
        Self::from_connection(conn, retention)
    }

    /// 打开一个纯内存数据库（测试用）。
    pub fn open_in_memory() -> Result<Self, CcError> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        Self::from_connection(conn, 0)
    }

    fn from_connection(conn: Connection, retention: i64) -> Result<Self, CcError> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(db_err)?;
        let store = Self {
            conn: Mutex::new(conn),
            retention: std::sync::atomic::AtomicI64::new(retention),
        };
        store.migrate()?;
        Ok(store)
    }

    /// 借出连接。
    ///
    /// 锁中毒时取回内部数据继续跑：一个线程的 panic 不该把整个应用的数据库
    /// 访问永久锁死。这与 mock_upstream 的处理一致。
    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T, CcError>) -> Result<T, CcError> {
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    /// 建表与迁移（可重入）。
    fn migrate(&self) -> Result<(), CcError> {
        self.with(|conn| {
            conn.execute_batch(SCHEMA_SQL).map_err(db_err)?;
            // 兼容老版本库：增量补全新列（已存在时忽略错误）
            let _ = conn.execute("ALTER TABLE requests ADD COLUMN account_label TEXT", []);
            let _ = conn.execute(
                "ALTER TABLE requests ADD COLUMN client_protocol TEXT NOT NULL DEFAULT 'openai_chat'",
                [],
            );
            let _ = conn.execute(
                "ALTER TABLE requests ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0.0",
                [],
            );
            conn.execute(
                "INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    /// 读取 schema 版本。
    pub fn schema_version(&self) -> Result<i64, CcError> {
        self.with(|conn| {
            let value: Option<String> = conn
                .query_row(
                    "SELECT value FROM schema_meta WHERE key = 'version'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
        })
    }
}

// ------------------------------------------------------------------ 账号

impl Store {
    /// 新增账号，返回其 id。
    pub fn insert_account(
        &self,
        label: &str,
        key_cipher: &[u8],
        key_hint: &str,
        created_at_ms: i64,
    ) -> Result<i64, CcError> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO accounts (label, key_cipher, key_hint, enabled, created_at_ms)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![label, key_cipher, key_hint, created_at_ms],
            )
            .map_err(db_err)?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 列出全部账号（按 id 升序 = 配置顺序 = 轮换顺序）。
    pub fn list_accounts(&self) -> Result<Vec<AccountRow>, CcError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(ACCOUNT_SELECT).map_err(db_err)?;
            let rows = stmt
                .query_map([], map_account)
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            Ok(rows)
        })
    }

    /// 删除账号。
    pub fn delete_account(&self, id: i64) -> Result<bool, CcError> {
        self.with(|conn| {
            let affected = conn
                .execute("DELETE FROM accounts WHERE id = ?1", params![id])
                .map_err(db_err)?;
            Ok(affected > 0)
        })
    }

    /// 改展示名。
    pub fn rename_account(&self, id: i64, label: &str) -> Result<bool, CcError> {
        self.with(|conn| {
            let affected = conn
                .execute(
                    "UPDATE accounts SET label = ?2 WHERE id = ?1",
                    params![id, label],
                )
                .map_err(db_err)?;
            Ok(affected > 0)
        })
    }

    /// 启停一个账号。
    pub fn set_account_enabled(&self, id: i64, enabled: bool) -> Result<bool, CcError> {
        self.with(|conn| {
            let affected = conn
                .execute(
                    "UPDATE accounts SET enabled = ?2 WHERE id = ?1",
                    params![id, if enabled { 1 } else { 0 }],
                )
                .map_err(db_err)?;
            Ok(affected > 0)
        })
    }

    /// 写入一次配额刷新结果。
    ///
    /// quota_json 传 None 表示「这次没拿到新快照」（例如刷新失败）——
    /// 此时用 COALESCE 保留上一次的快照，面板不会突然空白。
    pub fn update_account_quota(
        &self,
        id: i64,
        quota_json: Option<&str>,
        last_error: Option<&str>,
        checked_at_ms: i64,
    ) -> Result<bool, CcError> {
        self.with(|conn| {
            let affected = conn
                .execute(
                    "UPDATE accounts
                     SET quota_json = COALESCE(?2, quota_json),
                         last_error = ?3,
                         last_checked_ms = ?4
                     WHERE id = ?1",
                    params![id, quota_json, last_error, checked_at_ms],
                )
                .map_err(db_err)?;
            Ok(affected > 0)
        })
    }

    /// 按 key 提示查找账号（用于「重复添加」检测）。
    pub fn find_account_by_hint(&self, key_hint: &str) -> Result<Option<AccountRow>, CcError> {
        self.with(|conn| {
            let row = conn
                .query_row(
                    "SELECT id, label, key_cipher, key_hint, enabled, created_at_ms,
                            quota_json, last_error, last_checked_ms
                     FROM accounts WHERE key_hint = ?1 LIMIT 1",
                    params![key_hint],
                    map_account,
                )
                .optional()
                .map_err(db_err)?;
            Ok(row)
        })
    }
}

// -------------------------------------------------------------- 请求流水

impl Store {
    /// 追加一条请求流水，并按保留策略清理旧记录。
    pub fn insert_request(&self, req: &NewRequest) -> Result<i64, CcError> {
        let mut cost = req.cost_usd;
        if cost == 0.0 && req.status == 200 && (req.input_tokens > 0 || req.output_tokens > 0) {
            cost = crate::proxy::estimate_cost_usd(
                &req.model,
                req.status,
                req.input_tokens.max(0) as u64,
                req.output_tokens.max(0) as u64,
                req.cached_tokens.max(0) as u64,
            );
        }
        let id = self.with(|conn| {
            conn.execute(
                "INSERT INTO requests (
                    at_ms, account_id, account_label, model, protocol, client_protocol, stream, status, error_code,
                    input_tokens, output_tokens, cached_tokens, ttft_ms, total_ms, attempts, cost_usd
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                params![
                    req.at_ms,
                    req.account_id,
                    req.account_label,
                    req.model,
                    req.protocol,
                    req.client_protocol,
                    if req.stream { 1 } else { 0 },
                    req.status,
                    req.error_code,
                    req.input_tokens,
                    req.output_tokens,
                    req.cached_tokens,
                    req.ttft_ms,
                    req.total_ms,
                    req.attempts,
                    cost,
                ],
            )
            .map_err(db_err)?;
            Ok(conn.last_insert_rowid())
        })?;
        if self.retention() > 0 {
            self.trim_requests(self.retention())?;
        }
        Ok(id)
    }

    /// 当前的流水保留条数（0 表示不清理）。
    pub fn retention(&self) -> i64 {
        self.retention.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 运行时修改保留条数，并**立即**按新上限裁剪一次。
    ///
    /// 立即裁剪是必要的：只改数字的话，用户把上限从 5000 调小到 100 之后，
    /// 要等到下一次写入请求才会生效——而「下次写入」在空闲期可能永远不来，
    /// 于是 UI 上显示已生效、数据库里却还是 5000 条。
    pub fn set_retention(&self, keep: i64) -> Result<usize, CcError> {
        self.retention
            .store(keep, std::sync::atomic::Ordering::Relaxed);
        self.trim_requests(keep)
    }

    /// 只保留最近 keep 条请求流水。
    pub fn trim_requests(&self, keep: i64) -> Result<usize, CcError> {
        if keep <= 0 {
            return Ok(0);
        }
        self.with(|conn| {
            let affected = conn
                .execute(
                    "DELETE FROM requests WHERE id NOT IN (
                        SELECT id FROM requests ORDER BY id DESC LIMIT ?1
                     )",
                    params![keep],
                )
                .map_err(db_err)?;
            Ok(affected)
        })
    }

    /// 最近的请求流水（新的在前）。
    pub fn recent_requests(&self, limit: i64) -> Result<Vec<RequestRow>, CcError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(REQUEST_SELECT).map_err(db_err)?;
            let rows = stmt
                .query_map(params![limit], map_request)
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            Ok(rows)
        })
    }

    /// 请求流水的总条数。
    pub fn request_count(&self) -> Result<i64, CcError> {
        self.with(|conn| {
            conn.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
                .map_err(db_err)
        })
    }

    /// 按账号汇总用量（面板的「哪个账号用了多少」）。
    pub fn usage_by_account(&self) -> Result<Vec<(String, i64, i64, i64)>, CcError> {
        self.with(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT account_id,
                            COALESCE(SUM(input_tokens), 0),
                            COALESCE(SUM(output_tokens), 0),
                            COALESCE(SUM(cached_tokens), 0)
                     FROM requests GROUP BY account_id ORDER BY account_id ASC",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            Ok(rows)
        })
    }
}

// -------------------------------------------------------------- 路由规则

impl Store {
    /// 新增一条路由规则（按插入顺序决定优先级）。
    pub fn insert_route_rule(&self, models_json: &str, account_id: &str) -> Result<i64, CcError> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO route_rules (models_json, account_id) VALUES (?1, ?2)",
                params![models_json, account_id],
            )
            .map_err(db_err)?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 列出全部路由规则（顺序即优先级）。
    pub fn list_route_rules(&self) -> Result<Vec<RouteRuleRow>, CcError> {
        self.with(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, models_json, account_id FROM route_rules ORDER BY id ASC")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(RouteRuleRow {
                        id: row.get(0)?,
                        models_json: row.get(1)?,
                        account_id: row.get(2)?,
                    })
                })
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            Ok(rows)
        })
    }

    /// 清空并重建路由规则（面板保存时整表替换最简单也最不易出错）。
    pub fn replace_route_rules(&self, rules: &[(String, String)]) -> Result<(), CcError> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction().map_err(db_err)?;
            tx.execute("DELETE FROM route_rules", []).map_err(db_err)?;
            for (models_json, account_id) in rules {
                tx.execute(
                    "INSERT INTO route_rules (models_json, account_id) VALUES (?1, ?2)",
                    params![models_json, account_id],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }
}

// ------------------------------------------------------------------ 设置

impl Store {
    /// 读设置。
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, CcError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
    }

    /// 写设置（同 key 覆盖）。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), CcError> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }
}

/// 建表语句。
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    key_cipher BLOB NOT NULL,
    key_hint TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at_ms INTEGER NOT NULL,
    quota_json TEXT,
    last_error TEXT,
    last_checked_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_accounts_enabled ON accounts(enabled);
CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms INTEGER NOT NULL,
    account_id TEXT NOT NULL,
    account_label TEXT,
    model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    client_protocol TEXT NOT NULL DEFAULT 'openai_chat',
    stream INTEGER NOT NULL,
    status INTEGER NOT NULL,
    error_code TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    ttft_ms INTEGER,
    total_ms INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 1,
    cost_usd REAL NOT NULL DEFAULT 0.0
);
CREATE INDEX IF NOT EXISTS idx_requests_at ON requests(at_ms DESC);
CREATE TABLE IF NOT EXISTS route_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    models_json TEXT NOT NULL,
    account_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// 账号查询语句（三处复用，避免列顺序不一致）。
const ACCOUNT_SELECT: &str = "SELECT id, label, key_cipher, key_hint, enabled, created_at_ms,
                                    quota_json, last_error, last_checked_ms
                             FROM accounts ORDER BY id ASC";

/// 请求查询语句：LEFT JOIN 账号表以获取最新的展示名/备注。
const REQUEST_SELECT: &str = "SELECT r.id, r.at_ms, r.account_id,
                                     COALESCE(NULLIF(r.account_label, ''), NULLIF(a.label, ''), a.key_hint, r.account_id) AS account_label,
                                     r.model, r.protocol,
                                     COALESCE(r.client_protocol, 'openai_chat') AS client_protocol,
                                     r.stream, r.status,
                                     r.error_code, r.input_tokens, r.output_tokens, r.cached_tokens,
                                     r.ttft_ms, r.total_ms, r.attempts,
                                     COALESCE(r.cost_usd, 0.0) AS cost_usd
                              FROM requests r
                              LEFT JOIN accounts a ON r.account_id = CAST(a.id AS TEXT)
                              ORDER BY r.id DESC LIMIT ?1";

/// 行 → AccountRow。
fn map_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        id: row.get(0)?,
        label: row.get(1)?,
        key_cipher: row.get(2)?,
        key_hint: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        created_at_ms: row.get(5)?,
        quota_json: row.get(6)?,
        last_error: row.get(7)?,
        last_checked_ms: row.get(8)?,
    })
}

/// 行 → RequestRow。
fn map_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    let mut req = RequestRow {
        id: row.get(0)?,
        at_ms: row.get(1)?,
        account_id: row.get(2)?,
        account_label: row.get(3)?,
        model: row.get(4)?,
        protocol: row.get(5)?,
        client_protocol: row.get(6)?,
        stream: row.get::<_, i64>(7)? != 0,
        status: row.get::<_, i64>(8)? as u16,
        error_code: row.get(9)?,
        input_tokens: row.get(10)?,
        output_tokens: row.get(11)?,
        cached_tokens: row.get(12)?,
        ttft_ms: row.get(13)?,
        total_ms: row.get(14)?,
        attempts: row.get(15)?,
        cost_usd: row.get(16)?,
    };
    if req.cost_usd == 0.0 && req.status == 200 && (req.input_tokens > 0 || req.output_tokens > 0) {
        req.cost_usd = crate::proxy::estimate_cost_usd(
            &req.model,
            req.status,
            req.input_tokens.max(0) as u64,
            req.output_tokens.max(0) as u64,
            req.cached_tokens.max(0) as u64,
        );
    }
    Ok(req)
}

/// 把 rusqlite 错误统一成 [CcError::Transport]。
///
/// 数据库故障对用户而言是「本地存储出问题」，不是上游协议问题。错误类型里
/// 没有专门的 Storage 变体，而 Transport 更贴近实际语义（本地 I/O 失败，
/// 重试可能有用），并在消息里写明来源以便诊断。
fn db_err(e: rusqlite::Error) -> CcError {
    CcError::Transport(format!("本地数据库错误：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn sample_request(account: &str, status: u16) -> NewRequest {
        NewRequest {
            at_ms: 1_700_000_000_000,
            account_id: account.to_string(),
            account_label: Some(format!("账号_{account}")),
            model: "deepseek/deepseek-v4-flash".into(),
            protocol: "cli".into(),
            client_protocol: "openai_chat".into(),
            stream: true,
            status,
            error_code: None,
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 2,
            ttft_ms: Some(120),
            total_ms: 900,
            attempts: 1,
            cost_usd: 0.0001,
        }
    }

    #[test]
    fn migration_sets_schema_version() {
        let s = store();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_idempotent_on_reopen() {
        // 迁移必须可重入：同一个文件重复打开不应报错
        let dir = std::env::temp_dir().join(format!("cc-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idempotent.db");
        let _ = std::fs::remove_file(&path);
        drop(Store::open(&path, 0).unwrap());
        let reopened = Store::open(&path, 0).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn account_roundtrip_preserves_cipher_and_hint() {
        let s = store();
        let cipher = vec![1u8, 2, 3, 4];
        let id = s
            .insert_account("Go #1", &cipher, "user_…ab12", 1_000)
            .unwrap();
        let rows = s.list_accounts().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].label, "Go #1");
        assert_eq!(
            rows[0].key_cipher, cipher,
            "密文必须原样存取（解密在宿主层）"
        );
        assert_eq!(rows[0].key_hint, "user_…ab12");
        assert!(rows[0].enabled, "新账号默认启用");
        assert_eq!(rows[0].created_at_ms, 1_000);
    }

    #[test]
    fn accounts_are_listed_in_rotation_order() {
        let s = store();
        s.insert_account("first", b"a", "h1", 1).unwrap();
        s.insert_account("second", b"b", "h2", 2).unwrap();
        let labels: Vec<String> = s
            .list_accounts()
            .unwrap()
            .into_iter()
            .map(|r| r.label)
            .collect();
        assert_eq!(
            labels,
            vec!["first", "second"],
            "id 升序即配置顺序即轮换顺序"
        );
    }

    #[test]
    fn account_rename_enable_delete() {
        let s = store();
        let id = s.insert_account("old", b"k", "hint", 1).unwrap();
        assert!(s.rename_account(id, "new").unwrap());
        assert_eq!(s.list_accounts().unwrap()[0].label, "new");

        assert!(s.set_account_enabled(id, false).unwrap());
        assert!(!s.list_accounts().unwrap()[0].enabled);

        assert!(s.delete_account(id).unwrap());
        assert!(s.list_accounts().unwrap().is_empty());
        assert!(
            !s.delete_account(id).unwrap(),
            "重复删除应返回 false 而非报错"
        );
    }

    #[test]
    fn quota_update_keeps_previous_snapshot_when_refresh_fails() {
        let s = store();
        let id = s.insert_account("a", b"k", "h", 1).unwrap();
        s.update_account_quota(id, Some(r#"{"used":1}"#), None, 100)
            .unwrap();
        let row = &s.list_accounts().unwrap()[0];
        assert_eq!(row.quota_json.as_deref(), Some(r#"{"used":1}"#));
        assert_eq!(row.last_checked_ms, Some(100));

        // 刷新失败：记下错误，但上一次的快照要保留（面板不该突然空白）
        s.update_account_quota(id, None, Some("boom"), 200).unwrap();
        let row = &s.list_accounts().unwrap()[0];
        assert_eq!(
            row.quota_json.as_deref(),
            Some(r#"{"used":1}"#),
            "失败不应清掉旧快照"
        );
        assert_eq!(row.last_error.as_deref(), Some("boom"));
        assert_eq!(row.last_checked_ms, Some(200));
    }

    #[test]
    fn duplicate_detection_by_key_hint() {
        let s = store();
        s.insert_account("a", b"k", "user_…1111", 1).unwrap();
        assert!(s.find_account_by_hint("user_…1111").unwrap().is_some());
        assert!(s.find_account_by_hint("user_…2222").unwrap().is_none());
    }

    #[test]
    fn request_rows_roundtrip_with_all_fields() {
        let s = store();
        let id = s.insert_request(&sample_request("default", 200)).unwrap();
        let rows = s.recent_requests(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].account_id, "default");
        assert_eq!(rows[0].account_label.as_deref(), Some("账号_default"));
        assert_eq!(rows[0].client_protocol, "openai_chat");
        assert!(rows[0].cost_usd > 0.0);
        assert!(rows[0].stream);
        assert_eq!(rows[0].status, 200);
        assert_eq!(rows[0].input_tokens, 10);
        assert_eq!(rows[0].cached_tokens, 2);
        assert_eq!(rows[0].ttft_ms, Some(120));
        assert_eq!(rows[0].error_code, None);
    }

    #[test]
    fn recent_requests_are_newest_first() {
        let s = store();
        s.insert_request(&sample_request("a", 200)).unwrap();
        s.insert_request(&sample_request("b", 500)).unwrap();
        let rows = s.recent_requests(10).unwrap();
        assert_eq!(
            rows[0].account_id, "b",
            "最新的请求应排在最前（面板要倒序展示）"
        );
    }

    #[test]
    fn retention_trimming_keeps_only_the_newest() {
        let dir = std::env::temp_dir().join(format!("cc-store-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trim.db");
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path, 3).unwrap();

        for i in 0..6 {
            let mut req = sample_request("a", 200);
            req.at_ms = 1_000 + i;
            s.insert_request(&req).unwrap();
        }
        assert_eq!(
            s.request_count().unwrap(),
            3,
            "超过保留上限后应自动清理旧记录"
        );
        assert_eq!(
            s.recent_requests(10).unwrap()[0].at_ms,
            1_005,
            "保留的是最新的三条"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn manual_trim_handles_disabled_retention() {
        let s = store();
        s.insert_request(&sample_request("a", 200)).unwrap();
        assert_eq!(s.trim_requests(0).unwrap(), 0, "保留条数为 0 表示不清理");
        assert_eq!(s.request_count().unwrap(), 1);
    }

    #[test]
    fn set_retention_takes_effect_immediately() {
        // 回归：retention 曾是只读字段且被 bootstrap 硬编码，设置页改了也没用。
        // 现在改上限必须【立刻】裁剪，而不是等下一次写入。
        let s = Store::open_in_memory().unwrap();
        for i in 0..6 {
            let mut req = sample_request("a", 200);
            req.at_ms = 1_000 + i;
            s.insert_request(&req).unwrap();
        }
        assert_eq!(s.request_count().unwrap(), 6, "retention=0 时不清理");

        let trimmed = s.set_retention(2).unwrap();
        assert_eq!(trimmed, 4, "改小上限应立即删掉多余的 4 条");
        assert_eq!(s.retention(), 2);
        assert_eq!(s.request_count().unwrap(), 2, "无需等待下一次写入");

        // 后续写入也遵守新上限
        let mut req = sample_request("a", 200);
        req.at_ms = 9_999;
        s.insert_request(&req).unwrap();
        assert_eq!(s.request_count().unwrap(), 2);
        assert_eq!(
            s.recent_requests(10).unwrap()[0].at_ms,
            9_999,
            "保留的仍应是最新的记录"
        );
    }

    #[test]
    fn usage_aggregates_by_account() {
        let s = store();
        s.insert_request(&sample_request("a", 200)).unwrap();
        s.insert_request(&sample_request("a", 200)).unwrap();
        s.insert_request(&sample_request("b", 200)).unwrap();
        let rows = s.usage_by_account().unwrap();
        let a = rows.iter().find(|r| r.0 == "a").unwrap();
        assert_eq!(a.1, 20, "a 的输入 token 应是两次之和");
        assert_eq!(a.3, 4, "缓存命中也要累加");
        assert_eq!(rows.iter().find(|r| r.0 == "b").unwrap().1, 10);
    }

    #[test]
    fn route_rules_preserve_order_and_can_be_replaced() {
        let s = store();
        s.insert_route_rule(r#"["m1"]"#, "a").unwrap();
        s.insert_route_rule(r#"["m2"]"#, "b").unwrap();
        let rules = s.list_route_rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].account_id, "a", "插入顺序即优先级");

        s.replace_route_rules(&[(r#"["m3"]"#.to_string(), "c".to_string())])
            .unwrap();
        let rules = s.list_route_rules().unwrap();
        assert_eq!(rules.len(), 1, "整表替换应清掉旧规则");
        assert_eq!(rules[0].account_id, "c");
    }

    #[test]
    fn settings_upsert() {
        let s = store();
        assert_eq!(s.get_setting("port").unwrap(), None);
        s.set_setting("port", "3050").unwrap();
        assert_eq!(s.get_setting("port").unwrap().as_deref(), Some("3050"));
        s.set_setting("port", "3051").unwrap();
        assert_eq!(
            s.get_setting("port").unwrap().as_deref(),
            Some("3051"),
            "同 key 应覆盖"
        );
    }

    #[test]
    fn storage_errors_are_reported_as_transport_not_protocol() {
        // 数据库故障属于本地 I/O 问题，不该伪装成上游协议错误
        let err = db_err(rusqlite::Error::InvalidQuery);
        assert!(matches!(err, CcError::Transport(_)));
        assert!(err.to_string().contains("本地数据库"));
    }
}
