//! Patterns: nodes, relationships, quantified path patterns and label expressions.

use crate::ast::{
    BinOp, CmpOp, Expr, Literal, NodePattern, Pattern, PatternChain, PatternElement, PatternPart,
    RelDirection, RelPattern, ShortestPathMode, UnaryOp,
};
use crate::error::CypherError;
use crate::lexer::TokenKind;

use super::{Parser, is_keyword_as_ident};

/// Parse-time label/type expression tree. Never reaches the public AST: complex shapes lower to
/// predicates before the pattern is returned.
#[derive(Debug, Clone)]
pub(super) enum LabelTerm {
    Leaf(String),
    Dynamic(Expr),
    Wildcard,
    And(Box<LabelTerm>, Box<LabelTerm>),
    Or(Box<LabelTerm>, Box<LabelTerm>),
    Not(Box<LabelTerm>),
}

impl LabelTerm {
    /// Flattens a pure conjunction of leaves (`A&B`, `A:B`) into `NodePattern::labels` /
    /// `dynamic_labels`: index-friendly, and the only shape CREATE/MERGE accept.
    fn as_pure_conjunction(&self) -> Option<(Vec<String>, Vec<Expr>)> {
        #[recursive::recursive]
        fn walk(t: &LabelTerm, names: &mut Vec<String>, dyns: &mut Vec<Expr>) -> bool {
            match t {
                LabelTerm::Leaf(l) => {
                    names.push(l.clone());
                    true
                }
                LabelTerm::Dynamic(e) => {
                    dyns.push(e.clone());
                    true
                }
                LabelTerm::And(a, b) => walk(a, names, dyns) && walk(b, names, dyns),
                _ => false,
            }
        }
        let (mut names, mut dyns) = (Vec::new(), Vec::new());
        walk(self, &mut names, &mut dyns).then_some((names, dyns))
    }

    /// Flattens a pure disjunction of leaves (`A|B`) into `RelPattern::types` / `dynamic_types`
    /// (the engine's native OR).
    fn as_pure_disjunction(&self) -> Option<(Vec<String>, Vec<Expr>)> {
        #[recursive::recursive]
        fn walk(t: &LabelTerm, names: &mut Vec<String>, dyns: &mut Vec<Expr>) -> bool {
            match t {
                LabelTerm::Leaf(l) => {
                    names.push(l.clone());
                    true
                }
                LabelTerm::Dynamic(e) => {
                    dyns.push(e.clone());
                    true
                }
                LabelTerm::Or(a, b) => walk(a, names, dyns) && walk(b, names, dyns),
                _ => false,
            }
        }
        let (mut names, mut dyns) = (Vec::new(), Vec::new());
        walk(self, &mut names, &mut dyns)
            .then_some((names, dyns))
            .map(|(mut names, dyns)| {
                // `[:T|:T]` matches each edge once: a repeated name is one predicate, not two scans
                let mut seen = std::collections::HashSet::new();
                names.retain(|n| seen.insert(n.clone()));
                (names, dyns)
            })
    }

    /// Lowers to a boolean predicate over a NODE variable.
    pub(super) fn node_pred(&self, var: &str) -> Expr {
        let v = || Expr::Variable(var.to_string());
        match self {
            LabelTerm::Leaf(l) => Expr::FunctionCall {
                name: "__has_label".to_string(),
                distinct: false,
                args: vec![v(), Expr::Literal(Literal::String(l.clone()))],
            },
            LabelTerm::Dynamic(e) => Expr::FunctionCall {
                name: "__has_label".to_string(),
                distinct: false,
                args: vec![v(), e.clone()],
            },
            // `%`: the node has some label
            LabelTerm::Wildcard => Expr::Comparison(
                Box::new(Expr::FunctionCall {
                    name: "size".to_string(),
                    distinct: false,
                    args: vec![Expr::FunctionCall {
                        name: "labels".to_string(),
                        distinct: false,
                        args: vec![v()],
                    }],
                }),
                vec![(CmpOp::Gt, Expr::Literal(Literal::Integer(0)))],
            ),
            LabelTerm::And(a, b) => Expr::BinaryOp(
                Box::new(a.node_pred(var)),
                BinOp::And,
                Box::new(b.node_pred(var)),
            ),
            LabelTerm::Or(a, b) => Expr::BinaryOp(
                Box::new(a.node_pred(var)),
                BinOp::Or,
                Box::new(b.node_pred(var)),
            ),
            LabelTerm::Not(a) => Expr::UnaryOp(UnaryOp::Not, Box::new(a.node_pred(var))),
        }
    }

