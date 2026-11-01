//! Multi-user authentication with role-based access control.

use std::collections::BTreeMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::registry::DatabaseRegistry;
use crate::sync::Unpoisoned;

/// Access roles, ordered by privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Reader,
    Editor,
    Admin,
}

impl Role {
    /// Role from a case-insensitive string.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ADMIN" => Some(Self::Admin),
            "EDITOR" => Some(Self::Editor),
            "READER" => Some(Self::Reader),
            _ => None,
        }
    }

    /// Role name, uppercase.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admin => "ADMIN",
            Self::Editor => "EDITOR",
            Self::Reader => "READER",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stored user record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub username: String,
    pub password_hash: String,
    pub role: Role,
}

/// bcrypt cost of stored password hashes.
const BCRYPT_COST: u32 = 12;

/// Why a user change didn't happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserError {
    /// A user by that name exists.
    Exists(String),
    /// No user by that name.
    NotFound(String),
    /// Dropping the user the session is logged in as.
    DropSelf,
    /// The password couldn't be hashed.
    Hash(String),
    /// The change couldn't be saved, so it wasn't made.
    Save(String),
}

impl UserError {
    /// Neo4j status code.
    pub fn code(&self) -> &'static str {
        match self {
            UserError::Exists(_) => "Neo.ClientError.Security.UserAlreadyExists",
            UserError::NotFound(_) => "Neo.ClientError.Security.UserNotFound",
            UserError::DropSelf => "Neo.ClientError.Security.Forbidden",
            UserError::Hash(_) | UserError::Save(_) => "Neo.DatabaseError.General.UnknownError",
        }
    }
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserError::Exists(u) => write!(f, "user '{u}' already exists"),
            UserError::NotFound(u) => write!(f, "user '{u}' does not exist"),
            UserError::DropSelf => f.write_str("cannot drop the currently logged-in user"),
            UserError::Hash(e) => write!(f, "couldn't hash the password: {e}"),
            UserError::Save(e) => write!(f, "couldn't save the user catalog: {e}"),
        }
    }
}

impl std::error::Error for UserError {}

/// A change to the user catalog, ready to apply. Passwords are hashed when it's made, before any
/// lock is taken: bcrypt is slow on purpose, and nobody should wait on a lock meanwhile.
#[derive(Debug, Clone)]
pub enum UserChange {
    Add {
        username: String,
        hash: String,
        role: Role,
    },
    Drop {
        username: String,
    },
    Alter {
        username: String,
        hash: Option<String>,
        role: Option<Role>,
    },
}

fn hash_password(password: &str) -> Result<String, UserError> {
    bcrypt::hash(password, BCRYPT_COST).map_err(|e| UserError::Hash(e.to_string()))
}

impl UserChange {
    pub fn add(username: &str, password: &str, role: Role) -> Result<Self, UserError> {
        Ok(Self::Add {
            username: username.to_string(),
            hash: hash_password(password)?,
            role,
        })
    }

    pub fn drop(username: &str) -> Self {
        Self::Drop {
            username: username.to_string(),
        }
    }

    pub fn alter(
        username: &str,
        password: Option<&str>,
        role: Option<Role>,
    ) -> Result<Self, UserError> {
        Ok(Self::Alter {
            username: username.to_string(),
            hash: password.map(hash_password).transpose()?,
            role,
        })
    }
}

/// In-memory catalog of all users, serializable to/from the tvdb metadata page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthCatalog {
    /// By name, so the stored catalog is the same bytes for the same users.
    users: BTreeMap<String, UserRecord>,
    /// Bumped by every change, so credentials verified earlier can be recognized as stale.
    #[serde(skip)]
    generation: u64,
}

impl AuthCatalog {
    /// Empty catalog (auth disabled).
    pub fn new() -> Self {
        Self::default()
    }

