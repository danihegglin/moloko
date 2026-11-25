//! Users, sessions, personal access tokens and per-project roles.
//!
//! Browsers log in with a password and get a session cookie. Git, the CLI and AI agents use
//! personal access tokens (`mlkpat_…`), sent as a Bearer token or as the password of HTTP Basic auth.

use crate::db::{Db, now_ms};
use anyhow::{Result, bail};
use axum::http::{HeaderMap, header};
use base64::Engine as _;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    display_name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'human',
    is_admin INTEGER NOT NULL DEFAULT 0,
    password_hash TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scope TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    last_used INTEGER
);
CREATE TABLE IF NOT EXISTS members (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    PRIMARY KEY (project_id, user_id)
);
"#;

pub const SESSION_COOKIE: &str = "moloko_session";
const SESSION_TTL_MS: i64 = 30 * 24 * 3600 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "read" => Some(Role::Read),
            "write" => Some(Role::Write),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    /// `human` or `bot` (AI agents).
    pub kind: String,
    pub is_admin: bool,
    pub created_at: i64,
}

/// Limits on what a token may do. AI agent sessions get a scoped token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Scope {
    /// Only this project.
    pub project_id: Option<i64>,
    /// Only push branches starting with this prefix.
    pub push_prefix: Option<String>,
}

/// Who is making a request.
#[derive(Debug, Clone)]
pub struct Actor {
    pub user: User,
    pub scope: Option<Scope>,
}

impl Actor {
    pub fn may_push(&self, project_id: i64, branch_ref: &str) -> bool {
        let Some(scope) = &self.scope else {
            return true;
        };
        if scope.project_id.is_some_and(|p| p != project_id) {
            return false;
        }
        match &scope.push_prefix {
            Some(prefix) => branch_ref
                .strip_prefix("refs/heads/")
                .is_some_and(|b| b.starts_with(prefix.as_str())),
            None => true,
        }
    }
}

const USER_COLS: &str = "id, username, display_name, kind, is_admin, created_at";

fn map_user(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        display_name: r.get(2)?,
        kind: r.get(3)?,
        is_admin: r.get::<_, i64>(4)? != 0,
        created_at: r.get(5)?,
    })
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .expect("read /dev/urandom");
    hex::encode(buf)
}

pub fn new_pat() -> String {
    format!("mlkpat_{}", random_hex(32))
}

pub fn hash_password(password: &str) -> String {
    let salt = random_hex(16);
    let mut out = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut out)
        .expect("argon2 with valid params");
    format!("argon2id${salt}${}", hex::encode(out))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some("argon2id"), Some(salt), Some(want)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let mut out = [0u8; 32];
    if argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut out)
        .is_err()
    {
        return false;
    }
    // Constant-time compare.
    let got = hex::encode(out);
    got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0, |a, (x, y)| a | (x ^ y)) == 0
}