    /// Lowers to a boolean predicate over a RELATIONSHIP variable, checked against its single type
    /// via `type(r)`.
    fn rel_pred(&self, var: &str) -> Expr {
        let type_of = || Expr::FunctionCall {
            name: "type".to_string(),
            distinct: false,
            args: vec![Expr::Variable(var.to_string())],
        };
        match self {
            LabelTerm::Leaf(t) => Expr::Comparison(
                Box::new(type_of()),
                vec![(CmpOp::Eq, Expr::Literal(Literal::String(t.clone())))],
            ),
            LabelTerm::Dynamic(e) => {
                Expr::Comparison(Box::new(type_of()), vec![(CmpOp::Eq, e.clone())])
            }
            // `%`: every relationship has a type
            LabelTerm::Wildcard => Expr::Literal(Literal::Bool(true)),
            LabelTerm::And(a, b) => Expr::BinaryOp(
                Box::new(a.rel_pred(var)),
                BinOp::And,
                Box::new(b.rel_pred(var)),
            ),
            LabelTerm::Or(a, b) => Expr::BinaryOp(
                Box::new(a.rel_pred(var)),
                BinOp::Or,
                Box::new(b.rel_pred(var)),
            ),
            LabelTerm::Not(a) => Expr::UnaryOp(UnaryOp::Not, Box::new(a.rel_pred(var))),
        }
    }

    /// Splits a top-level conjunction into pure leaf conjuncts and complex residue, so `:(A|B)&C`
    /// scans by C and filters A|B.
    fn split_conjuncts(self) -> (Vec<String>, Vec<Expr>, Vec<LabelTerm>) {
        #[recursive::recursive]
        fn walk(
            t: LabelTerm,
            names: &mut Vec<String>,
            dyns: &mut Vec<Expr>,
            rest: &mut Vec<LabelTerm>,
        ) {
            match t {
                LabelTerm::Leaf(l) => names.push(l),
                LabelTerm::Dynamic(e) => dyns.push(e),
                LabelTerm::And(a, b) => {
                    walk(*a, names, dyns, rest);
                    walk(*b, names, dyns, rest);
                }
                other => rest.push(other),
            }
        }
        let (mut names, mut dyns, mut rest) = (Vec::new(), Vec::new(), Vec::new());
        walk(self, &mut names, &mut dyns, &mut rest);
        (names, dyns, rest)
    }
}

impl Parser {
    pub(super) fn parse_pattern(&mut self) -> Result<Pattern, CypherError> {
        let mut parts = vec![self.parse_pattern_part()?];
        while self.eat(&TokenKind::Comma) {
            parts.push(self.parse_pattern_part()?);
        }
        Ok(Pattern { parts })
    }

    fn parse_pattern_part(&mut self) -> Result<PatternPart, CypherError> {
        // named path: `p = (a)-->(b)` or `p = shortestPath(...)`
        let variable = if matches!(self.peek(), TokenKind::Ident(_))
            && matches!(self.peek_offset(1), TokenKind::Eq)
        {
            let name = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Eq)?;
            Some(name)
        } else {
            None
        };

