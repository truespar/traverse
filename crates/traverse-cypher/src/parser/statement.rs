//! Statements: queries, UNION and conditional queries, SHOW, and the clause sequence of a
//! single query.

use crate::ast::{
    BinOp, Clause, CmpOp, Expr, FinalPart, Literal, ProjectionItem, ProjectionItems, Query,
    QueryMode, QueryPart, RegularQuery, SingleQuery, Statement, UnaryOp, Union, UnwindClause,
    WithClause,
};
use crate::error::CypherError;
use crate::lexer::TokenKind;

use super::{Parser, parse};

impl Parser {
    /// Skips a leading `CYPHER [<version>] [key=value ...]` prefix. A statement can't otherwise
    /// start with a bare identifier, so a leading `CYPHER` is unambiguous.
    fn skip_cypher_prefix(&mut self) {
        if !matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("CYPHER")) {
            return;
        }
        self.advance(); // CYPHER
        // optional version (`5`, `25`); the server's plan-cache `strip_literals` turns it into a
        // parameter placeholder, so accept `Parameter` too
        if matches!(
            self.peek(),
            TokenKind::Integer(_) | TokenKind::Float(_) | TokenKind::Parameter(_)
        ) {
            self.advance();
        }
        // zero or more `key = value` options (runtime=, planner=, ...)
        while matches!(self.peek(), TokenKind::Ident(_)) && *self.peek_offset(1) == TokenKind::Eq {
            self.advance(); // key
            self.advance(); // '='
            if matches!(
                self.peek(),
                TokenKind::Ident(_) | TokenKind::Integer(_) | TokenKind::Float(_)
            ) {
                self.advance(); // value
            }
        }
    }

    /// Parses a complete statement.
    pub fn parse_statement(&mut self) -> Result<Statement, CypherError> {
        // optional `CYPHER [<version>] [key=value ...]` prefix (`CYPHER 25`, `CYPHER
        // runtime=parallel`), accepted and ignored: there's one runtime and version pinning is a
        // no-op. Drivers and Studio send these
        self.skip_cypher_prefix();
        // EXPLAIN / PROFILE prefix
        let mode = if self.eat(&TokenKind::Explain) {
            QueryMode::Explain
        } else if self.eat(&TokenKind::Profile) {
            QueryMode::Profile
        } else {
            QueryMode::Normal
        };
        // `CYPHER ...` is also accepted AFTER the mode keyword (`PROFILE CYPHER runtime=parallel
        // MATCH ...`, the order Neo4j's manual uses)
        self.skip_cypher_prefix();
        // leading `USE <graph[.name]>`, skipped: the embedded engine has one graph, and the server
        // routes by pre-scanning the query text for it
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("USE"))
            && matches!(self.peek_offset(1), TokenKind::Ident(_))
        {
            self.advance(); // USE
            self.parse_symbolic_name()?;
            while self.eat(&TokenKind::Dot) {
                self.parse_symbolic_name()?;
            }
        }

        let query = self.parse_query()?;
        // optional semicolon
        self.eat(&TokenKind::Semicolon);
        if *self.peek() != TokenKind::Eof {
            return Err(CypherError::syntax(
                format!("unexpected token after query: {:?}", self.peek()),
                self.span_pos(),
            ));
        }
        Ok(Statement { mode, query })
    }

    fn parse_query(&mut self) -> Result<Query, CypherError> {
        // CREATE EDGE INDEX
        if *self.peek() == TokenKind::Create
            && matches!(self.peek_offset(1), TokenKind::Ident(s) if s.eq_ignore_ascii_case("EDGE"))
            && *self.peek_offset(2) == TokenKind::Index
        {
            return self.parse_create_edge_index();
        }
        // DROP EDGE INDEX
        if *self.peek() == TokenKind::Drop
            && matches!(self.peek_offset(1), TokenKind::Ident(s) if s.eq_ignore_ascii_case("EDGE"))
            && *self.peek_offset(2) == TokenKind::Index
        {
            return self.parse_drop_edge_index();
        }
        // CREATE VECTOR INDEX (before plain CREATE INDEX)
        if *self.peek() == TokenKind::Create
            && matches!(self.peek_offset(1), TokenKind::Ident(s) if s.eq_ignore_ascii_case("VECTOR"))
            && *self.peek_offset(2) == TokenKind::Index
        {
            return self.parse_create_vector_index();
        }
        // CREATE <RANGE|TEXT|POINT|LOOKUP|FULLTEXT> INDEX (before plain CREATE INDEX); the type
        // keyword is a contextual identifier
        if *self.peek() == TokenKind::Create
            && let TokenKind::Ident(s) = self.peek_offset(1)
        {
            let up = s.to_uppercase();
            if matches!(
                up.as_str(),
                "RANGE" | "TEXT" | "POINT" | "LOOKUP" | "FULLTEXT"
            ) && *self.peek_offset(2) == TokenKind::Index
            {
                return self.parse_typed_create_index();
            }
        }
        // CREATE INDEX
        if *self.peek() == TokenKind::Create && *self.peek_offset(1) == TokenKind::Index {
            return self.parse_create_index();
        }
        // DROP INDEX
        if *self.peek() == TokenKind::Drop && *self.peek_offset(1) == TokenKind::Index {
            return self.parse_drop_index();
        }
        // CREATE CONSTRAINT
        if *self.peek() == TokenKind::Create && *self.peek_offset(1) == TokenKind::Constraint {
            return self.parse_create_constraint();
        }
        // DROP CONSTRAINT
        if *self.peek() == TokenKind::Drop && *self.peek_offset(1) == TokenKind::Constraint {
            return self.parse_drop_constraint();
        }
        // SHOW / TERMINATE TRANSACTIONS, alone or chained
        if self.at_transaction_command() {
            return self.parse_transaction_commands();
        }
        // SHOW INDEXES / CONSTRAINTS / FUNCTIONS / PROCEDURES, possibly composable
        // (`... [YIELD ...] [WHERE ...] [RETURN ...]`, Cypher 25). Typed index SHOW
        // (`SHOW <RANGE|TEXT|POINT|VECTOR|FULLTEXT|LOOKUP|ALL> INDEXES`) pre-filters the
        // leaf's `type` column
        if *self.peek() == TokenKind::Show {
            let type_word = match self.peek_offset(1) {
                TokenKind::Ident(s) => match s.to_uppercase().as_str() {
                    w @ ("RANGE" | "TEXT" | "POINT" | "VECTOR" | "FULLTEXT" | "LOOKUP") => {
                        Some(Some(w.to_string()))
                    }
                    _ => None,
                },
                TokenKind::All => Some(None),
                _ => None,
            };
            if let Some(tf) = type_word {
                let is_indexes = matches!(self.peek_offset(2), TokenKind::Index)
                    || matches!(
                        self.peek_offset(2),
                        TokenKind::Ident(s) if s.eq_ignore_ascii_case("INDEX") || s.eq_ignore_ascii_case("INDEXES")
                    );
                if is_indexes {
                    self.advance(); // SHOW
                    self.advance(); // <type>
                    self.advance(); // INDEXES
                    self.show_pre_filter = tf.map(|t| {
                        Expr::Comparison(
                            Box::new(Expr::Variable("type".to_string())),
                            vec![(CmpOp::Eq, Expr::Literal(Literal::String(t)))],
                        )
                    });
                    return self.parse_show_tail(crate::ast::ShowKind::Indexes);
                }
            }
        }
        if *self.peek() == TokenKind::Show {
            let kind = match self.peek_offset(1) {
                TokenKind::Index => Some(crate::ast::ShowKind::Indexes),
                TokenKind::Constraint => Some(crate::ast::ShowKind::Constraints),
                TokenKind::Ident(s) => match s.to_uppercase().as_str() {
                    "INDEX" | "INDEXES" => Some(crate::ast::ShowKind::Indexes),
                    "CONSTRAINT" | "CONSTRAINTS" => Some(crate::ast::ShowKind::Constraints),
                    "FUNCTION" | "FUNCTIONS" => Some(crate::ast::ShowKind::Functions),
                    "PROCEDURE" | "PROCEDURES" => Some(crate::ast::ShowKind::Procedures),
                    "SETTING" | "SETTINGS" => Some(crate::ast::ShowKind::Settings),
                    _ => None,
                },
                _ => None,
            };
            if let Some(kind) = kind {
                self.advance(); // SHOW
                self.advance(); // <thing>
                return self.parse_show_tail(kind);
            }
        }
        // ANALYZE GRAPH
        if *self.peek() == TokenKind::Analyze && *self.peek_offset(1) == TokenKind::Graph {
            return self.parse_analyze_graph();
        }
        let regular = self.parse_regular_query()?;
        Ok(Query::Regular(regular))
    }

    /// At `SHOW TRANSACTION[S]` or `TERMINATE TRANSACTION[S]`?
    fn at_transaction_command(&self) -> bool {
        let lead = match self.peek() {
            TokenKind::Show => true,
            TokenKind::Ident(s) => s.eq_ignore_ascii_case("TERMINATE"),
            _ => false,
        };
        lead && matches!(
            self.peek_offset(1),
            TokenKind::Ident(s) if s.eq_ignore_ascii_case("TRANSACTION")
                || s.eq_ignore_ascii_case("TRANSACTIONS")
                || s.eq_ignore_ascii_case("TX")
        )
    }

    /// `SHOW TRANSACTIONS [ids] [YIELD ... | WHERE ...]`, `TERMINATE TRANSACTIONS ids [YIELD ... | WHERE
    /// ...]`, chained (each but the last with YIELD), then `[RETURN ...]`. Ids are comma-separated
    /// expressions or one expression giving a STRING or LIST<STRING> (Neo4j's grammar).
    fn parse_transaction_commands(&mut self) -> Result<Query, CypherError> {
        use crate::ast::{TransactionCommand, TransactionCommandKind};
        let mut commands = Vec::new();
        loop {
            let start = self.span_pos();
            let kind = if *self.peek() == TokenKind::Show {
                TransactionCommandKind::Show
            } else {
                TransactionCommandKind::Terminate
            };
            self.advance(); // SHOW | TERMINATE
            self.advance(); // TRANSACTION[S]
            let ids = if self.at_transaction_ids() {
                let first = self.parse_expr()?;
                if *self.peek() == TokenKind::Comma {
                    let mut list = vec![first];
                    while self.eat(&TokenKind::Comma) {
                        list.push(self.parse_expr()?);
                    }
                    Some(Expr::ListLiteral(list))
                } else {
                    Some(first)
                }
            } else {
                None
            };
            if kind == TransactionCommandKind::Terminate && ids.is_none() {
                return Err(CypherError::syntax(
                    "TERMINATE TRANSACTIONS needs the ids of the transactions to terminate",
                    self.span_pos(),
                ));
            }
            let mut yield_clause = None;
            let mut where_only = None;
            if self.eat_keyword(&TokenKind::Yield) {
                yield_clause = Some(self.parse_yield_as_with()?);
            } else if self.eat_keyword(&TokenKind::Where) {
                where_only = Some(self.parse_expr()?);
            }
            let chained = self.at_transaction_command();
            if chained && yield_clause.is_none() {
                return Err(CypherError::syntax(
                    format!(
                        "{} followed by another transaction command needs a YIELD clause",
                        kind.name()
                    ),
                    start,
                ));
            }
            commands.push(TransactionCommand {
                kind,
                ids,
                yield_clause,
                where_only,
            });
            if !chained {
                break;
            }
        }
        let return_clause = if *self.peek() == TokenKind::Return {
            Some(self.parse_return_clause()?)
        } else {
            None
        };
        Ok(Query::TransactionCommands {
            commands,
            return_clause,
        })
    }

    /// Do transaction ids follow (anything but the clause keywords, another command, or the end)?
    fn at_transaction_ids(&self) -> bool {
        !matches!(
            self.peek(),
            TokenKind::Yield
                | TokenKind::Where
                | TokenKind::Return
                | TokenKind::Semicolon
                | TokenKind::Eof
        ) && !self.at_transaction_command()
    }

    /// Tail after `SHOW <thing>`: nothing, `WHERE pred`, `YIELD ...`, and/or a trailing `RETURN ...`.
    fn parse_show_tail(&mut self, kind: crate::ast::ShowKind) -> Result<Query, CypherError> {
        use crate::ast::ShowKind;
        let is_yield = *self.peek() == TokenKind::Yield;
        let is_where = *self.peek() == TokenKind::Where;
        let is_return = *self.peek() == TokenKind::Return;
        if !is_yield && !is_where && !is_return {
            // typed bare form (`SHOW FULLTEXT INDEXES`): the pre-filter needs the composable form
            if self.show_pre_filter.is_some() {
                return Ok(Query::ShowComposable {
                    kind,
                    yield_clause: None,
                    where_only: None,
                    return_clause: None,
                    pre_filter: self.show_pre_filter.take(),
                });
            }
            // bare form: the standalone SHOW variants
            return Ok(match kind {
                ShowKind::Indexes => Query::ShowIndexes,
                ShowKind::Constraints => Query::ShowConstraints,
                ShowKind::Functions => Query::ShowFunctions,
                ShowKind::Procedures => Query::ShowProcedures,
                ShowKind::Settings => Query::ShowSettings,
            });
        }
        let mut yield_clause = None;
        let mut where_only = None;
        if is_yield {
            self.advance(); // YIELD
            yield_clause = Some(self.parse_yield_as_with()?);
        } else if is_where {
            self.advance(); // WHERE
            where_only = Some(self.parse_expr()?);
        }
        let return_clause = if *self.peek() == TokenKind::Return {
            Some(self.parse_return_clause()?)
        } else {
            None
        };
        Ok(Query::ShowComposable {
            kind,
            yield_clause,
            where_only,
            return_clause,
            pre_filter: self.show_pre_filter.take(),
        })
    }

    /// `YIELD (* | col [AS alias], ...) [WHERE p] [ORDER BY][SKIP][LIMIT]` as a `WithClause` (YIELD
    /// is a WITH over the SHOW leaf's columns). Column names may be keywords (`type`, ...).
    fn parse_yield_as_with(&mut self) -> Result<WithClause, CypherError> {
        let items = if *self.peek() == TokenKind::Star {
            self.advance();
            ProjectionItems::All
        } else {
            let mut list = vec![self.parse_yield_item_as_projection()?];
            while self.eat(&TokenKind::Comma) {
                list.push(self.parse_yield_item_as_projection()?);
            }
            ProjectionItems::Explicit(list)
        };
        // Neo4j allows WHERE and ORDER BY/SKIP/LIMIT in either order after the field list; accept
        // both
        let mut where_clause = None;
        let mut order_by = None;
        let mut skip = None;
        let mut limit = None;
        loop {
            if where_clause.is_none() && self.eat_keyword(&TokenKind::Where) {
                where_clause = Some(self.parse_expr()?);
            } else if order_by.is_none() && *self.peek() == TokenKind::Order {
                order_by = self.parse_order_by()?;
            } else if skip.is_none() && self.eat_keyword(&TokenKind::Skip) {
                skip = Some(self.parse_expr()?);
            } else if limit.is_none() && self.eat_keyword(&TokenKind::Limit) {
                limit = Some(self.parse_expr()?);
            } else {
                break;
            }
        }
        Ok(WithClause {
            distinct: false,
            items,
            order_by,
            skip,
            limit,
            where_clause,
        })
    }

    /// One YIELD column: `name [AS alias]`; the name may be a reserved word (`type`).
    fn parse_yield_item_as_projection(&mut self) -> Result<ProjectionItem, CypherError> {
        let name = self.parse_procedure_name_segment()?;
        let alias = if self.eat_keyword(&TokenKind::As) {
            Some(self.parse_symbolic_name()?)
        } else {
            None
        };
        Ok(ProjectionItem {
            expr: Expr::Variable(name.clone()),
            alias,
            source_text: Some(name),
        })
    }

    fn parse_regular_query(&mut self) -> Result<RegularQuery, CypherError> {
        // Cypher 25 conditional query: WHEN ... THEN ... [WHEN ...] [ELSE ...]
        if *self.peek() == TokenKind::When {
            return self.parse_conditional_query(None);
        }
        let mut single = self.parse_single_query()?;
        // mid-query conditional `MATCH ... WHEN cond THEN ...`: the clauses so far become a READ-ONLY
        // prefix copied into every guarded arm (guards partition its rows, so per-row routing falls
        // out of the UNION ALL desugar)
        if *self.peek() == TokenKind::When {
            if single.final_part.return_clause.is_some() {
                return Err(CypherError::syntax(
                    "WHEN after RETURN needs NEXT to start a new segment".to_string(),
                    self.span_pos(),
                ));
            }
            return self.parse_conditional_query(Some(single));
        }
        // Cypher 25 sequential composition `Q1 NEXT Q2 NEXT ...`: each segment's RETURN drives the
        // next. RETURN and WITH have the same shape, so segments FUSE into one SingleQuery, the
        // left RETURN becoming a WITH part
        let is_next =
            |t: &TokenKind| matches!(t, TokenKind::Ident(s) if s.eq_ignore_ascii_case("NEXT"));
        while is_next(self.peek()) {
            self.advance();
            if *self.peek() == TokenKind::When {
                // `... RETURN ... NEXT WHEN ...`: fuse the RETURN into a WITH part (empty continuation)
                // and use it as the prefix
                let prefix = Self::fuse_next(
                    single,
                    SingleQuery {
                        parts: Vec::new(),
                        final_part: FinalPart {
                            clauses: Vec::new(),
                            return_clause: None,
                        },
                    },
                )?;
                return self.parse_conditional_query(Some(prefix));
            }
            let second = self.parse_single_query()?;
            single = Self::fuse_next(single, second)?;
        }
        let mut unions = Vec::new();
        while self.eat_keyword(&TokenKind::Union) {
            let all = self.eat_keyword(&TokenKind::All);
            let query = self.parse_single_query()?;
            unions.push(Union { all, query });
            if is_next(self.peek()) {
                return Err(CypherError::syntax(
                    "NEXT combined with UNION segments is not supported yet".to_string(),
                    self.span_pos(),
                ));
            }
        }
        Ok(RegularQuery { single, unions })
    }

    /// Fuses `first NEXT second`: the first RETURN (same shape as a WITH) becomes a query-part
    /// boundary whose aliases are exactly what the second segment sees.
    fn fuse_next(first: SingleQuery, second: SingleQuery) -> Result<SingleQuery, CypherError> {
        let Some(rc) = first.final_part.return_clause else {
            return Err(CypherError::syntax(
                "NEXT requires the previous segment to end with RETURN".to_string(),
                0,
            ));
        };
        let mut parts = first.parts;
        parts.push(QueryPart {
            clauses: first.final_part.clauses,
            with: WithClause {
                distinct: rc.distinct,
                items: rc.items,
                order_by: rc.order_by,
                skip: rc.skip,
                limit: rc.limit,
                where_clause: None,
            },
        });
        parts.extend(second.parts);
        Ok(SingleQuery {
            parts,
            final_part: second.final_part,
        })
    }

    /// Cypher 25 conditional query, desugared to guarded `UNION ALL` arms. Each
    /// branch starts with `UNWIND CASE WHEN <cond> THEN [true] ELSE [] END AS __when_i`
    /// (one row if the condition holds, else none; NULL counts as false so ELSE
    /// fires, as in Neo4j). Conditions are made exclusive by conjoining
    /// `NOT coalesce(p_j, false)` for every earlier predicate, so exactly one arm
    /// yields rows. Column consistency falls out of UNION validation.
    fn parse_conditional_query(
        &mut self,
        prefix: Option<SingleQuery>,
    ) -> Result<RegularQuery, CypherError> {
        // the prefix runs in every arm, so it must be read-only (writes would repeat per arm)
        if let Some(ref pfx) = prefix {
            let clauses = pfx
                .parts
                .iter()
                .flat_map(|p| p.clauses.iter())
                .chain(pfx.final_part.clauses.iter());
            for c in clauses {
                let writes = matches!(
                    c,
                    Clause::Create(_)
                        | Clause::Merge(_)
                        | Clause::Set(_)
                        | Clause::Remove(_)
                        | Clause::Delete(_)
                        | Clause::ForEach(_)
                        | Clause::LoadCsv(_)
                        | Clause::CallSubquery(_)
                        | Clause::Call(_)
                );
                if writes {
                    return Err(CypherError::syntax(
                        "conditional queries (WHEN) after updating or CALL clauses are not supported yet"
                            .to_string(),
                        self.span_pos(),
                    ));
                }
            }
        }
        let mut branches: Vec<(Option<Expr>, SingleQuery)> = Vec::new();
        while self.eat_keyword(&TokenKind::When) {
            let pred = self.parse_expr()?;
            self.expect(&TokenKind::Then)?;
            branches.push((Some(pred), self.parse_branch_query()?));
        }
        if self.eat_keyword(&TokenKind::Else) {
            branches.push((None, self.parse_branch_query()?));
        }

        let not_coalesced = |p: &Expr| {
            Expr::UnaryOp(
                UnaryOp::Not,
                Box::new(Expr::FunctionCall {
                    name: "coalesce".to_string(),
                    distinct: false,
                    args: vec![p.clone(), Expr::Literal(Literal::Bool(false))],
                }),
            )
        };
        let mut arms: Vec<SingleQuery> = Vec::new();
        let mut prior: Vec<Expr> = Vec::new();
        for (i, (pred, mut body)) in branches.into_iter().enumerate() {
            // cond = p_i AND !coalesce(p_1,false) AND ...; the ELSE arm is the negations alone
            let mut cond: Option<Expr> = pred.clone();
            for p in &prior {
                let neg = not_coalesced(p);
                cond = Some(match cond {
                    None => neg,
                    Some(c) => Expr::BinaryOp(Box::new(c), BinOp::And, Box::new(neg)),
                });
            }
            let cond = cond.unwrap_or(Expr::Literal(Literal::Bool(true)));
            let guard = Clause::Unwind(UnwindClause {
                expr: Expr::Case {
                    operand: None,
                    whens: vec![(
                        cond,
                        Expr::ListLiteral(vec![Expr::Literal(Literal::Bool(true))]),
                    )],
                    else_expr: Some(Box::new(Expr::ListLiteral(Vec::new()))),
                },
                alias: format!("__when_{i}"),
            });
            // this arm's leading clauses: the read-only prefix's final clauses, then the guard
            let mut lead: Vec<Clause> = prefix
                .as_ref()
                .map(|p| p.final_part.clauses.clone())
                .unwrap_or_default();
            lead.push(guard);
            if let Some(part) = body.parts.first_mut() {
                let mut cl = lead;
                cl.extend(std::mem::take(&mut part.clauses));
                part.clauses = cl;
            } else {
                let mut cl = lead;
                cl.extend(std::mem::take(&mut body.final_part.clauses));
                body.final_part.clauses = cl;
            }
            if let Some(ref pfx) = prefix {
                let mut parts = pfx.parts.clone();
                parts.extend(std::mem::take(&mut body.parts));
                body.parts = parts;
            }
            if let Some(p) = pred {
                prior.push(p);
            }
            arms.push(body);
        }

        let mut arms = arms.into_iter();
        let single = arms.next().ok_or_else(|| {
            CypherError::syntax(
                "conditional query needs at least one WHEN branch".to_string(),
                0,
            )
        })?;
        let unions = arms.map(|query| Union { all: true, query }).collect();
        Ok(RegularQuery { single, unions })
    }

    /// Conditional-query branch: `{ <query> }`, or a bare query up to the next WHEN / ELSE / end.
    fn parse_branch_query(&mut self) -> Result<SingleQuery, CypherError> {
        if self.eat(&TokenKind::LBrace) {
            let q = self.parse_single_query()?;
            self.expect(&TokenKind::RBrace)?;
            Ok(q)
        } else {
            self.parse_single_query()
        }
    }

    #[recursive::recursive]
    pub(super) fn parse_single_query(&mut self) -> Result<SingleQuery, CypherError> {
        self.enter_nesting()?;
        let result = self.parse_single_query_inner();
        self.leave_nesting();
        result
    }

    fn parse_single_query_inner(&mut self) -> Result<SingleQuery, CypherError> {
        // clauses: WITH closes a QueryPart; RETURN or EOF/UNION ends the FinalPart
        let mut parts = Vec::new();
        let mut current_clauses = Vec::new();

        loop {
            match self.peek() {
                TokenKind::With => {
                    let with = self.parse_with_clause()?;
                    parts.push(QueryPart {
                        clauses: std::mem::take(&mut current_clauses),
                        with,
                    });
                }
                TokenKind::Return => {
                    let ret = self.parse_return_clause()?;
                    return Ok(SingleQuery {
                        parts,
                        final_part: FinalPart {
                            clauses: current_clauses,
                            return_clause: Some(ret),
                        },
                    });
                }
                TokenKind::Match | TokenKind::Optional => {
                    let clause = self.parse_match_clause()?;
                    current_clauses.push(clause);
                }
                TokenKind::Create => {
                    let clause = self.parse_create_clause()?;
                    current_clauses.push(Clause::Create(clause));
                }
                TokenKind::Merge => {
                    let clause = self.parse_merge_clause()?;
                    current_clauses.push(Clause::Merge(clause));
                }
                TokenKind::Delete => {
                    let clause = self.parse_delete_clause(false)?;
                    current_clauses.push(Clause::Delete(clause));
                }
                TokenKind::Detach => {
                    self.advance();
                    self.expect(&TokenKind::Delete)?;
                    let mut clause = self.parse_delete_inner()?;
                    clause.detach = true;
                    current_clauses.push(Clause::Delete(clause));
                }
                TokenKind::Set => {
                    let clause = self.parse_set_clause()?;
                    current_clauses.push(Clause::Set(clause));
                }
                TokenKind::Remove => {
                    let clause = self.parse_remove_clause()?;
                    current_clauses.push(Clause::Remove(clause));
                }
                TokenKind::Unwind => {
                    let clause = self.parse_unwind_clause()?;
                    current_clauses.push(Clause::Unwind(clause));
                }
                // NEXT ends this sequential-composition segment; parse_regular_query fuses segments
                TokenKind::Ident(s) if s.eq_ignore_ascii_case("NEXT") => {
                    return Ok(SingleQuery {
                        parts,
                        final_part: FinalPart {
                            clauses: current_clauses,
                            return_clause: None,
                        },
                    });
                }
                // -- Cypher 25 / GQL clauses -----------------
                // FILTER pred: standalone WHERE without MATCH, desugared to `WITH * WHERE pred`
                // (all variables pass, rows filtered)
                TokenKind::Ident(s) if s.eq_ignore_ascii_case("FILTER") => {
                    self.advance();
                    let pred = self.parse_expr()?;
                    parts.push(QueryPart {
                        clauses: std::mem::take(&mut current_clauses),
                        with: WithClause {
                            distinct: false,
                            items: ProjectionItems::All,
                            order_by: None,
                            skip: None,
                            limit: None,
                            where_clause: Some(pred),
                        },
                    });
                }
                // LET a = e [, b = e ...]: per-row bindings keeping every variable in scope. Desugars
                // to `UNWIND [e] AS a` per binding: one output row per input row, aggregates
                // rejected, exactly GQL's LET
                TokenKind::Ident(s) if s.eq_ignore_ascii_case("LET") => {
                    self.advance();
                    loop {
                        let name = self.parse_symbolic_name()?;
                        self.expect(&TokenKind::Eq)?;
                        let value = self.parse_expr()?;
                        current_clauses.push(Clause::Unwind(UnwindClause {
                            expr: Expr::ListLiteral(vec![value]),
                            alias: name,
                        }));
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                // FINISH: ends the query with no result columns, side effects still run (GQL /
                // Cypher 25); a write query without RETURN, generalized to reads
                TokenKind::Ident(s) if s.eq_ignore_ascii_case("FINISH") => {
                    self.advance();
                    return Ok(SingleQuery {
                        parts,
                        final_part: FinalPart {
                            clauses: current_clauses,
                            return_clause: None,
                        },
                    });
                }
                TokenKind::Foreach => {
                    let clause = self.parse_foreach_clause()?;
                    current_clauses.push(Clause::ForEach(clause));
                }
                TokenKind::Load => {
                    let clause = self.parse_load_csv_clause()?;
                    current_clauses.push(Clause::LoadCsv(clause));
                }
                TokenKind::Call => {
                    // inline subquery vs procedure call, by the token after `CALL`:
                    //   CALL { ... }        -> subquery (transparent scope)
                    //   CALL (a, b) { ... } -> subquery with explicit imports
                    //   CALL proc.name()  -> procedure call
                    // procedure names are (dotted) identifiers, so `{` or `(` is unambiguous
                    match self.peek_offset(1) {
                        TokenKind::LBrace | TokenKind::LParen => {
                            let clause = self.parse_call_subquery_clause()?;
                            current_clauses.push(Clause::CallSubquery(clause));
                        }
                        _ => {
                            let clause = self.parse_call_clause()?;
                            current_clauses.push(Clause::Call(clause));
                            // `CALL ... YIELD ... WHERE pred`: post-YIELD filter, desugars to `WITH *
                            // WHERE pred` like FILTER
                            if self.eat_keyword(&TokenKind::Where) {
                                let pred = self.parse_expr()?;
                                parts.push(QueryPart {
                                    clauses: std::mem::take(&mut current_clauses),
                                    with: WithClause {
                                        distinct: false,
                                        items: ProjectionItems::All,
                                        order_by: None,
                                        skip: None,
                                        limit: None,
                                        where_clause: Some(pred),
                                    },
                                });
                            }
                        }
                    }
                }
                TokenKind::Eof
                | TokenKind::Union
                | TokenKind::Semicolon
                | TokenKind::RParen
                | TokenKind::RBrace
                // `WHEN` / `ELSE` end a conditional branch, `NEXT` a composition segment; none can
                // start a clause
                | TokenKind::When
                | TokenKind::Else => {
                    // end of a single query (no RETURN). `RBrace` ends an inline `CALL { ... }` body
                    // (see `parse_call_subquery_clause`), the others a top-level query; a bare `}`
                    // never starts a clause, so it's a safe terminator.
                    //
                    // A TOP-LEVEL query (Eof/Union/;) without RETURN must end with an update
                    // clause, FINISH or a call; ending on a read clause is a syntax error, as in
                    // Neo4j. `}` is exempt: EXISTS/COUNT subqueries legally end with MATCH.
                    if matches!(
                        self.peek(),
                        TokenKind::Eof | TokenKind::Union | TokenKind::Semicolon
                    ) {
                        let bad = match current_clauses.last() {
                            Some(Clause::Match(_)) | Some(Clause::OptionalMatch(_)) => {
                                Some("MATCH")
                            }
                            Some(Clause::Unwind(_)) => Some("UNWIND"),
                            Some(Clause::LoadCsv(_)) => Some("LOAD CSV"),
                            None if !parts.is_empty() => Some("WITH"),
                            _ => None,
                        };
                        if let Some(kw) = bad {
                            return Err(CypherError::syntax(
                                format!(
                                    "Query cannot conclude with {kw} (must be a RETURN \
                                     clause, a FINISH clause, an update clause, a unit \
                                     subquery call, or a procedure call with no YIELD)"
                                ),
                                self.span_pos(),
                            ));
                        }
                    }
                    return Ok(SingleQuery {
                        parts,
                        final_part: FinalPart {
                            clauses: current_clauses,
                            return_clause: None,
                        },
                    });
                }
                _ => {
                    return Err(CypherError::syntax(
                        format!("unexpected token: {:?}", self.peek()),
                        self.span_pos(),
                    ));
                }
            }
        }
    }
}

/// `apoc.cypher.runFirstColumnMany/Single(statement, params)` in FUNCTION
/// position, desugared at parse time into a `COLLECT { ... }` subquery (Single
/// also wraps it in `head(...)`). The statement must be a string LITERAL (it's
/// inlined into the plan); params must be a map literal whose values may be any
/// expression. They become a leading `WITH v1 AS k1, ...`, so outer correlation
/// works like APOC's identifier binding.
pub(super) fn desugar_run_first_column(name: &str, args: Vec<Expr>) -> Result<Expr, CypherError> {
    let mut it = args.into_iter();
    let stmt_text = match it.next() {
        Some(Expr::Literal(Literal::String(s))) => s,
        _ => {
            return Err(CypherError::semantic(format!(
                "{name} requires a string-literal statement (the query is inlined \
                 at plan time); for dynamic statements use CALL apoc.cypher.run"
            )));
        }
    };
    let params = match it.next() {
        None | Some(Expr::Literal(Literal::Null)) => Vec::new(),
        Some(Expr::MapLiteral(pairs)) => pairs,
        Some(_) => {
            return Err(CypherError::semantic(format!(
                "{name} requires a map-literal params argument \
                 (values may be arbitrary expressions)"
            )));
        }
    };
    // legacy 3-arg form runFirstColumn(stmt, params, firstColumnOnly): only the shared first-column
    // semantics are supported
    let single = name.ends_with("single");

    let inner = parse(&stmt_text).map_err(|e| {
        CypherError::semantic(format!("{name}: inner statement failed to parse: {e}"))
    })?;
    let Query::Regular(reg) = inner.query else {
        return Err(CypherError::semantic(format!(
            "{name}: inner statement must be a regular query"
        )));
    };
    if !reg.unions.is_empty() {
        return Err(CypherError::semantic(format!(
            "{name}: UNION inner statements are not supported"
        )));
    }
    let mut sq = reg.single;
    if !params.is_empty() {
        let items = params
            .into_iter()
            .map(|(k, v)| ProjectionItem {
                expr: v,
                alias: Some(k),
                source_text: None,
            })
            .collect();
        sq.parts.insert(
            0,
            QueryPart {
                clauses: Vec::new(),
                with: WithClause {
                    distinct: false,
                    items: ProjectionItems::Explicit(items),
                    order_by: None,
                    skip: None,
                    limit: None,
                    where_clause: None,
                },
            },
        );
    }
    let collect = Expr::CollectSubquery(Box::new(sq));
    Ok(if single {
        Expr::FunctionCall {
            name: "head".to_string(),
            distinct: false,
            args: vec![collect],
        }
    } else {
        collect
    })
}