pub fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 39
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Db {
    pub fn user_count(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM users WHERE kind = 'human'", [], |r| {
                r.get(0)
            })?)
    }

    pub fn create_user(
        &self,
        username: &str,
        password: Option<&str>,
        kind: &str,
        is_admin: bool,
    ) -> Result<i64> {
        if !valid_username(username) {
            bail!("usernames may only contain letters, digits, '-' and '_'");
        }
        let c = self.conn();
        c.execute(
            "INSERT INTO users (username, display_name, kind, is_admin, password_hash, created_at) VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
            params![username, kind, is_admin as i64, password.map(hash_password), now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn set_password(&self, user_id: i64, password: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE users SET password_hash = ?2 WHERE id = ?1",
            params![user_id, hash_password(password)],
        )?;
        Ok(())
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY username"))?;
        let rows = st.query_map([], map_user)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn user(&self, id: i64) -> Result<Option<User>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {USER_COLS} FROM users WHERE id = ?1"),
            [id],
            map_user,
        )
        .optional()?)
    }

    pub fn user_by_name(&self, username: &str) -> Result<Option<User>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {USER_COLS} FROM users WHERE username = ?1"),
            [username],
            map_user,
        )
        .optional()?)
    }

    /// The user, if the password matches. Bots have no password and can't log in this way.
    pub fn check_password(&self, username: &str, password: &str) -> Result<Option<User>> {
        let hash: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT password_hash FROM users WHERE username = ?1",
                [username],
                |r| r.get(0),
            )
            .optional()?;
        match hash.flatten() {
            Some(h) if verify_password(password, &h) => self.user_by_name(username),
            _ => Ok(None),
        }
    }

    pub fn create_session(&self, user_id: i64) -> Result<String> {
        let token = random_hex(32);
        self.conn().execute(
            "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
            params![
                crate::agents::hash_token(&token),
                user_id,
                now_ms() + SESSION_TTL_MS
            ],
        )?;
        Ok(token)
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            [crate::agents::hash_token(token)],
        )?;
        Ok(())
    }

    fn session_user(&self, token: &str) -> Result<Option<User>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!(
                "SELECT {} FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                USER_COLS.split(", ").map(|c| format!("u.{c}")).collect::<Vec<_>>().join(", ")
            ),
            params![crate::agents::hash_token(token), now_ms()],
            map_user,
        )
        .optional()?)
    }

    /// Create a personal access token; returns (id, token). The token is shown once.
    pub fn create_token(
        &self,
        user_id: i64,
        name: &str,
        scope: Option<&Scope>,
        ttl_ms: Option<i64>,
    ) -> Result<(i64, String)> {
        let token = new_pat();
        let c = self.conn();
        c.execute(
            "INSERT INTO tokens (user_id, name, token_hash, scope, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                user_id,
                name,
                crate::agents::hash_token(&token),
                scope.map(|s| serde_json::to_string(s).unwrap()),
                now_ms(),
                ttl_ms.map(|t| now_ms() + t)
            ],
        )?;
        Ok((c.last_insert_rowid(), token))
    }

    pub fn delete_token(&self, user_id: i64, id: i64) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM tokens WHERE id = ?1 AND user_id = ?2",
            params![id, user_id],
        )? > 0)
    }

    pub fn delete_token_by_value(&self, token: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM tokens WHERE token_hash = ?1",
            [crate::agents::hash_token(token)],
        )?;
        Ok(())
    }

    pub fn tokens(&self, user_id: i64) -> Result<Vec<serde_json::Value>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT id, name, created_at, expires_at, last_used, scope FROM tokens WHERE user_id = ?1 ORDER BY id DESC",
        )?;
        let rows = st.query_map([user_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "created_at": r.get::<_, i64>(2)?,
                "expires_at": r.get::<_, Option<i64>>(3)?,
                "last_used": r.get::<_, Option<i64>>(4)?,
                "scoped": r.get::<_, Option<String>>(5)?.is_some(),
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn token_actor(&self, token: &str) -> Result<Option<Actor>> {
        let c = self.conn();
        let row: Option<(i64, i64, Option<String>)> = c
            .query_row(
                "SELECT id, user_id, scope FROM tokens WHERE token_hash = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
                params![crate::agents::hash_token(token), now_ms()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((id, user_id, scope)) = row else {
            return Ok(None);
        };
        c.execute(
            "UPDATE tokens SET last_used = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        drop(c);
        Ok(self.user(user_id)?.map(|user| Actor {
            user,
            scope: scope.and_then(|s| serde_json::from_str(&s).ok()),
        }))
    }

    pub fn set_member(&self, project_id: i64, user_id: i64, role: Role) -> Result<()> {
        self.conn().execute(
            "INSERT INTO members (project_id, user_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (project_id, user_id) DO UPDATE SET role = excluded.role",
            params![project_id, user_id, role.as_str()],
        )?;
        Ok(())
    }

    pub fn remove_member(&self, project_id: i64, user_id: i64) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM members WHERE project_id = ?1 AND user_id = ?2",
            params![project_id, user_id],
        )? > 0)
    }

    pub fn members(&self, project_id: i64) -> Result<Vec<(User, Role)>> {
        let c = self.conn();
        let cols = USER_COLS
            .split(", ")
            .map(|c| format!("u.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut st = c.prepare(&format!(
            "SELECT {cols}, m.role FROM members m JOIN users u ON u.id = m.user_id WHERE m.project_id = ?1 ORDER BY u.username"
        ))?;
        let rows = st.query_map([project_id], |r| {
            let role: String = r.get(6)?;
            Ok((map_user(r)?, Role::parse(&role).unwrap_or(Role::Read)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The user's role in a project: admins have admin everywhere, others need membership.
    pub fn role(&self, user: &User, project_id: i64) -> Result<Option<Role>> {
        if user.is_admin {
            return Ok(Some(Role::Admin));
        }
        let role: Option<String> = self
            .conn()
            .query_row(
                "SELECT role FROM members WHERE project_id = ?1 AND user_id = ?2",
                params![project_id, user.id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(role.as_deref().and_then(Role::parse))
    }

    /// Authenticate a request from its session cookie, Bearer token or Basic credentials.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Actor>> {
        if let Some(auth) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        {
            if let Some(token) = auth.strip_prefix("Bearer ") {
                return self.token_actor(token.trim());
            }
            if let Some(b64) = auth.strip_prefix("Basic ") {
                let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
                    return Ok(None);
                };
                let raw = String::from_utf8_lossy(&raw);
                let Some((user, secret)) = raw.split_once(':') else {
                    return Ok(None);
                };
                if secret.starts_with("mlkpat_") {
                    // The username is ignored, as with GitHub tokens.
                    return self.token_actor(secret);
                }
                return Ok(self
                    .check_password(user, secret)?
                    .map(|user| Actor { user, scope: None }));
            }
        }
        if let Some(token) = cookie(headers, SESSION_COOKIE) {
            return Ok(self
                .session_user(&token)?
                .map(|user| Actor { user, scope: None }));
        }
        Ok(None)
    }
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let h = hash_password("hunter2");
        assert!(verify_password("hunter2", &h));
        assert!(!verify_password("hunter3", &h));
        assert!(!verify_password("hunter2", "garbage"));
    }

    #[test]
    fn scoped_push() {
        let actor = Actor {
            user: User {
                id: 1,
                username: "coder".into(),
                display_name: "coder".into(),
                kind: "bot".into(),
                is_admin: false,
                created_at: 0,
            },
            scope: Some(Scope {
                project_id: Some(7),
                push_prefix: Some("MOL-42-".into()),
            }),
        };
        assert!(actor.may_push(7, "refs/heads/MOL-42-fix"));
        assert!(!actor.may_push(7, "refs/heads/main"));
        assert!(!actor.may_push(8, "refs/heads/MOL-42-fix"));
    }
}