        // -- ISO section 14.6 path selectors (Cypher 25 / GQL) --
        //   ALL | ANY [k] | ANY SHORTEST | SHORTEST k | ALL SHORTEST |
        //   SHORTEST k GROUPS   (+ optional PATH / PATHS / GROUP noise words)
        // all lower to ShortestPathMode / path_selector_limit
        let mut selector: Option<ShortestPathMode> = None;
        let mut selector_limit: Option<usize> = None;
        let kw =
            |t: &TokenKind, w: &str| matches!(t, TokenKind::Ident(s) if s.eq_ignore_ascii_case(w));
        let eat_noise = |this: &mut Self| {
            if kw(this.peek(), "PATH") || kw(this.peek(), "PATHS") {
                this.advance();
            }
        };
        if kw(self.peek(), "SHORTEST") && matches!(self.peek_offset(1), TokenKind::Integer(_)) {
            self.advance();
            let k = match self.advance().kind.clone() {
                TokenKind::Integer(n) if n >= 1 => n as usize,
                _ => {
                    return Err(CypherError::syntax(
                        "SHORTEST k requires a positive integer".to_string(),
                        self.span_pos(),
                    ));
                }
            };
            if kw(self.peek(), "GROUP") || kw(self.peek(), "GROUPS") {
                self.advance();
                selector = Some(ShortestPathMode::CountedGroups(k));
            } else {
                eat_noise(self);
                selector = Some(ShortestPathMode::Counted(k));
            }
        } else if matches!(self.peek(), TokenKind::All | TokenKind::Ident(_))
            && (kw(self.peek(), "ALL") || *self.peek() == TokenKind::All)
            && kw(self.peek_offset(1), "SHORTEST")
        {
            self.advance();
            self.advance();
            eat_noise(self);
            selector = Some(ShortestPathMode::All);
        } else if (*self.peek() == TokenKind::All || kw(self.peek(), "ALL"))
            && matches!(self.peek_offset(1), TokenKind::LParen)
        {
            // explicit `ALL` is the default: consume, continue
            self.advance();
        } else if kw(self.peek(), "ANY") {
            if kw(self.peek_offset(1), "SHORTEST") {
                self.advance();
                self.advance();
                eat_noise(self);
                selector = Some(ShortestPathMode::Single);
            } else if let TokenKind::Integer(n) = self.peek_offset(1).clone() {
                self.advance();
                self.advance();
                eat_noise(self);
                if n < 1 {
                    return Err(CypherError::syntax(
                        "ANY k requires a positive integer".to_string(),
                        self.span_pos(),
                    ));
                }
                selector_limit = Some(n as usize);
            } else if matches!(self.peek_offset(1), TokenKind::LParen) {
                self.advance();
                selector_limit = Some(1);
            }
        }

        // -- ISO section 14.5 path mode prefix: WALK | TRAIL | ACYCLIC --
        let mut part_path_mode: Option<crate::ast::PathMode> = None;
        if matches!(self.peek_offset(1), TokenKind::LParen) {
            if kw(self.peek(), "WALK") {
                self.advance();
                part_path_mode = Some(crate::ast::PathMode::Walk);
            } else if kw(self.peek(), "TRAIL") {
                self.advance();
                part_path_mode = Some(crate::ast::PathMode::Trail);
            } else if kw(self.peek(), "ACYCLIC") {
                self.advance();
                part_path_mode = Some(crate::ast::PathMode::Acyclic);
            }
        }

        // shortestPath(...) / allShortestPaths(...)
        let mode = match self.peek() {
            TokenKind::Ident(name)
                if (name == "shortestPath" || name == "allShortestPaths")
                    && matches!(self.peek_offset(1), TokenKind::LParen) =>
            {
                let m = if name == "shortestPath" {
                    ShortestPathMode::Single
                } else {
                    ShortestPathMode::All
                };
                self.advance(); // consume the identifier
                self.advance(); // consume '('
                m
            }
            _ => ShortestPathMode::None,
        };
        if selector.is_some() && mode != ShortestPathMode::None {
            return Err(CypherError::syntax(
                "a path selector can't be combined with shortestPath()/allShortestPaths()"
                    .to_string(),
                self.span_pos(),
            ));
        }

        let element = self.parse_pattern_element()?;

        if mode != ShortestPathMode::None {
            // closing ')'
            self.expect(&TokenKind::RParen)?;
            // exactly one chain entry, with a variable-length range
            if element.chain.len() != 1 {
                return Err(CypherError::syntax(
                    "shortestPath requires exactly one relationship in the pattern".to_string(),
                    self.span_pos(),
                ));
            }
            if element.chain[0].rel.range.is_none() {
                return Err(CypherError::syntax(
                    "shortestPath requires a variable-length relationship (e.g. -[*]->)"
                        .to_string(),
                    self.span_pos(),
                ));
            }
        }

