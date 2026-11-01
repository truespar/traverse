//! Expressions, by precedence from OR down to atoms.

use crate::ast::{
    BinOp, Clause, CmpOp, Expr, FinalPart, Literal, MatchClause, Pattern, PatternPart,
    QuantifierKind, QueryPart, ShortestPathMode, SingleQuery, StringOp, UnaryOp,
};
use crate::error::CypherError;
use crate::lexer::TokenKind;

use super::statement::desugar_run_first_column;
use super::{Parser, is_keyword_as_ident, normalize_agg_alias};

impl Parser {
    /// Parses an expression.
    #[recursive::recursive]
    pub fn parse_expr(&mut self) -> Result<Expr, CypherError> {
        self.enter_nesting()?;
        let result = self.parse_or();
        self.leave_nesting();
        result
    }

    fn parse_or(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.parse_xor()?;
        let mut terms = 1usize;
        while self.eat_keyword(&TokenKind::Or) {
            terms = self.count_chain_term(terms)?;
            let right = self.parse_xor()?;
            left = Expr::BinaryOp(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn parse_xor(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.parse_and()?;
        let mut terms = 1usize;
        while self.eat_keyword(&TokenKind::Xor) {
            terms = self.count_chain_term(terms)?;
            let right = self.parse_and()?;
            left = Expr::BinaryOp(Box::new(left), BinOp::Xor, Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.parse_not()?;
        let mut terms = 1usize;
        while self.eat_keyword(&TokenKind::And) {
            terms = self.count_chain_term(terms)?;
            let right = self.parse_not()?;
            left = Expr::BinaryOp(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, CypherError> {
        // `NOT NOT ... x`: consume iteratively (no stack for long chains), wrap inside out
        let mut nots = 0usize;
        while self.eat_keyword(&TokenKind::Not) {
            nots = self.count_chain_term(nots)?;
        }
        let mut expr = self.parse_comparison()?;
        for _ in 0..nots {
            expr = Expr::UnaryOp(UnaryOp::Not, Box::new(expr));
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr, CypherError> {
        let left = self.parse_string_predicate()?;

        let mut comparisons = Vec::new();
        loop {
            let op = match self.peek() {
                TokenKind::Eq => CmpOp::Eq,
                TokenKind::Neq => CmpOp::Neq,
                TokenKind::Lt => {
                    // not `<-` (relationship): `<-[` / `<--` are patterns, `< -expr` is a
                    // comparison with unary minus
                    if matches!(self.peek_offset(1), TokenKind::Minus)
                        && matches!(self.peek_offset(2), TokenKind::LBracket | TokenKind::Minus)
                    {
                        break;
                    }
                    CmpOp::Lt
                }
                TokenKind::Lte => CmpOp::Lte,
                TokenKind::Gt => CmpOp::Gt,
                TokenKind::Gte => CmpOp::Gte,
                _ => break,
            };
            self.advance();
            let right = self.parse_string_predicate()?;
            comparisons.push((op, right));
        }

        if comparisons.is_empty() {
            Ok(left)
        } else {
            Ok(Expr::Comparison(Box::new(left), comparisons))
        }
    }

    /// Type name for `IS :: <TYPE>` / `IS TYPED <TYPE>`: one to three words (`INTEGER`, `LOCAL
    /// DATETIME`), optional `<...>` generic (`LIST<INTEGER>`), optional `NOT NULL`. Returns the
    /// normalised name (uppercase, space-joined) and the not-null flag.
    fn parse_type_predicate_name(&mut self) -> Result<(String, bool), CypherError> {
        let mut name = String::new();
        while let TokenKind::Ident(s) = self.peek() {
            if !name.is_empty() {
                name.push(' ');
            }
            name.push_str(&s.to_uppercase());
            self.advance();
            if name.split(' ').count() >= 3 {
                break;
            }
        }
        // generic parameter: LIST<T> / ARRAY<T>
        if *self.peek() == TokenKind::Lt {
            self.advance(); // '<'
            name.push('<');
            let mut depth = 1;
            while depth > 0 && *self.peek() != TokenKind::Eof {
                match self.peek() {
                    TokenKind::Lt => {
                        depth += 1;
                        name.push('<');
                    }
                    TokenKind::Gt => {
                        depth -= 1;
                        if depth > 0 {
                            name.push('>');
                        }
                    }
                    TokenKind::Ident(s) => {
                        if name.ends_with(|c: char| c.is_alphanumeric()) {
                            name.push(' ');
                        }
                        name.push_str(&s.to_uppercase());
                    }
                    _ => {}
                }
                self.advance();
            }
            name.push('>');
        }
        if name.is_empty() {
            return Err(CypherError::syntax(
                "expected a type name after IS :: / IS TYPED",
                self.span_pos(),
            ));
        }
        // optional `NOT NULL`
        let not_null = if self.eat_keyword(&TokenKind::Not) {
            self.expect(&TokenKind::Null)?;
            true
        } else {
            false
        };
        Ok((name, not_null))
    }

    fn parse_string_predicate(&mut self) -> Result<Expr, CypherError> {
        let mut expr = self.parse_addition()?;

        loop {
            if self.eat_keyword(&TokenKind::In) {
                let right = self.parse_addition()?;
                expr = Expr::In(Box::new(expr), Box::new(right));
            } else if self.peek() == &TokenKind::Is {
                self.advance(); // IS
                let negated = self.eat_keyword(&TokenKind::Not);
                if *self.peek() == TokenKind::Null {
                    // IS [NOT] NULL
                    self.advance();
                    expr = if negated {
                        Expr::IsNotNull(Box::new(expr))
                    } else {
                        Expr::IsNull(Box::new(expr))
                    };
                } else if *self.peek() == TokenKind::DoubleColon
                    || matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("TYPED"))
                {
                    // IS [NOT] :: <TYPE> [NOT NULL] | IS [NOT] TYPED <TYPE>
                    self.advance(); // '::' or TYPED
                    let (type_name, not_null) = self.parse_type_predicate_name()?;
                    expr = Expr::IsTyped {
                        expr: Box::new(expr),
                        type_name,
                        negated,
                        not_null,
                    };
                } else {
                    return Err(CypherError::syntax(
                        "expected NULL, ::, or TYPED after IS",
                        self.span_pos(),
                    ));
                }
            } else if self.peek() == &TokenKind::StartsWith {
                self.advance();
                // STARTS lexes as StartsWith; WITH must follow
                self.expect(&TokenKind::With)?;
                let right = self.parse_addition()?;
                expr = Expr::StringOp(Box::new(expr), StringOp::StartsWith, Box::new(right));
            } else if self.peek() == &TokenKind::EndsWith {
                self.advance();
                // ENDS WITH
                self.expect(&TokenKind::With)?;
                let right = self.parse_addition()?;
                expr = Expr::StringOp(Box::new(expr), StringOp::EndsWith, Box::new(right));
            } else if self.peek() == &TokenKind::Contains {
                self.advance();
                let right = self.parse_addition()?;
                expr = Expr::StringOp(Box::new(expr), StringOp::Contains, Box::new(right));
            } else if self.peek() == &TokenKind::RegexMatch {
                self.advance();
                let right = self.parse_addition()?;
                expr = Expr::StringOp(Box::new(expr), StringOp::Regex, Box::new(right));
            } else {
                break;
            }
        }

        Ok(expr)
    }

    fn parse_addition(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.parse_multiplication()?;
        let mut terms = 1usize;
        loop {
            match self.peek() {
                TokenKind::Plus => {
                    self.advance();
                    terms = self.count_chain_term(terms)?;
                    let right = self.parse_multiplication()?;
                    left = Expr::BinaryOp(Box::new(left), BinOp::Add, Box::new(right));
                }
                TokenKind::Minus => {
                    // `a - b` vs `(a)-[...]->(b)`: don't consume `-` if a pattern follows
                    if matches!(self.peek_offset(1), TokenKind::LBracket | TokenKind::Gt) {
                        break;
                    }
                    self.advance();
                    terms = self.count_chain_term(terms)?;
                    let right = self.parse_multiplication()?;
                    left = Expr::BinaryOp(Box::new(left), BinOp::Sub, Box::new(right));
                }
                TokenKind::ConcatOp => {
                    // `||`: Cypher 25 concatenation, additive precedence
                    self.advance();
                    terms = self.count_chain_term(terms)?;
                    let right = self.parse_multiplication()?;
                    left = Expr::BinaryOp(Box::new(left), BinOp::Concat, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_multiplication(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.parse_power()?;
        let mut terms = 1usize;
        loop {
            match self.peek() {
                TokenKind::Star => {
                    self.advance();
                    terms = self.count_chain_term(terms)?;
                    let right = self.parse_power()?;
                    left = Expr::BinaryOp(Box::new(left), BinOp::Mul, Box::new(right));
                }
                TokenKind::Slash => {
                    self.advance();
                    terms = self.count_chain_term(terms)?;
                    let right = self.parse_power()?;
                    left = Expr::BinaryOp(Box::new(left), BinOp::Div, Box::new(right));
                }
                TokenKind::Percent => {
                    self.advance();
                    terms = self.count_chain_term(terms)?;
                    let right = self.parse_power()?;
                    left = Expr::BinaryOp(Box::new(left), BinOp::Mod, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_power(&mut self) -> Result<Expr, CypherError> {
        let mut left = self.parse_unary()?;
        let mut terms = 1usize;
        while self.eat(&TokenKind::Caret) {
            terms = self.count_chain_term(terms)?;
            let right = self.parse_unary()?;
            left = Expr::BinaryOp(Box::new(left), BinOp::Pow, Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, CypherError> {
        // `- - x` / `+ + x`: consume iteratively (no stack for long chains); unary `+` is a no-op,
        // each `-` wraps a negation inside out
        let mut negations = 0usize;
        let mut literal_min: Option<Expr> = None;
        loop {
            match self.peek() {
                TokenKind::Minus => {
                    self.advance();
                    // -OverflowInt -> negative integer (-9223372036854775808 = i64::MIN)
                    if let TokenKind::OverflowInt(text) = self.peek() {
                        let text = text.clone();
                        let parsed = if let Some(hex) =
                            text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
                        {
                            u64::from_str_radix(hex, 16).ok()
                        } else if let Some(oct) =
                            text.strip_prefix("0o").or_else(|| text.strip_prefix("0O"))
                        {
                            u64::from_str_radix(oct, 8).ok()
                        } else {
                            text.parse::<u64>().ok()
                        };
                        if let Some(mag) = parsed
                            && mag == (i64::MAX as u64) + 1
                        {
                            self.advance();
                            literal_min = Some(Expr::Literal(Literal::Integer(i64::MIN)));
                            break;
                        }
                    }
                    negations = self.count_chain_term(negations)?;
                }
                TokenKind::Plus => {
                    self.advance();
                }
                _ => break,
            }
        }
        let mut expr = match literal_min {
            Some(expr) => expr,
            None => self.parse_postfix()?,
        };
        for _ in 0..negations {
            expr = Expr::UnaryOp(UnaryOp::Neg, Box::new(expr));
        }
        Ok(expr)
    }

    fn parse_postfix(&mut self) -> Result<Expr, CypherError> {
        let mut expr = self.parse_atom()?;

        loop {
            match self.peek() {
                TokenKind::Dot => {
                    self.advance();
                    // keyword-permissive: property keys and namespaced function leaves may be
                    // reserved words (`n.order`, `coll.remove`)
                    let key = self.parse_procedure_name_segment()?;
                    // namespaced call: date.truncate(...), duration.between(...); two-level:
                    // vector.similarity.cosine(...)
                    if *self.peek() == TokenKind::LParen
                        && let Expr::Variable(ref ns) = expr
                    {
                        let ns_lower = ns.to_lowercase();
                        if matches!(
                            ns_lower.as_str(),
                            "date"
                                | "time"
                                | "localtime"
                                | "localdatetime"
                                | "datetime"
                                | "duration"
                                | "point"
                                | "coll"
                                | "string"
                        ) {
                            let full_name = format!("{}.{}", ns_lower, key.to_lowercase());
                            self.expect(&TokenKind::LParen)?;
                            let mut args = Vec::new();
                            if *self.peek() != TokenKind::RParen {
                                args.push(self.parse_expr()?);
                                while self.eat(&TokenKind::Comma) {
                                    args.push(self.parse_expr()?);
                                }
                            }
                            self.expect(&TokenKind::RParen)?;
                            expr = Expr::FunctionCall {
                                name: full_name,
                                distinct: false,
                                args,
                            };
                            continue;
                        }
                    }
                    // `apoc.<ns>[.<ns>...].fn(...)`: greedy 2-4 segment APOC names, committed ONLY if
                    // the chain ends in `(`, so a variable named `apoc` keeps property access
                    if let Expr::Variable(ref ns) = expr
                        && (ns.eq_ignore_ascii_case("apoc") || ns.eq_ignore_ascii_case("gds"))
                    {
                        // lookahead: (Dot Ident)* LParen
                        let mut off = 0usize;
                        loop {
                            match self.peek_offset(off) {
                                TokenKind::LParen => break,
                                // any token may be a segment (APOC uses Cypher keywords like
                                // `merge`, `create`); the real parse validates each
                                TokenKind::Dot => off += 2,
                                _ => {
                                    off = usize::MAX;
                                    break;
                                }
                            }
                        }
                        if off != usize::MAX {
                            let mut segs = vec![ns.to_ascii_lowercase(), key.to_lowercase()];
                            while *self.peek() == TokenKind::Dot {
                                self.advance();
                                segs.push(self.parse_procedure_name_segment()?.to_lowercase());
                            }
                            self.expect(&TokenKind::LParen)?;
                            let mut args = Vec::new();
                            if *self.peek() != TokenKind::RParen {
                                args.push(self.parse_expr()?);
                                while self.eat(&TokenKind::Comma) {
                                    args.push(self.parse_expr()?);
                                }
                            }
                            self.expect(&TokenKind::RParen)?;
                            let name = segs.join(".");
                            expr = if matches!(
                                name.as_str(),
                                "apoc.cypher.runfirstcolumnmany"
                                    | "apoc.cypher.runfirstcolumnsingle"
                                    | "apoc.cypher.runfirstcolumn"
                            ) {
                                desugar_run_first_column(&name, args)?
                            } else {
                                Expr::FunctionCall {
                                    name,
                                    distinct: false,
                                    args,
                                }
                            };
                            continue;
                        }
                    }
                    // two-level call `vector.similarity.cosine(...)`: after `vector`+`.similarity`
                    // another Dot; peek past symbol+`(` for a 3-segment call
                    if *self.peek() == TokenKind::Dot
                        && let Expr::Variable(ref ns) = expr
                    {
                        let ns_lower = ns.to_lowercase();
                        if ns_lower == "vector" && key.to_lowercase() == "similarity" {
                            self.advance(); // consume second Dot
                            let leaf = self.parse_symbolic_name()?;
                            let full_name = format!("vector.similarity.{}", leaf.to_lowercase());
                            self.expect(&TokenKind::LParen)?;
                            let mut args = Vec::new();
                            if *self.peek() != TokenKind::RParen {
                                args.push(self.parse_expr()?);
                                while self.eat(&TokenKind::Comma) {
                                    args.push(self.parse_expr()?);
                                }
                            }
                            self.expect(&TokenKind::RParen)?;
                            expr = Expr::FunctionCall {
                                name: full_name,
                                distinct: false,
                                args,
                            };
                            continue;
                        }
                    }
                    expr = Expr::Property(Box::new(expr), key);
                }
                // label check `n:Label`, `n:A:B`, and Cypher 5+ `n:A&B`, `n:A|B`, `n:!A`, `n:%`,
                // grouping
                TokenKind::Colon if matches!(expr, Expr::Variable(_)) => {
                    self.advance(); // consume ':'
                    let term = self.parse_label_expression()?;
                    let var = match &expr {
                        Expr::Variable(v) => v.clone(),
                        _ => unreachable!(),
                    };
                    expr = term.node_pred(&var);
                }
                // map projection expr {.prop, .*, var, key: expr} (all four Neo4j entry kinds);
                // lookahead after `{` for `.`, `ident :`, `ident ,` or `ident }` tells it from
                // other brace constructs
                TokenKind::LBrace if matches!(expr, Expr::Variable(_)) => {
                    let starts_projection = matches!(self.peek_offset(1), TokenKind::Dot)
                        || (matches!(self.peek_offset(1), TokenKind::Ident(_))
                            && matches!(
                                self.peek_offset(2),
                                TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace
                            ));
                    if starts_projection {
                        self.advance(); // consume '{'
                        let mut properties = Vec::new();
                        let mut all_properties = false;
                        let mut literal_entries: Vec<(String, Expr)> = Vec::new();
                        loop {
                            if self.eat(&TokenKind::Dot) {
                                if self.eat(&TokenKind::Star) {
                                    all_properties = true;
                                } else {
                                    properties.push(self.parse_symbolic_name()?);
                                }
                            } else {
                                let key = self.parse_symbolic_name()?;
                                if self.eat(&TokenKind::Colon) {
                                    // `key: expr`
                                    let v = self.parse_expr()?;
                                    literal_entries.push((key, v));
                                } else {
                                    // bare variable selector: `x` == `x: x`
                                    literal_entries.push((key.clone(), Expr::Variable(key)));
                                }
                            }
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(&TokenKind::RBrace)?;
                        expr = Expr::MapProjection {
                            expr: Box::new(expr),
                            properties,
                            all_properties,
                            literal_entries,
                        };
                        continue;
                    }
                    break;
                }
                TokenKind::LBracket => {
                    self.advance();
                    // index or slice
                    if self.peek() == &TokenKind::DotDot {
                        // [..end]
                        self.advance();
                        let end = self.parse_expr()?;
                        self.expect(&TokenKind::RBracket)?;
                        expr = Expr::ListSlice(Box::new(expr), None, Some(Box::new(end)));
                    } else {
                        let idx = self.parse_expr()?;
                        if self.eat(&TokenKind::DotDot) {
                            // [start..end] or [start..]
                            let end = if *self.peek() == TokenKind::RBracket {
                                None
                            } else {
                                Some(Box::new(self.parse_expr()?))
                            };
                            self.expect(&TokenKind::RBracket)?;
                            expr = Expr::ListSlice(Box::new(expr), Some(Box::new(idx)), end);
                        } else {
                            self.expect(&TokenKind::RBracket)?;
                            expr = Expr::ListIndex(Box::new(expr), Box::new(idx));
                        }
                    }
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_atom(&mut self) -> Result<Expr, CypherError> {
        match self.peek().clone() {
            // literals
            TokenKind::Integer(n) => {
                self.advance();
                Ok(Expr::Literal(Literal::Integer(n)))
            }
            TokenKind::OverflowInt(ref text) => {
                let text = text.clone();
                self.advance();
                Err(CypherError::syntax(
                    format!("integer literal out of range: {text}"),
                    self.pos.saturating_sub(1),
                ))
            }
            TokenKind::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Literal::Float(f)))
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Expr::Literal(Literal::String(s)))
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(false)))
            }
            TokenKind::Null => {
                self.advance();
                Ok(Expr::Literal(Literal::Null))
            }

            // parameter
            TokenKind::Parameter(name) => {
                self.advance();
                Ok(Expr::Parameter(name))
            }

            // parenthesized expression or pattern predicate
            TokenKind::LParen => {
                // `()` and `(:Label)` are never expressions, so a predicate starting with one is a
                // pattern (`NOT ()-[:KNOWS]->(b)`)
                if matches!(self.peek_offset(1), TokenKind::RParen | TokenKind::Colon) {
                    return self.parse_pattern_predicate();
                }

                let saved_pos = self.pos;
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;

                // pattern predicate (n)-[...]->() / (n)<-[]-()?
                if self.is_rel_start() {
                    // backtrack and reparse as a pattern element -> PatternPredicate
                    self.pos = saved_pos;
                    self.parse_pattern_predicate()
                } else {
                    Ok(expr)
                }
            }

            // list literal, list comprehension or pattern comprehension
            TokenKind::LBracket => {
                // list comprehension [x IN expr ...]?
                if matches!(self.peek_offset(1), TokenKind::Ident(_))
                    && matches!(self.peek_offset(2), TokenKind::In)
                {
                    return self.parse_list_comprehension();
                }
                // keyword-as-ident too
                if is_keyword_as_ident(self.peek_offset(1))
                    && matches!(self.peek_offset(2), TokenKind::In)
                {
                    return self.parse_list_comprehension();
                }
                // pattern comprehension:
                //   [p = (pattern) ... | expr]
                //   [(pattern) ... | expr]
                if self.is_pattern_comprehension_start() {
                    return self.parse_pattern_comprehension();
                }
                self.advance();
                let mut items = Vec::new();
                if *self.peek() != TokenKind::RBracket {
                    items.push(self.parse_expr()?);
                    while self.eat(&TokenKind::Comma) {
                        items.push(self.parse_expr()?);
                    }
                }
                self.expect(&TokenKind::RBracket)?;
                Ok(Expr::ListLiteral(items))
            }

            // map literal
            TokenKind::LBrace => self.parse_map_literal_expr(),

            // CASE expression
            TokenKind::Case => self.parse_case_expr(),

            // COUNT { subquery }, count(...) or count as a variable
            TokenKind::Count => {
                if matches!(self.peek_offset(1), TokenKind::LBrace) {
                    self.advance(); // count
                    self.advance(); // {
                    let match_clause = self.parse_inner_match()?;
                    self.expect(&TokenKind::RBrace)?;
                    Ok(Expr::CountSubquery(Box::new(match_clause)))
                } else if matches!(self.peek_offset(1), TokenKind::LParen) {
                    self.parse_function_call()
                } else {
                    // count as a variable
                    let name = self.parse_symbolic_name()?;
                    Ok(Expr::Variable(name))
                }
            }

            // EXISTS { subquery }, exists(...) or exists as a variable
            TokenKind::Exists => {
                if matches!(self.peek_offset(1), TokenKind::LBrace) {
                    self.advance(); // exists
                    self.advance(); // {
                    // full subquery first (WITH/RETURN support)
                    let subquery = self.parse_subquery_body()?;
                    self.expect(&TokenKind::RBrace)?;
                    if subquery.parts.is_empty() {
                        // no WITH: simple form (RETURN ignored)
                        let clause = &subquery.final_part.clauses;
                        if let Some(Clause::Match(mc)) = clause.first() {
                            Ok(Expr::ExistentialSubquery(Box::new(mc.clone())))
                        } else {
                            Ok(Expr::FullExistentialSubquery(Box::new(subquery)))
                        }
                    } else {
                        Ok(Expr::FullExistentialSubquery(Box::new(subquery)))
                    }
                } else if matches!(self.peek_offset(1), TokenKind::LParen) {
                    // `exists((a)-[:R]->(b))` is the legacy pattern predicate. exists() is a null
                    // test and Bool(false) isn't null, so leaving the call would make every such
                    // pattern true; unwrap to the predicate, like the bare pattern and `EXISTS { ...
                    // }`
                    let call = self.parse_function_call()?;
                    if let Expr::FunctionCall {
                        ref name, ref args, ..
                    } = call
                        && name == "exists"
                        && args.len() == 1
                        && matches!(
                            args[0],
                            Expr::PatternPredicate(_) | Expr::ExistentialSubquery(_)
                        )
                    {
                        return Ok(args[0].clone());
                    }
                    Ok(call)
                } else {
                    let name = self.parse_symbolic_name()?;
                    Ok(Expr::Variable(name))
                }
            }

            // ALL: quantifier or keyword-as-ident
            TokenKind::All => {
                if matches!(self.peek_offset(1), TokenKind::LParen)
                    && (matches!(self.peek_offset(2), TokenKind::Ident(_))
                        || is_keyword_as_ident(self.peek_offset(2)))
                    && matches!(self.peek_offset(3), TokenKind::In)
                {
                    self.advance(); // consume ALL
                    self.parse_quantifier_expr(QuantifierKind::All)
                } else {
                    // fall through to keyword-as-ident
                    let name = self.parse_symbolic_name()?;
                    Ok(Expr::Variable(name))
                }
            }

            // function call or variable
            TokenKind::Ident(_) => {
                // quantifiers: none, any, single
                if let TokenKind::Ident(ref name) = self.peek().clone() {
                    let lower = name.to_lowercase();
                    if matches!(lower.as_str(), "none" | "any" | "single")
                        && matches!(self.peek_offset(1), TokenKind::LParen)
                        && (matches!(self.peek_offset(2), TokenKind::Ident(_))
                            || is_keyword_as_ident(self.peek_offset(2)))
                        && matches!(self.peek_offset(3), TokenKind::In)
                    {
                        let kind = match lower.as_str() {
                            "none" => QuantifierKind::None,
                            "any" => QuantifierKind::Any,
                            "single" => QuantifierKind::Single,
                            _ => unreachable!(),
                        };
                        self.advance(); // consume the ident
                        return self.parse_quantifier_expr(kind);
                    }
                }
                // reduce(acc = init, var IN list | expr)
                if let TokenKind::Ident(ref name) = self.peek().clone()
                    && name.to_lowercase() == "reduce"
                    && matches!(self.peek_offset(1), TokenKind::LParen)
                {
                    self.advance(); // consume "reduce"
                    return self.parse_reduce_expr();
                }
                // `COLLECT { ... }` subquery, not the `collect(...)` aggregate: `collect` + `{` is
                // the subquery (like EXISTS / COUNT), `collect` + `(` the function call
                if let TokenKind::Ident(ref name) = self.peek().clone()
                    && name.eq_ignore_ascii_case("collect")
                    && matches!(self.peek_offset(1), TokenKind::LBrace)
                {
                    self.advance(); // consume `collect`
                    self.advance(); // consume `{`
                    let subquery = self.parse_subquery_body()?;
                    self.expect(&TokenKind::RBrace)?;
                    return Ok(Expr::CollectSubquery(Box::new(subquery)));
                }
                if matches!(self.peek_offset(1), TokenKind::LParen) {
                    self.parse_function_call()
                } else {
                    let name = self.parse_symbolic_name()?;
                    Ok(Expr::Variable(name))
                }
            }

            // keywords usable as variable names
            ref kw if is_keyword_as_ident(kw) => {
                let name = self.parse_symbolic_name()?;
                Ok(Expr::Variable(name))
            }

            _ => Err(CypherError::syntax(
                format!("unexpected token in expression: {:?}", self.peek()),
                self.span_pos(),
            )),
        }
    }

    fn parse_function_call(&mut self) -> Result<Expr, CypherError> {
        let name = match self.peek().clone() {
            TokenKind::Ident(n) => {
                self.advance();
                n.to_lowercase()
            }
            TokenKind::Count => {
                self.advance();
                "count".to_string()
            }
            TokenKind::Exists => {
                self.advance();
                "exists".to_string()
            }
            _ => {
                return Err(CypherError::syntax(
                    format!("expected function name, found {:?}", self.peek()),
                    self.span_pos(),
                ));
            }
        };

        // Cypher 25 GQL aggregate aliases (Neo4j 2026.02) normalize to canonical names, so
        // validation / planning / execution are unchanged
        let name = normalize_agg_alias(name);

        self.expect(&TokenKind::LParen)?;

        let distinct = self.eat_keyword(&TokenKind::Distinct);

        // count(*)
        if name == "count" && self.eat(&TokenKind::Star) {
            self.expect(&TokenKind::RParen)?;
            return Ok(Expr::FunctionCall {
                name,
                distinct,
                args: Vec::new(),
            });
        }

        let mut args = Vec::new();
        if *self.peek() != TokenKind::RParen {
            args.push(self.parse_expr()?);
            while self.eat(&TokenKind::Comma) {
                args.push(self.parse_expr()?);
            }
        }

        self.expect(&TokenKind::RParen)?;

        // `vector(list, dim, FLOAT32)` (Cypher 25): the coordinate type is a bare keyword that
        // would parse as an unbound variable; rewrite to a string literal
        if name == "vector" {
            for a in args.iter_mut() {
                if let Expr::Variable(v) = a {
                    let up = v.to_uppercase();
                    if matches!(
                        up.as_str(),
                        "FLOAT32"
                            | "FLOAT64"
                            | "INTEGER8"
                            | "INTEGER16"
                            | "INTEGER32"
                            | "INTEGER64"
                            | "INT8"
                            | "INT16"
                            | "INT32"
                            | "INT64"
                    ) {
                        *a = Expr::Literal(Literal::String(up));
                    }
                }
            }
        }

        Ok(Expr::FunctionCall {
            name,
            distinct,
            args,
        })
    }

    pub(super) fn parse_map_literal_expr(&mut self) -> Result<Expr, CypherError> {
        self.expect(&TokenKind::LBrace)?;
        let mut entries = Vec::new();
        if *self.peek() != TokenKind::RBrace {
            let key = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            let value = self.parse_expr()?;
            entries.push((key, value));
            while self.eat(&TokenKind::Comma) {
                let key = self.parse_symbolic_name()?;
                self.expect(&TokenKind::Colon)?;
                let value = self.parse_expr()?;
                entries.push((key, value));
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(Expr::MapLiteral(entries))
    }

    pub(super) fn parse_case_expr(&mut self) -> Result<Expr, CypherError> {
        self.expect(&TokenKind::Case)?;

        // simple CASE (CASE expr WHEN ...) vs searched (CASE WHEN ...)
        let operand = if *self.peek() != TokenKind::When {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };

        let mut whens = Vec::new();
        while self.eat_keyword(&TokenKind::When) {
            let when = self.parse_expr()?;
            self.expect(&TokenKind::Then)?;
            let then = self.parse_expr()?;
            whens.push((when, then));
        }

        let else_expr = if self.eat_keyword(&TokenKind::Else) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };

        self.expect(&TokenKind::End)?;
        Ok(Expr::Case {
            operand,
            whens,
            else_expr,
        })
    }

    pub(super) fn parse_list_comprehension(&mut self) -> Result<Expr, CypherError> {
        self.expect(&TokenKind::LBracket)?;
        let variable = self.parse_symbolic_name()?;
        self.expect(&TokenKind::In)?;
        let list_expr = self.parse_expr()?;

        let filter = if self.eat_keyword(&TokenKind::Where) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };

        let map_expr = if self.eat(&TokenKind::Pipe) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };

        self.expect(&TokenKind::RBracket)?;
        Ok(Expr::ListComprehension {
            variable,
            list_expr: Box::new(list_expr),
            filter,
            map_expr,
        })
    }

    fn is_pattern_comprehension_start(&self) -> bool {
        // form 1: [ident = ( ...
        if (matches!(self.peek_offset(1), TokenKind::Ident(_))
            || is_keyword_as_ident(self.peek_offset(1)))
            && matches!(self.peek_offset(2), TokenKind::Eq)
            && matches!(self.peek_offset(3), TokenKind::LParen)
        {
            return true;
        }
        // form 2: [( ... ) followed by a relationship start (- or <)
        if !matches!(self.peek_offset(1), TokenKind::LParen) {
            return false;
        }
        // scan past the first node pattern's closing )
        let mut depth = 0;
        let mut i = 1;
        loop {
            let tok = self.peek_offset(i);
            match tok {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        // relationship start next?
                        let next = self.peek_offset(i + 1);
                        return matches!(next, TokenKind::Minus | TokenKind::Lt);
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
            if i > 50 {
                return false;
            }
        }
    }

    /// Pattern used as a predicate (`(a)-[:R]->(b)`, `()-[:R]->(b)`, `(:L)-[:R]->(b)`), parsed from
    /// the opening paren and wrapped as `PatternPredicate`.
    fn parse_pattern_predicate(&mut self) -> Result<Expr, CypherError> {
        let element = self.parse_pattern_element()?;
        let pattern = Pattern {
            parts: vec![PatternPart {
                variable: None,
                element,
                shortest_path: ShortestPathMode::None,
                weight_property: None,
                total_cost_var: None,
                path_mode: crate::ast::PathMode::Trail,
                qpp_list_groups: Vec::new(),
                path_selector_limit: None,
            }],
        };
        Ok(Expr::PatternPredicate(Box::new(MatchClause {
            pattern,
            where_clause: None,
            hints: Vec::new(),
            search: None,
        })))
    }

    fn parse_pattern_comprehension(&mut self) -> Result<Expr, CypherError> {
        self.expect(&TokenKind::LBracket)?;

        // optional path variable: p = ...
        let variable = if (matches!(self.peek(), TokenKind::Ident(_))
            || is_keyword_as_ident(self.peek()))
            && matches!(self.peek_offset(1), TokenKind::Eq)
        {
            let name = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Eq)?;
            Some(name)
        } else {
            None
        };

        // single pattern element, not comma-separated
        let element = self.parse_pattern_element()?;
        let pattern = Pattern {
            parts: vec![PatternPart {
                variable: None,
                element,
                shortest_path: ShortestPathMode::None,
                weight_property: None,
                total_cost_var: None,
                path_mode: crate::ast::PathMode::Trail,
                qpp_list_groups: Vec::new(),
                path_selector_limit: None,
            }],
        };

        // optional WHERE
        let where_clause = if self.eat_keyword(&TokenKind::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        // pipe + projection
        self.expect(&TokenKind::Pipe)?;
        let projection = self.parse_expr()?;

        self.expect(&TokenKind::RBracket)?;

        Ok(Expr::PatternComprehension {
            variable,
            match_clause: Box::new(MatchClause {
                pattern,
                where_clause,
                hints: Vec::new(),
                search: None,
            }),
            projection: Box::new(projection),
        })
    }

    fn parse_quantifier_expr(&mut self, kind: QuantifierKind) -> Result<Expr, CypherError> {
        self.expect(&TokenKind::LParen)?;
        let variable = self.parse_symbolic_name()?;
        self.expect(&TokenKind::In)?;
        let list_expr = self.parse_expr()?;
        self.expect(&TokenKind::Where)?;
        let predicate = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::QuantifierExpr {
            kind,
            variable,
            list_expr: Box::new(list_expr),
            predicate: Box::new(predicate),
        })
    }

    /// `reduce(accumulator = initial, variable IN list | expression)`.
    fn parse_reduce_expr(&mut self) -> Result<Expr, CypherError> {
        self.expect(&TokenKind::LParen)?;
        let accumulator = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Eq)?;
        let initial = self.parse_expr()?;
        self.expect(&TokenKind::Comma)?;
        let variable = self.parse_symbolic_name()?;
        self.expect(&TokenKind::In)?;
        let list_expr = self.parse_expr()?;
        self.expect(&TokenKind::Pipe)?;
        let update_expr = self.parse_expr()?;
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::Reduce {
            accumulator,
            initial: Box::new(initial),
            variable,
            list_expr: Box::new(list_expr),
            update_expr: Box::new(update_expr),
        })
    }

    #[recursive::recursive]
    fn parse_subquery_body(&mut self) -> Result<SingleQuery, CypherError> {
        self.enter_nesting()?;
        let result = self.parse_subquery_body_inner();
        self.leave_nesting();
        result
    }

    fn parse_subquery_body_inner(&mut self) -> Result<SingleQuery, CypherError> {
        let mut parts = Vec::new();
        let mut current_clauses = Vec::new();
        // MATCH is optional in EXISTS/COUNT subqueries
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
                TokenKind::Unwind => {
                    // bodies may lead with UNWIND (`COLLECT { UNWIND [...] AS x RETURN x }`); Neo4j
                    // accepts any query as a subquery body
                    let clause = self.parse_unwind_clause()?;
                    current_clauses.push(Clause::Unwind(clause));
                }
                TokenKind::RBrace | TokenKind::Eof => {
                    return Ok(SingleQuery {
                        parts,
                        final_part: FinalPart {
                            clauses: current_clauses,
                            return_clause: None,
                        },
                    });
                }
                _ => {
                    // bare pattern without MATCH?
                    if current_clauses.is_empty() && parts.is_empty() {
                        self.eat_keyword(&TokenKind::Match);
                        let pattern = self.parse_pattern()?;
                        let where_clause = if self.eat_keyword(&TokenKind::Where) {
                            Some(self.parse_expr()?)
                        } else {
                            None
                        };
                        current_clauses.push(Clause::Match(MatchClause {
                            pattern,
                            where_clause,
                            hints: Vec::new(),
                            search: None,
                        }));
                    } else {
                        return Err(CypherError::syntax(
                            format!("unexpected token in subquery: {:?}", self.peek()),
                            self.span_pos(),
                        ));
                    }
                }
            }
        }
    }

    fn parse_inner_match(&mut self) -> Result<MatchClause, CypherError> {
        // MATCH is optional in EXISTS { ... } and COUNT { ... }
        self.eat_keyword(&TokenKind::Match);
        let pattern = self.parse_pattern()?;
        let where_clause = if self.eat_keyword(&TokenKind::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        // full existential subquery: consume the trailing RETURN (only existence matters)
        if self.eat_keyword(&TokenKind::Return) {
            let _discard = self.parse_expr()?;
        }
        Ok(MatchClause {
            pattern,
            where_clause,
            hints: Vec::new(),
            search: None,
        })
    }
}
