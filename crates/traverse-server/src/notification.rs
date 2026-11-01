//! Query notification analysis, shared by Bolt and gRPC.

use crate::sync::CacheLock as _;
use traverse_core::tx::{GraphView, MvccStore};

/// Notification severity, least to most severe. derive(Ord) follows declaration order (Information
/// < Warning), so `severity >= min_severity` filters correctly: min Warning passes only Warning,
/// min Information passes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Information,
    Warning,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warning => "WARNING",
            Self::Information => "INFORMATION",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "WARNING" => Some(Self::Warning),
            "INFORMATION" => Some(Self::Information),
            "OFF" => None,            // OFF disables all
            _ => Some(Self::Warning), // default
        }
    }
}

/// A query notification for the driver.
#[derive(Debug, Clone)]
pub struct Notification {
    pub code: &'static str,
    pub title: &'static str,
    pub description: String,
    pub severity: Severity,
    pub category: &'static str,
    /// Source position (offset, line, column), if any.
    pub position: Option<(u32, u32, u32)>,
}

/// Case-insensitive rfind of an ASCII keyword in a byte slice.
fn rfind_keyword_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let nlen = needle.len();
    if haystack.len() < nlen {
        return None;
    }
    for i in (0..=haystack.len() - nlen).rev() {
        if haystack[i..i + nlen].eq_ignore_ascii_case(needle) {
            return Some(i);
        }
    }
    None
}

/// Notifications for a query (performance hints, unknown labels, ...).
pub fn analyze_query_notifications(
    query: &str,
    store: &MvccStore,
    min_severity: Option<Severity>,
    disabled_categories: &[String],
) -> Vec<Notification> {
    let Some(min_sev) = min_severity else {
        return vec![]; // notifications disabled
    };
    // notifications are advisory and identical for every run of the same text, so memoize per
    // worker thread by (text, severity, disabled). Store-dependent checks (unknown labels) can go
    // stale until the memo rotates, acceptable for hints and bounded by the cap. L1 (thread-local)
    // keeps the steady-state hit free of shared state; L2 (global) turns a cold-thread miss after
    // tokio task migration into a read-lock lookup, which matters since the cartesian check parses
    // the query
    thread_local! {
        static MEMO: std::cell::RefCell<
            std::collections::HashMap<String, (u8, String, Vec<Notification>)>,
        > = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    static SHARED: std::sync::OnceLock<
        std::sync::RwLock<std::collections::HashMap<String, (u8, String, Vec<Notification>)>>,
    > = std::sync::OnceLock::new();
    // borrowed-key probe, no allocation on a hit; settings are checked against the stored entry
    // (they rarely change)
    let sev_tag = min_sev as u8;
    let settings_match = |s: &u8, dis: &str| {
        *s == sev_tag
            && disabled_categories.len() == dis.split(',').filter(|x| !x.is_empty()).count()
            && disabled_categories
                .iter()
                .all(|c| dis.split(',').any(|x| x == c))
    };
    let hit = MEMO.with(|m| {
        m.borrow()
            .get(query)
            .and_then(|(s, dis, v)| settings_match(s, dis).then(|| v.clone()))
    });
    if let Some(hit) = hit {
        return hit;
    }
    let shared = SHARED.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    let cached = shared
        .cache_read()
        .get(query)
        .and_then(|(s, dis, v)| settings_match(s, dis).then(|| v.clone()));
    let computed = match cached {
        Some(v) => v,
        None => {
            let computed =
                analyze_query_notifications_uncached(query, store, min_sev, disabled_categories);
            let mut m = shared.cache_write();
            if m.len() >= 1024 {
                m.clear();
            }
            m.insert(
                query.to_string(),
                (sev_tag, disabled_categories.join(","), computed.clone()),
            );
            computed
        }
    };
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= 1024 {
            m.clear();
        }
        m.insert(
            query.to_string(),
            (sev_tag, disabled_categories.join(","), computed.clone()),
        );
    });
    computed
}