        // SHORTEST-family selectors use the same engine paths as shortestPath()/allShortestPaths()
        // and need the same shape (one variable-length/quantified relationship, as in every manual
        // example)
        if let Some(sel) = &selector
            && (element.chain.len() != 1 || element.chain[0].rel.range.is_none())
        {
            return Err(CypherError::syntax(
                format!(
                    "{} requires a single variable-length or quantified relationship (e.g. (a)-[:T]-+(b))",
                    match sel {
                        ShortestPathMode::All => "ALL SHORTEST",
                        ShortestPathMode::Single => "ANY SHORTEST",
                        ShortestPathMode::CountedGroups(_) => "SHORTEST k GROUPS",
                        _ => "SHORTEST k",
                    }
                ),
                self.span_pos(),
            ));
        }

        // *wShortest: a chain relationship with wshortest_weight promotes to WeightedSingle
        let (sp_mode, weight_prop, cost_var) = if let Some(sel) = selector {
            (sel, None, None) // ISO section 14.6 selector
        } else if mode != ShortestPathMode::None {
            (mode, None, None) // shortestPath / allShortestPaths
        } else if let Some(chain) = element.chain.first() {
            if let Some(ref w) = chain.rel.wshortest_weight {
                (
                    ShortestPathMode::WeightedSingle,
                    Some(w.clone()),
                    chain.rel.wshortest_cost_var.clone(),
                )
            } else {
                (ShortestPathMode::None, None, None)
            }
        } else {
            (ShortestPathMode::None, None, None)
        };

        // weighted patterns: exactly one chain, with a range
        if sp_mode == ShortestPathMode::WeightedSingle {
            if element.chain.len() != 1 {
                return Err(CypherError::syntax(
                    "wShortest requires exactly one relationship in the pattern".to_string(),
                    self.span_pos(),
                ));
            }
            if element.chain[0].rel.range.is_none() {
                return Err(CypherError::syntax(
                    "wShortest requires a variable-length relationship (e.g. -[*wShortest ...]->)"
                        .to_string(),
                    self.span_pos(),
                ));
            }
        }