    /// No users registered?
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Changes so far; see [`VerifiedCache`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Applies a prepared change.
    pub fn apply(&mut self, change: &UserChange) -> Result<(), UserError> {
        match change {
            UserChange::Add {
                username,
                hash,
                role,
            } => {
                if self.users.contains_key(username) {
                    return Err(UserError::Exists(username.clone()));
                }
                self.users.insert(
                    username.clone(),
                    UserRecord {
                        username: username.clone(),
                        password_hash: hash.clone(),
                        role: *role,
                    },
                );
            }
            UserChange::Drop { username } => {
                if self.users.remove(username).is_none() {
                    return Err(UserError::NotFound(username.clone()));
                }
            }
            UserChange::Alter {
                username,
                hash,
                role,
            } => {
                let record = self
                    .users
                    .get_mut(username)
                    .ok_or_else(|| UserError::NotFound(username.clone()))?;
                if let Some(h) = hash {
                    record.password_hash = h.clone();
                }
                if let Some(r) = role {
                    record.role = *r;
                }
            }
        }
        self.generation += 1;
        Ok(())
    }

    /// Adds a user, hashing the password here: for catalogs nobody else holds yet (seeding).
    pub fn add_user(
        &mut self,
        username: &str,
        password: &str,
        role: Role,
    ) -> Result<(), UserError> {
        self.apply(&UserChange::add(username, password, role)?)
    }

    /// `username`'s stored password hash and role, to check a password against with
    /// [`password_matches`] once the lock is released.
    pub fn credentials(&self, username: &str) -> Option<(String, Role)> {
        self.users
            .get(username)
            .map(|r| (r.password_hash.clone(), r.role))
    }

    /// All users as (username, role), by name; no password hashes.
    pub fn list_users(&self) -> Vec<(&str, Role)> {
        self.users
            .values()
            .map(|r| (r.username.as_str(), r.role))
            .collect()
    }

    /// Serializes the catalog for the tvdb metadata page: a framed, CRC-32-checked `traverse_codec`
    /// container.
    pub fn serialize(&self) -> Vec<u8> {
        traverse_codec::encode(traverse_codec::Kind::AuthCatalog, self)
            .expect("AuthCatalog serialization should not fail")
    }

    /// Deserializes a catalog from the metadata page. Accepts the framed layout and the headerless
    /// bincode layout of 0.8.2 and earlier, so existing tvdb files keep their users.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, String> {
        traverse_codec::decode(traverse_codec::Kind::AuthCatalog, bytes)
            .map_err(|e| format!("auth catalog deserialization error: {e}"))
    }
}

/// Does `password` match a stored bcrypt `hash`? Slow on purpose: call it without holding the
/// catalog's lock.
pub fn password_matches(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// The role of `username` if `password` is theirs. bcrypt runs after the catalog's lock is
/// released, and an unknown user costs a check too, so timing doesn't tell who exists.
pub fn authenticate(users: &RwLock<AuthCatalog>, username: &str, password: &str) -> Option<Role> {
    static UNKNOWN_USER: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| bcrypt::hash("no such user", BCRYPT_COST).unwrap_or_default());
    let found = users.read_unpoisoned().credentials(username);
    match found {
        Some((hash, role)) => password_matches(password, &hash).then_some(role),
        None => {
            password_matches(password, &UNKNOWN_USER);
            None
        }
    }
}

/// Runs a user-management statement on the live catalog `users`, saving changes through
/// `registry`. Passwords are hashed before any lock is taken. `Ok(Some(users))` for SHOW USERS,
/// `Ok(None)` for a change; `current_user` can't drop themselves.
pub fn run_user_command(
    command: UserCommand,
    users: &RwLock<AuthCatalog>,
    registry: &DatabaseRegistry,
    current_user: Option<&str>,
) -> Result<Option<Vec<(String, Role)>>, UserError> {
    let known = |name: &str| users.read_unpoisoned().credentials(name).is_some();
    let change = match command {
        UserCommand::Show => {
            let users = users.read_unpoisoned();
            let list = users.list_users();
            return Ok(Some(
                list.into_iter()
                    .map(|(name, role)| (name.to_string(), role))
                    .collect(),
            ));
        }
        // the existence checks spare a doomed bcrypt; the change re-checks under the lock
        UserCommand::Create {
            username,
            password,
            role,
        } => {
            if known(&username) {
                return Err(UserError::Exists(username));
            }
            UserChange::add(&username, &password, role)?
        }
        UserCommand::Drop { username } => {
            if current_user == Some(username.as_str()) {
                return Err(UserError::DropSelf);
            }
            UserChange::drop(&username)
        }
        UserCommand::Alter {
            username,
            password,
            role,
        } => {
            if !known(&username) {
                return Err(UserError::NotFound(username));
            }
            UserChange::alter(&username, password.as_deref(), role)?
        }
    };
    registry.change_users(users, &change)?;
    Ok(None)
}

