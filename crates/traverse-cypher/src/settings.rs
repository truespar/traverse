//! Process-wide settings registry behind `SHOW SETTINGS`. The embedded engine has no config file;
//! settings come from the host. The server registers its resolved config at startup (secrets
//! redacted); embedded users can register their own. `SHOW SETTINGS` lists them plus a few engine
//! built-ins in Neo4j's column shape (`name`, `value`, `isDynamic`, `defaultValue`, `description`).

use std::sync::{Mutex, OnceLock};

/// A registered setting.
#[derive(Clone, Debug)]
pub struct Setting {
    /// Name (e.g. `server.bolt.listen_address`).
    pub name: String,
    /// Resolved value (redacted by the registrar if secret).
    pub value: String,
    /// Default when unset.
    pub default_value: String,
    /// Human-readable description.
    pub description: String,
    /// Can it change without a restart?
    pub is_dynamic: bool,
}

fn registry() -> &'static Mutex<Vec<Setting>> {
    static REGISTRY: OnceLock<Mutex<Vec<Setting>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers (or updates, by name) a setting for `SHOW SETTINGS`. The CALLER must redact secrets;
/// values are stored and shown verbatim.
pub fn register_setting(
    name: &str,
    value: &str,
    default_value: &str,
    description: &str,
    is_dynamic: bool,
) {
    let mut reg = registry().lock().unwrap();
    let entry = Setting {
        name: name.to_string(),
        value: value.to_string(),
        default_value: default_value.to_string(),
        description: description.to_string(),
        is_dynamic,
    };
    match reg.iter_mut().find(|s| s.name == name) {
        Some(slot) => *slot = entry,
        None => reg.push(entry),
    }
}

/// Registered settings plus engine built-ins, sorted by name.
pub fn all_settings() -> Vec<Setting> {
    let mut out: Vec<Setting> = registry().lock().unwrap().clone();
    // engine built-ins, always there so `SHOW SETTINGS` is never empty, even embedded; registered
    // names win
    let builtins = [
        Setting {
            name: "internal.cypher.dialect".to_string(),
            value: "opencypher+cypher25".to_string(),
            default_value: "opencypher+cypher25".to_string(),
            description: "Query dialects accepted by this engine (openCypher \
                          with Cypher 25 extensions; ISO GQL via the GQL endpoint)."
                .to_string(),
            is_dynamic: false,
        },
        Setting {
            name: "internal.engine.version".to_string(),
            value: env!("CARGO_PKG_VERSION").to_string(),
            default_value: env!("CARGO_PKG_VERSION").to_string(),
            description: "Traverse engine version.".to_string(),
            is_dynamic: false,
        },
    ];
    for b in builtins {
        if !out.iter().any(|s| s.name == b.name) {
            out.push(b);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_upserts_and_lists_sorted() {
        register_setting("z.test.two", "2", "0", "second", false);
        register_setting("a.test.one", "1", "0", "first", true);
        register_setting("a.test.one", "1b", "0", "first-updated", true);
        let all = all_settings();
        let one = all.iter().find(|s| s.name == "a.test.one").unwrap();
        assert_eq!(one.value, "1b");
        assert!(one.is_dynamic);
        // sorted by name
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        // built-ins present
        assert!(all.iter().any(|s| s.name == "internal.engine.version"));
    }
}