/// Disconnected pattern components across the top-level MATCH / OPTIONAL MATCH clauses (max over
/// UNION branches). Parts connect when they share a variable (node, relationship or path), also
/// through bare `WITH v AS alias` renames. Subquery expressions aren't descended into: correlated
/// subqueries take the outer row as an argument and aren't cartesian branches (Neo4j doesn't warn
/// on them either). 0 if the query doesn't parse or has no MATCH.
fn disconnected_pattern_parts(query: &str) -> usize {
    use std::collections::HashMap;
    use traverse_cypher::ast::{Clause, Expr, PatternPart, ProjectionItems, Query, SingleQuery};

    fn part_vars(part: &PatternPart) -> Vec<String> {
        let mut vars = Vec::new();
        if let Some(v) = &part.variable {
            vars.push(v.clone());
        }
        if let Some(v) = &part.element.node.variable {
            vars.push(v.clone());
        }
        for ch in &part.element.chain {
            if let Some(v) = &ch.rel.variable {
                vars.push(v.clone());
            }
            if let Some(v) = &ch.node.variable {
                vars.push(v.clone());
            }
        }
        vars
    }

    // tiny union-find over variable names
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }

    fn analyze_single(sq: &SingleQuery) -> usize {
        let mut var_ids: HashMap<String, usize> = HashMap::new();
        let mut parent: Vec<usize> = Vec::new();
        let id_of = |name: &str, parent: &mut Vec<usize>, ids: &mut HashMap<String, usize>| {
            *ids.entry(name.to_string()).or_insert_with(|| {
                parent.push(parent.len());
                parent.len() - 1
            })
        };

        // each pattern part is a union-find node joined by all its variables; anonymous parts stay
        // singletons
        let mut part_nodes: Vec<usize> = Vec::new();
        let mut clause_lists: Vec<&[Clause]> = Vec::new();
        for p in &sq.parts {
            clause_lists.push(&p.clauses);
        }
        clause_lists.push(&sq.final_part.clauses);

        for clauses in &clause_lists {
            for c in *clauses {
                let mc = match c {
                    Clause::Match(m) | Clause::OptionalMatch(m) => m,
                    _ => continue,
                };
                for part in &mc.pattern.parts {
                    parent.push(parent.len());
                    let pnode = parent.len() - 1;
                    part_nodes.push(pnode);
                    for v in part_vars(part) {
                        let vid = id_of(&v, &mut parent, &mut var_ids);
                        union(&mut parent, pnode, vid);
                    }
                }
            }
        }

        // `WITH v AS alias` keeps the row wired through the rename
        for p in &sq.parts {
            if let ProjectionItems::Explicit(items) = &p.with.items {
                for item in items {
                    if let (Expr::Variable(v), Some(alias)) = (&item.expr, &item.alias) {
                        let a = id_of(v, &mut parent, &mut var_ids);
                        let b = id_of(alias, &mut parent, &mut var_ids);
                        union(&mut parent, a, b);
                    }
                }
            }
        }

        if part_nodes.len() < 2 {
            return part_nodes.len();
        }
        let mut roots: Vec<usize> = part_nodes
            .iter()
            .map(|&p| {
                let mut parent = parent.clone();
                find(&mut parent, p)
            })
            .collect();
        roots.sort_unstable();
        roots.dedup();
        roots.len()
    }

    let Ok(stmt) = traverse_cypher::parse(query) else {
        return 0;
    };
    let Query::Regular(rq) = &stmt.query else {
        return 0;
    };
    let mut worst = analyze_single(&rq.single);
    for u in &rq.unions {
        worst = worst.max(analyze_single(&u.query));
    }
    worst
}

fn analyze_query_notifications_uncached(
    query: &str,
    store: &MvccStore,
    min_sev: Severity,
    disabled_categories: &[String],
) -> Vec<Notification> {
    let mut notifications = Vec::new();
    let bytes = query.as_bytes();

    // cartesian products, detected on the AST: counting "MATCH" keywords would false-positive on
    // OPTIONAL MATCH, WITH...MATCH chains and correlated COUNT/COLLECT/EXISTS subqueries, and every
    // such response would carry a notification the drivers materialize and log
    let effective_disconnected = disconnected_pattern_parts(query);
    if effective_disconnected >= 2 && !disabled_categories.contains(&"PERFORMANCE".to_string()) {
        let note = Notification {
            code: "Neo.ClientNotification.Statement.CartesianProductWarning",
            title: "This query builds a cartesian product between disconnected patterns.",
            description: format!(
                "If a part of a query contains multiple disconnected patterns, \
                 this will build a cartesian product between all those parts. \
                 This may produce a large amount of data ({effective_disconnected} disconnected patterns). \
                 Consider adding a relationship or WHERE clause to connect the patterns."
            ),
            severity: Severity::Warning,
            category: "PERFORMANCE",
            position: None,
        };
        if note.severity >= min_sev {
            notifications.push(note);
        }
    }

    // unknown labels: MATCH labels missing from the graph; skipped entirely when the query has no
    // ':'
    let has_colon = bytes.contains(&b':');
    if has_colon && !disabled_categories.contains(&"HINT".to_string()) {
        let view = store.unregistered_read();
        let strings = GraphView::strings(&view);

        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b':' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                let label = &query[start..end];
                let before = &bytes[..i];
                let last_kw = rfind_keyword_ci(before, b"MATCH")
                    .or_else(|| rfind_keyword_ci(before, b"WHERE"))
                    .or_else(|| rfind_keyword_ci(before, b"OPTIONAL"));
                let is_match_context = last_kw.is_some()
                    && rfind_keyword_ci(before, b"CREATE").is_none_or(|c| c < last_kw.unwrap())
                    && rfind_keyword_ci(before, b"MERGE").is_none_or(|m| m < last_kw.unwrap());

                if is_match_context && strings.get(label).is_none() {
                    let note = Notification {
                        code: "Neo.ClientNotification.Statement.UnknownLabelWarning",
                        title: "The provided label is not in the database.",
                        description: format!(
                            "One of the labels in your query is not available in the database, \
                             make sure you didn't misspell it or that the label is available when \
                             you run this statement in your application (the missing label name is: {label})"
                        ),
                        severity: Severity::Warning,
                        category: "HINT",
                        position: None,
                    };
                    if note.severity >= min_sev
                        && !notifications.iter().any(|n| n.description.contains(label))
                    {
                        notifications.push(note);
                    }
                }
                i = end;
            } else {
                i += 1;
            }
        }
    }

    // unbounded variable-length pattern: -[*]->
    if bytes.windows(3).any(|w| w == b"[*]")
        && !disabled_categories.contains(&"PERFORMANCE".to_string())
    {
        let note = Notification {
            code: "Neo.ClientNotification.Statement.UnboundedVariableLengthPatternWarning",
            title: "The provided pattern is unbounded, consider adding an upper limit to the number of node hops.",
            description:
                "Using an unbounded variable-length pattern may lead to long-running queries \
                 as the number of hops grows. Consider adding an upper bound, e.g. [*..10]."
                    .into(),
            severity: Severity::Warning,
            category: "PERFORMANCE",
            position: None,
        };
        if note.severity >= min_sev {
            notifications.push(note);
        }
    }

    notifications
}