        Ok(PatternPart {
            variable,
            element,
            shortest_path: sp_mode,
            weight_property: weight_prop,
            total_cost_var: cost_var,
            path_mode: part_path_mode.unwrap_or_default(),
            qpp_list_groups: Vec::new(),
            path_selector_limit: selector_limit,
        })
    }

    pub(super) fn parse_pattern_element(&mut self) -> Result<PatternElement, CypherError> {
        let node = self.parse_node_pattern()?;
        let mut chain = Vec::new();
        loop {
            if self.is_rel_start() {
                let rel = self.parse_rel_pattern()?;
                let next_node = self.parse_node_pattern()?;
                chain.push(PatternChain {
                    rel,
                    node: next_node,
                });
            } else if self.is_qpp_group_start() {
                chain.push(self.parse_qpp_group()?);
            } else {
                break;
            }
        }
        Ok(PatternElement { node, chain })
    }

    /// A Neo4j quantified-path-pattern group starts with `((`: an outer paren opening an inner node
    /// pattern.
    fn is_qpp_group_start(&self) -> bool {
        *self.peek() == TokenKind::LParen && *self.peek_offset(1) == TokenKind::LParen
    }

    /// Parses a single-relationship quantified group `((a)-[r:T]->(b)){m,n}` (also `+` / `*`) and
    /// lowers it to a var-length relationship between the boundary nodes. Multi-relationship groups
    /// and per-hop inner-node constraints are rejected, not mis-lowered.
    #[recursive::recursive]
    fn parse_qpp_group(&mut self) -> Result<PatternChain, CypherError> {
        self.enter_nesting()?;
        let result = self.parse_qpp_group_inner();
        self.leave_nesting();
        result
    }

    fn parse_qpp_group_inner(&mut self) -> Result<PatternChain, CypherError> {
        self.expect(&TokenKind::LParen)?; // outer (
        let inner_start = self.parse_node_pattern()?;
        if !self.is_rel_start() {
            return Err(CypherError::syntax(
                "a quantified path pattern group must contain a relationship, e.g. ((a)-[:R]->(b)){1,3}",
                self.span_pos(),
            ));
        }
        let mut rel = self.parse_rel_pattern()?;
        let inner_end = self.parse_node_pattern()?;
        if self.is_rel_start() {
            return Err(CypherError::syntax(
                "multi-relationship quantified path pattern groups are not supported in the Bolt \
                 dialect yet - use a single relationship, or the GQL endpoint",
                self.span_pos(),
            ));
        }
        self.expect(&TokenKind::RParen)?; // outer )
        let quant = self.parse_rel_quantifier()?.ok_or_else(|| {
            CypherError::syntax(
                "a quantified path pattern group must be followed by a quantifier ({m,n}, +, or *)",
                self.span_pos(),
            )
        })?;
        if rel.range.is_some() {
            return Err(CypherError::syntax(
                "a quantified path pattern group's relationship can't also carry a `*range`",
                self.span_pos(),
            ));
        }
        rel.range = Some(quant);
        // ISO section 16.11 / Neo4j group variables: inner node patterns ride on the lowered relationship;
        // their variables bind per-iteration LISTS and their label/property constraints hold on
        // every iteration (planner emits the list bindings + all() filters)
        if inner_start.variable.is_some()
            || inner_end.variable.is_some()
            || !inner_start.labels.is_empty()
            || !inner_end.labels.is_empty()
            || !inner_start.dynamic_labels.is_empty()
            || !inner_end.dynamic_labels.is_empty()
            || inner_start.properties.is_some()
            || inner_end.properties.is_some()
        {
            rel.qpp_group = Some(Box::new(crate::ast::QppGroupInner {
                start: inner_start,
                end: inner_end,
            }));
        }
        // the group ends at a following node (unified with its boundary)
        let node = self.parse_node_pattern()?;
        Ok(PatternChain { rel, node })
    }

    /// Label / relationship-type expression (Cypher 5+): `A&B`, `A|B`, `!A`, `%`,
    /// grouping, dynamic `$param` / `$(expr)` leaves. Pure conjunctions/disjunctions
    /// keep the dedicated pattern fields; anything else lowers AT PARSE TIME to a
    /// boolean predicate on the element's variable via the pattern-internal WHERE
    /// machinery (deferred placement, per-element `all()` on var-length rels,
    /// per-iteration on QPP groups).
    #[recursive::recursive]
    pub(super) fn parse_label_expression(&mut self) -> Result<LabelTerm, CypherError> {
        self.enter_nesting()?;
        let result = self.parse_label_expression_inner();
        self.leave_nesting();
        result
    }

    fn parse_label_expression_inner(&mut self) -> Result<LabelTerm, CypherError> {
        // disjunction (lowest precedence); legacy `|:` accepted
        let mut left = self.parse_label_term_and()?;
        while self.eat(&TokenKind::Pipe) {
            self.eat(&TokenKind::Colon); // legacy `:A|:B`
            let right = self.parse_label_term_and()?;
            left = LabelTerm::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_label_term_and(&mut self) -> Result<LabelTerm, CypherError> {
        // conjunction: `&` (Cypher 5+) and `:` (legacy `:A:B`)
        let mut left = self.parse_label_term_not()?;
        loop {
            if self.eat(&TokenKind::Amp) {
                let right = self.parse_label_term_not()?;
                left = LabelTerm::And(Box::new(left), Box::new(right));
            } else if *self.peek() == TokenKind::Colon
                && !matches!(self.peek_offset(1), TokenKind::RParen | TokenKind::RBracket)
            {
                self.advance();
                let right = self.parse_label_term_not()?;
                left = LabelTerm::And(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_label_term_not(&mut self) -> Result<LabelTerm, CypherError> {
        // `!!!A`: count bangs iteratively, wrap inside out
        let mut bangs = 0usize;
        while self.eat(&TokenKind::Bang) {
            bangs = self.count_chain_term(bangs)?;
        }
        if bangs > 0 {
            let mut term = self.parse_label_term_not()?;
            for _ in 0..bangs {
                term = LabelTerm::Not(Box::new(term));
            }
            return Ok(term);
        }
        match self.peek().clone() {
            TokenKind::Percent => {
                self.advance();
                Ok(LabelTerm::Wildcard)
            }
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_label_expression()?;
                self.expect(&TokenKind::RParen)?;
                Ok(inner)
            }
            TokenKind::Parameter(name) => {
                self.advance();
                Ok(LabelTerm::Dynamic(Expr::Parameter(name)))
            }
            TokenKind::DollarLParen => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;
                Ok(LabelTerm::Dynamic(e))
            }
            _ => Ok(LabelTerm::Leaf(self.parse_symbolic_name()?)),
        }
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, CypherError> {
        self.expect(&TokenKind::LParen)?;

        let variable = if matches!(self.peek(), TokenKind::Ident(_))
            && !matches!(self.peek_offset(1), TokenKind::Colon if matches!(self.peek(), TokenKind::Ident(_)))
            || matches!(self.peek(), TokenKind::Ident(_))
        {
            // identifier followed by : or ) or { is a variable
            match self.peek_offset(1) {
                TokenKind::Colon | TokenKind::RParen | TokenKind::LBrace => {
                    Some(self.parse_symbolic_name()?)
                }
                _ => {
                    // may be a keyword used as a variable
                    if matches!(self.peek(), TokenKind::Ident(_)) {
                        Some(self.parse_symbolic_name()?)
                    } else {
                        None
                    }
                }
            }
        } else if is_keyword_as_ident(self.peek()) {
            match self.peek_offset(1) {
                TokenKind::Colon | TokenKind::RParen | TokenKind::LBrace => {
                    Some(self.parse_symbolic_name()?)
                }
                _ => None,
            }
        } else {
            None
        };

        let mut variable = variable;
        let mut labels = Vec::new();
        let mut dynamic_labels = Vec::new();
        // full label expression (Cypher 5+): `&`, `|`, `!`, `%`, grouping, legacy `:A:B`, dynamic
        // `$param` / `$(expr)`. Pure conjunctions fill `labels`/`dynamic_labels` (index-friendly,
        // the only shape CREATE/MERGE accept); complex residue becomes a pattern-internal predicate
        let mut label_where: Option<Expr> = None;
        if self.eat(&TokenKind::Colon) {
            let term = self.parse_label_expression()?;
            if let Some((ls, ds)) = term.as_pure_conjunction() {
                labels = ls;
                dynamic_labels = ds;
            } else {
                let (ls, ds, rest) = term.split_conjuncts();
                labels = ls;
                dynamic_labels = ds;
                let var = variable
                    .get_or_insert_with(|| {
                        self.fresh += 1;
                        format!("__lblx_{}", self.fresh)
                    })
                    .clone();
                for t in rest {
                    let pred = t.node_pred(&var);
                    label_where = Some(match label_where.take() {
                        None => pred,
                        Some(acc) => Expr::BinaryOp(Box::new(acc), BinOp::And, Box::new(pred)),
                    });
                }
            }
        }

        let properties = if *self.peek() == TokenKind::LBrace {
            Some(self.parse_map_literal_expr()?)
        } else {
            None
        };

        // pattern-internal predicate `(n:L {...} WHERE pred)` (Cypher 5+)
        let mut where_pred = if self.eat_keyword(&TokenKind::Where) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        // complex label residue joins that predicate
        if let Some(lp) = label_where {
            where_pred = Some(match where_pred {
                None => Box::new(lp),
                Some(wp) => Box::new(Expr::BinaryOp(Box::new(lp), BinOp::And, wp)),
            });
        }

        self.expect(&TokenKind::RParen)?;
        Ok(NodePattern {
            variable,
            labels,
            dynamic_labels,
            properties,
            where_pred,
        })
    }

    /// Does the current token start a relationship pattern?
    pub(super) fn is_rel_start(&self) -> bool {
        match self.peek() {
            TokenKind::Minus => {
                // `-[` or `->` start a relationship; `- 2` / `- n` are subtraction
                matches!(
                    self.peek_offset(1),
                    TokenKind::LBracket | TokenKind::Gt | TokenKind::Minus
                )
            }
            TokenKind::Lt => {
                // <-
                matches!(self.peek_offset(1), TokenKind::Minus)
            }
            _ => false,
        }
    }

    fn parse_rel_pattern(&mut self) -> Result<RelPattern, CypherError> {
        let mut direction;

        // leading direction
        if self.eat(&TokenKind::Lt) {
            // <-
            self.expect(&TokenKind::Minus)?;
            direction = RelDirection::Incoming;
        } else {
            self.expect(&TokenKind::Minus)?;
            direction = RelDirection::Both; // refined below
        }

        // optional [details]
        let (variable, types, dynamic_types, range, properties, ws_weight, ws_cost_var, where_pred) =
            if self.eat(&TokenKind::LBracket) {
                let var = if matches!(self.peek(), TokenKind::Ident(_)) {
                    Some(self.parse_symbolic_name()?)
                } else if is_keyword_as_ident(self.peek())
                    && !matches!(self.peek(), TokenKind::Star)
                {
                    match self.peek_offset(1) {
                        TokenKind::Colon
                        | TokenKind::RBracket
                        | TokenKind::Star
                        | TokenKind::LBrace
                        | TokenKind::Pipe => Some(self.parse_symbolic_name()?),
                        _ => None,
                    }
                } else {
                    None
                };

                let mut var = var;
                let mut rel_types = Vec::new();
                let mut rel_dynamic_types: Vec<Expr> = Vec::new();
                // full relationship-type expression (Cypher 5+): pure disjunctions (incl. dynamic
                // `$param` / `$(expr)`) fill `types`/`dynamic_types` (the native OR); complex
                // shapes (`!`, `&`, `%`, grouping) lower to a predicate on `type(r)` via the
                // pattern-internal WHERE (per element on var-length rels)
                let mut type_where: Option<Expr> = None;
                if self.eat(&TokenKind::Colon) {
                    let term = self.parse_label_expression()?;
                    if let Some((ts, ds)) = term.as_pure_disjunction() {
                        rel_types = ts;
                        rel_dynamic_types = ds;
                    } else {
                        let v = var
                            .get_or_insert_with(|| {
                                self.fresh += 1;
                                format!("__typx_{}", self.fresh)
                            })
                            .clone();
                        type_where = Some(term.rel_pred(&v));
                    }
                }

                let (range, ws_weight, ws_cost_var) = if self.eat(&TokenKind::Star) {
                    // *wShortest identifier?
                    if let TokenKind::Ident(ref name) = self.peek().clone() {
                        if name == "wShortest" {
                            self.advance(); // "wShortest"
                            let (weight_prop, cost_var) = self.parse_wshortest_lambda()?;
                            (Some((None, None)), Some(weight_prop), cost_var)
                        } else {
                            (self.parse_range()?, None, None)
                        }
                    } else {
                        (self.parse_range()?, None, None)
                    }
                } else {
                    (None, None, None)
                };

                let props = if *self.peek() == TokenKind::LBrace {
                    Some(self.parse_map_literal_expr()?)
                } else {
                    None
                };

                // pattern-internal predicate `[r:T {...} WHERE pred]` (Cypher 5+)
                let mut where_pred = if self.eat_keyword(&TokenKind::Where) {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                // complex type residue joins that predicate
                if let Some(tp) = type_where {
                    where_pred = Some(match where_pred {
                        None => Box::new(tp),
                        Some(wp) => Box::new(Expr::BinaryOp(Box::new(tp), BinOp::And, wp)),
                    });
                }

                self.expect(&TokenKind::RBracket)?;
                (
                    var,
                    rel_types,
                    rel_dynamic_types,
                    range,
                    props,
                    ws_weight,
                    ws_cost_var,
                    where_pred,
                )
            } else {
                (None, Vec::new(), Vec::new(), None, None, None, None, None)
            };

        // trailing direction ->
        self.expect(&TokenKind::Minus)?;
        if self.eat(&TokenKind::Gt) {
            if direction == RelDirection::Incoming {
                // <-..-> is invalid; treated as Both
                direction = RelDirection::Both;
            } else {
                direction = RelDirection::Outgoing;
            }
        }
        // no `>`: direction stays (Incoming or Both)

        // QPP quantifier on the relationship (`-[r:T]->{m,n}` / `->+` / `->*`): same connectivity
        // and edge-list binding as var-length, so fold it into the range
        let range = match self.parse_rel_quantifier()? {
            Some(q) => {
                if range.is_some() {
                    return Err(CypherError::syntax(
                        "a relationship can't have both a `*range` and a `{quantifier}`",
                        self.span_pos(),
                    ));
                }
                Some(q)
            }
            None => range,
        };

        Ok(RelPattern {
            variable,
            direction,
            types,
            dynamic_types,
            range,
            properties,
            wshortest_weight: ws_weight,
            wshortest_cost_var: ws_cost_var,
            where_pred,
            qpp_group: None,
        })
    }

    /// QPP quantifier right after a relationship (`{n}` / `{m,n}` / `{m,}` / `{,n}` / `+` / `*`) as
    /// a var-length `(min, max)`; `None` without one.
    fn parse_rel_quantifier(&mut self) -> Result<Option<(Option<u32>, Option<u32>)>, CypherError> {
        match self.peek() {
            // `+`: one or more
            TokenKind::Plus => {
                self.advance();
                Ok(Some((Some(1), None)))
            }
            // `*`: zero or more (QPP)
            TokenKind::Star => {
                self.advance();
                Ok(Some((Some(0), None)))
            }
            // `{ ... }` range
            TokenKind::LBrace => {
                self.advance(); // {
                let min = if let TokenKind::Integer(n) = self.peek() {
                    let n = *n as u32;
                    self.advance();
                    Some(n)
                } else {
                    None
                };
                let range = if self.eat(&TokenKind::Comma) {
                    let max = if let TokenKind::Integer(n) = self.peek() {
                        let n = *n as u32;
                        self.advance();
                        Some(n)
                    } else {
                        None
                    };
                    // `{,n}` -> min 0; `{m,}` -> max unbounded
                    (min.or(Some(0)), max)
                } else {
                    // `{n}` -> exactly n
                    (min, min)
                };
                self.expect(&TokenKind::RBrace)?;
                Ok(Some(range))
            }
            _ => Ok(None),
        }
    }

    /// wShortest lambda: `(r, n | r.property) [cost_var]`.
    fn parse_wshortest_lambda(&mut self) -> Result<(String, Option<String>), CypherError> {
        self.expect(&TokenKind::LParen)?;
        // edge var (ignored, already the rel variable)
        let _edge_var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Comma)?;
        // node var (accepted, ignored)
        let _node_var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Pipe)?;
        // `var.prop`: the weight
        let _weight_var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Dot)?;
        let weight_prop = self.parse_symbolic_name()?;
        self.expect(&TokenKind::RParen)?;
        // identifier before `]` = total-cost variable
        let cost_var = if matches!(self.peek(), TokenKind::Ident(_)) {
            Some(self.parse_symbolic_name()?)
        } else {
            None
        };
        Ok((weight_prop, cost_var))
    }

    fn parse_range(&mut self) -> Result<Option<(Option<u32>, Option<u32>)>, CypherError> {
        let min = if let TokenKind::Integer(n) = self.peek() {
            let n = *n as u32;
            self.advance();
            Some(n)
        } else {
            None
        };

        if self.eat(&TokenKind::DotDot) {
            let max = if let TokenKind::Integer(n) = self.peek() {
                let n = *n as u32;
                self.advance();
                Some(n)
            } else {
                None
            };
            Ok(Some((min, max)))
        } else if let Some(n) = min {
            // *3 = exactly 3
            Ok(Some((Some(n), Some(n))))
        } else {
            // bare *: any length
            Ok(Some((None, None)))
        }
    }
}