/// Credentials checked recently, so per-request Basic auth (gRPC) doesn't run bcrypt on every
/// call. Keyed by a SHA-256 over a per-process random secret, the user name and the password, so
/// neither is kept; an entry only counts for the catalog [generation](AuthCatalog::generation) it
/// was checked against, so any user change makes every entry stale.
#[cfg(feature = "grpc")]
pub struct VerifiedCache {
    secret: [u8; 32],
    entries: std::sync::Mutex<std::collections::HashMap<[u8; 32], (u64, Role)>>,
}

#[cfg(feature = "grpc")]
impl Default for VerifiedCache {
    fn default() -> Self {
        let mut secret = [0u8; 32];
        secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self {
            secret,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[cfg(feature = "grpc")]
impl VerifiedCache {
    /// Entries kept before the cache starts over.
    const CAPACITY: usize = 4096;

    fn key(&self, username: &str, password: &str) -> [u8; 32] {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        h.update(self.secret);
        h.update((username.len() as u64).to_le_bytes());
        h.update(username.as_bytes());
        h.update(password.as_bytes());
        h.finalize().into()
    }

    /// The role of credentials checked against catalog `generation`.
    pub fn get(&self, username: &str, password: &str, generation: u64) -> Option<Role> {
        let key = self.key(username, password);
        let entries = crate::sync::LockUnpoisoned::lock_unpoisoned(&self.entries);
        entries
            .get(&key)
            .filter(|(g, _)| *g == generation)
            .map(|(_, role)| *role)
    }

    /// Remembers credentials that matched against catalog `generation`.
    pub fn insert(&self, username: &str, password: &str, generation: u64, role: Role) {
        let key = self.key(username, password);
        let mut entries = crate::sync::LockUnpoisoned::lock_unpoisoned(&self.entries);
        if entries.len() >= Self::CAPACITY {
            entries.clear();
        }
        entries.insert(key, (generation, role));
    }
}

/// A user-management statement, as Bolt and gRPC accept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserCommand {
    Show,
    Create {
        username: String,
        password: String,
        role: Role,
    },
    Drop {
        username: String,
    },
    Alter {
        username: String,
        password: Option<String>,
        role: Option<Role>,
    },
}

const CREATE_SYNTAX: &str =
    "CREATE USER <name> SET PASSWORD '<pass>' [SET ROLE ADMIN|EDITOR|READER]";
const ALTER_SYNTAX: &str = "ALTER USER <name> SET PASSWORD '<pass>' | SET ROLE ADMIN|EDITOR|READER";

/// The quoted password after `SET PASSWORD`, case preserved.
fn extract_password(query: &str) -> Option<String> {
    let upper = query.to_uppercase();
    let idx = upper.find("SET PASSWORD")?;
    let after = query[idx + 12..].trim();
    let rest = after.strip_prefix('\'')?;
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// The role named after `SET ROLE` in `upper` (an upper-cased statement), if any.
fn parse_role(upper: &str) -> Option<Result<Role, String>> {
    let pos = upper.find("SET ROLE")?;
    let name = upper[pos + 8..].trim().trim_end_matches(';').trim();
    let name = name.split_whitespace().next().unwrap_or(name);
    Some(
        Role::from_str_ci(name)
            .ok_or_else(|| format!("invalid role '{name}': must be ADMIN, EDITOR, or READER")),
    )
}

/// `SHOW USERS`, `CREATE USER <name> SET PASSWORD '<pass>' [SET ROLE <role>]`, `DROP USER <name>`
/// and `ALTER USER <name> [SET PASSWORD '<pass>'] [SET ROLE <role>]`. `None` for any other
/// statement, `Some(Err(message))` for a malformed one. User names are case-insensitive (stored
/// lower-case); passwords keep their case.
pub fn parse_user_command(query: &str) -> Option<Result<UserCommand, String>> {
    let upper = query.to_uppercase();
    let trimmed = upper.trim();
    if trimmed == "SHOW USERS" || trimmed == "SHOW USERS;" {
        return Some(Ok(UserCommand::Show));
    }
    if let Some(rest) = trimmed.strip_prefix("CREATE USER") {
        let rest = rest.trim().trim_end_matches(';').trim();
        let Some((username, after_name)) = rest.split_once(char::is_whitespace) else {
            return Some(Err(CREATE_SYNTAX.into()));
        };
        let Some(password) = extract_password(query) else {
            return Some(Err(CREATE_SYNTAX.into()));
        };
        let role = match parse_role(after_name) {
            Some(Ok(r)) => r,
            Some(Err(e)) => return Some(Err(e)),
            None => Role::Reader,
        };
        return Some(Ok(UserCommand::Create {
            username: username.trim().to_lowercase(),
            password,
            role,
        }));
    }
    if let Some(rest) = trimmed.strip_prefix("DROP USER") {
        let username = rest.trim().trim_end_matches(';').trim().to_lowercase();
        if username.is_empty() {
            return Some(Err("DROP USER requires a username".into()));
        }
        return Some(Ok(UserCommand::Drop { username }));
    }
    if let Some(rest) = trimmed.strip_prefix("ALTER USER") {
        let rest = rest.trim().trim_end_matches(';').trim();
        let Some((username, after_name)) = rest.split_once(char::is_whitespace) else {
            return Some(Err(ALTER_SYNTAX.into()));
        };
        let password = extract_password(query);
        let role = match parse_role(after_name) {
            Some(Ok(r)) => Some(r),
            Some(Err(e)) => return Some(Err(e)),
            None => None,
        };
        if password.is_none() && role.is_none() {
            return Some(Err(
                "ALTER USER requires SET PASSWORD and/or SET ROLE".into()
            ));
        }
        return Some(Ok(UserCommand::Alter {
            username: username.trim().to_lowercase(),
            password,
            role,
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Catalog bytes exactly as 0.8.2 wrote them (bincode 2, no header): users `admin` (Admin) and
    /// `bob` (Reader). Old tvdb files must keep their users across the upgrade.
    const LEGACY_0_8_2: &str = "020561646d696e0561646d696e43243262243132246162636465666768696a6b6c6d6e6f707172737475754142434445464748494a4b4c4d4e4f505152535455565758595a3031323334353637383961620203626f6203626f624324326224313224303132333435363738396162636465666768696a6b754142434445464748494a4b4c4d4e4f505152535455565758595a30313233343536373839636400";

    #[test]
    fn legacy_catalog_from_0_8_2_loads_and_is_rewritten_framed() {
        let cat = AuthCatalog::deserialize(&unhex(LEGACY_0_8_2)).expect("legacy decode");
        assert_eq!(
            cat.list_users(),
            vec![("admin", Role::Admin), ("bob", Role::Reader)]
        );
        assert!(cat.users["admin"].password_hash.starts_with("$2b$12$"));

        let bytes = cat.serialize();
        assert!(traverse_codec::is_framed(&bytes));
        assert_eq!(
            traverse_codec::kind_of(&bytes),
            Some(traverse_codec::Kind::AuthCatalog)
        );
        let again = AuthCatalog::deserialize(&bytes).unwrap();
        assert_eq!(again.list_users(), cat.list_users());
        assert_eq!(
            again.users["bob"].password_hash,
            cat.users["bob"].password_hash
        );
    }

    #[test]
    fn round_trip_keeps_passwords_verifiable() {
        let mut cat = AuthCatalog::new();
        cat.add_user("alice", "s3cret", Role::Editor).unwrap();
        let back = AuthCatalog::deserialize(&cat.serialize()).unwrap();
        assert_eq!(back.list_users(), vec![("alice", Role::Editor)]);
        let (hash, role) = back.credentials("alice").unwrap();
        assert_eq!(role, Role::Editor);
        assert!(password_matches("s3cret", &hash));
        assert!(!password_matches("wrong", &hash));
        assert!(back.credentials("bob").is_none());
    }

    #[test]
    fn changes_apply_once_and_bump_the_generation() {
        let mut cat = AuthCatalog::new();
        let add = UserChange::add("alice", "pw", Role::Reader).unwrap();
        cat.apply(&add).unwrap();
        assert_eq!(cat.apply(&add), Err(UserError::Exists("alice".into())));
        let g = cat.generation();
        cat.apply(&UserChange::alter("alice", Some("pw2"), Some(Role::Admin)).unwrap())
            .unwrap();
        assert!(cat.generation() > g);
        let (hash, role) = cat.credentials("alice").unwrap();
        assert!(password_matches("pw2", &hash) && role == Role::Admin);
        cat.apply(&UserChange::drop("alice")).unwrap();
        assert_eq!(
            cat.apply(&UserChange::drop("alice")),
            Err(UserError::NotFound("alice".into()))
        );
    }

    #[test]
    fn user_commands_parse() {
        let ok = |q: &str| parse_user_command(q).unwrap().unwrap();
        assert_eq!(ok("SHOW USERS;"), UserCommand::Show);
        assert_eq!(
            ok("CREATE USER Bob SET PASSWORD 'MiXed Case' SET ROLE editor"),
            UserCommand::Create {
                username: "bob".into(),
                password: "MiXed Case".into(),
                role: Role::Editor
            }
        );
        assert_eq!(
            ok("create user bob set password 'x'"),
            UserCommand::Create {
                username: "bob".into(),
                password: "x".into(),
                role: Role::Reader
            }
        );
        assert_eq!(
            ok("ALTER USER bob SET ROLE ADMIN SET PASSWORD 'Y'"),
            UserCommand::Alter {
                username: "bob".into(),
                password: Some("Y".into()),
                role: Some(Role::Admin)
            }
        );
        assert_eq!(
            ok("DROP USER Bob;"),
            UserCommand::Drop {
                username: "bob".into()
            }
        );
        for bad in [
            "CREATE USER bob",
            "CREATE USER bob SET PASSWORD 'x' SET ROLE king",
            "ALTER USER bob",
            "DROP USER",
        ] {
            assert!(parse_user_command(bad).unwrap().is_err(), "{bad}");
        }
        assert!(parse_user_command("MATCH (n) RETURN n").is_none());
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn verified_credentials_go_stale_with_any_change() {
        let cache = VerifiedCache::default();
        cache.insert("alice", "pw", 3, Role::Editor);
        assert_eq!(cache.get("alice", "pw", 3), Some(Role::Editor));
        assert_eq!(cache.get("alice", "other", 3), None);
        assert_eq!(cache.get("alice", "pw", 4), None);
    }

    #[test]
    fn corrupted_catalog_is_rejected_not_misread() {
        let mut cat = AuthCatalog::new();
        cat.add_user("alice", "s3cret", Role::Admin).unwrap();
        let mut bytes = cat.serialize();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        let err = AuthCatalog::deserialize(&bytes).unwrap_err();
        assert!(err.contains("CRC-32"), "{err}");

        // a `.tvmodel` given to the catalog loader is refused by its kind byte before any parsing
        let model_blob = traverse_codec::encode(traverse_codec::Kind::Model, &cat).unwrap();
        let err = AuthCatalog::deserialize(&model_blob).unwrap_err();
        assert!(err.contains("model (.tvmodel)"), "{err}");
    }
}
