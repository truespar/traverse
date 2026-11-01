//! Schema paths and graph type bodies: element types, labels, property declarations and
//! value types.

use crate::ast::{GraphTypeBody, SchemaPath};
use crate::error::ParseError;
use crate::lexer::{Span, TokenKind};

use super::{Parser, describe_token};

impl<'t> Parser<'t> {
    /// Parse a `/foo/bar` or `name` catalog path. Leading `/` makes
    /// the path absolute; subsequent `/` separators chain segments.
    ///
    /// Uses a stricter segment parser than `expect_name` so reserved
    /// words like `ANY` aren't swallowed as path names - the body
    /// keywords need to remain visible to the caller for dispatch.
    pub(super) fn parse_schema_path(&mut self) -> Result<SchemaPath, ParseError> {
        let start = self.peek().span.start;
        let absolute = self.consume_if(&TokenKind::Slash);
        let mut segments = vec![self.expect_path_segment()?];
        while self.consume_if(&TokenKind::Slash) {
            segments.push(self.expect_path_segment()?);
        }
        let end = self.previous_end();
        Ok(SchemaPath {
            absolute,
            segments,
            span: Span::new(start, end),
        })
    }

    /// Consumes an `Ident` or `DelimitedIdent`, rejecting reserved keywords, so the catalog path
    /// parser's caller can still dispatch on the keyword.
    fn expect_path_segment(&mut self) -> Result<String, ParseError> {
        match &self.peek().kind {
            TokenKind::Ident(n) | TokenKind::DelimitedIdent(n) => {
                let s = n.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(ParseError::unexpected_token(
                self.peek().span,
                format!(
                    "expected a path segment name, found {}",
                    describe_token(&self.peek().kind)
                ),
            )),
        }
    }

    /// Could the next token start a graph-type body? Tells the bare Cypher `CREATE GRAPH name` from
    /// the GQL form with an explicit body.
    pub(super) fn peek_can_start_graph_body(&self) -> bool {
        match &self.peek().kind {
            TokenKind::LBrace => true,
            TokenKind::Keyword(k) if *k == "any" || *k == "like" => true,
            // reference to another graph type: bare identifier right after the path
            TokenKind::Ident(_) | TokenKind::DelimitedIdent(_) => true,
            _ => false,
        }
    }

    /// Stricter [`peek_can_start_graph_body`]: only the reserved body keywords (`ANY`, `LIKE`) or
    /// an opening brace. Detects the headless `CREATE GRAPH <body>` form with no graph name. Bare
    /// identifiers are excluded: ambiguous (graph name or type reference), and the spec resolves
    /// them as the named form.
    pub(super) fn peek_starts_graph_body_keyword(&self) -> bool {
        match &self.peek().kind {
            TokenKind::LBrace => true,
            TokenKind::Keyword(k) if *k == "any" || *k == "like" => true,
            _ => false,
        }
    }

    /// Graph-type body: `ANY`, an inline `{...}`, a type reference, `LIKE other`, or `<spec> AS COPY
    /// OF other`.
    pub(super) fn parse_graph_type_body(&mut self) -> Result<GraphTypeBody, ParseError> {
        let start = self.peek().span.start;
        let mut body = self.parse_graph_type_body_atom()?;

        // a trailing `AS COPY OF source` wraps whatever was parsed
        if self.peek_keyword("as") {
            self.advance();
            self.expect_keyword("copy")?;
            self.expect_keyword("of")?;
            let source = self.parse_schema_path()?;
            let end = self.previous_end();
            body = GraphTypeBody::Copy {
                type_part: Some(Box::new(body)),
                source,
                span: Span::new(start, end),
            };
        }
        Ok(body)
    }

    fn parse_graph_type_body_atom(&mut self) -> Result<GraphTypeBody, ParseError> {
        let start = self.peek().span.start;
        if self.peek_keyword("any") {
            let span = self.peek().span;
            self.advance();
            return Ok(GraphTypeBody::Open { span });
        }
        if self.peek_keyword("like") {
            self.advance();
            let source = self.parse_schema_path()?;
            let end = self.previous_end();
            return Ok(GraphTypeBody::Like {
                source,
                span: Span::new(start, end),
            });
        }
        if matches!(self.peek().kind, TokenKind::LBrace) {
            return self.parse_inline_graph_type_body();
        }
        // otherwise a graph-type reference (a path)
        let name = self.parse_schema_path()?;
        let end = self.previous_end();
        Ok(GraphTypeBody::Reference {
            name,
            span: Span::new(start, end),
        })
    }

    /// Parses `{ ... }`. Tries structured element-type parsing first; on any deviation from ISO 39075
    /// section 17.2's canonical grammar (NODE / VERTEX / EDGE / RELATIONSHIP declarations with optional
    /// labels and property-type lists) it falls back to brace counting, recording only the spans,
    /// so uncodified bodies still round-trip.
    fn parse_inline_graph_type_body(&mut self) -> Result<GraphTypeBody, ParseError> {
        let outer_start = self.peek().span.start;
        self.expect_kind(TokenKind::LBrace, "expected `{` to open graph-type body")?;
        let body_start = self.previous_end();
        // body start, to restore the cursor if structured parsing fails part-way
        let restore_pos = self.cursor;

        // empty `{}` is a valid open graph type
        if matches!(self.peek().kind, TokenKind::RBrace) {
            let body_end = self.peek().span.start;
            self.advance();
            let outer_end = self.previous_end();
            return Ok(GraphTypeBody::Inline {
                body_span: Span::new(body_start, body_end),
                span: Span::new(outer_start, outer_end),
                elements: Some(Vec::new()),
            });
        }

        // structured element-type list
        if let Some(els) = self.try_parse_element_type_list() {
            if matches!(self.peek().kind, TokenKind::RBrace) {
                let body_end = self.peek().span.start;
                self.advance();
                let outer_end = self.previous_end();
                return Ok(GraphTypeBody::Inline {
                    body_span: Span::new(body_start, body_end),
                    span: Span::new(outer_start, outer_end),
                    elements: Some(els),
                });
            }
            // trailing content after a good element list: not fully canonical, fall back
            self.cursor = restore_pos;
        } else {
            // no clean element list: fall back
            self.cursor = restore_pos;
        }

        // brace counting
        let mut depth: u32 = 1;
        let body_end: usize;
        loop {
            if self.is_eof() {
                return Err(ParseError::unexpected_eof(
                    self.peek().span,
                    "unterminated graph-type body - expected `}`",
                ));
            }
            match self.peek().kind {
                TokenKind::LBrace => {
                    depth += 1;
                    self.advance();
                }
                TokenKind::RBrace => {
                    if depth == 1 {
                        body_end = self.peek().span.start;
                        self.advance();
                        break;
                    }
                    depth -= 1;
                    self.advance();
                }
                _ => {
                    self.advance();
                }
            }
        }
        let outer_end = self.previous_end();
        Ok(GraphTypeBody::Inline {
            body_span: Span::new(body_start, body_end),
            span: Span::new(outer_start, outer_end),
            elements: None,
        })
    }

    /// Best-effort comma-separated element-type list (ISO 39075 section 17.2). `Some(elements)` when every
    /// segment parses cleanly; `None` at the first deviation, so the caller can fall back to brace
    /// counting without losing position.
    fn try_parse_element_type_list(&mut self) -> Option<Vec<crate::ast::GraphTypeElement>> {
        let mut out = Vec::new();
        loop {
            let el = self.try_parse_element_type()?;
            out.push(el);
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        Some(out)
    }

    fn try_parse_element_type(&mut self) -> Option<crate::ast::GraphTypeElement> {
        let start = self.peek().span.start;
        // parenthesized node-type pattern (ISO 39075 section 17.2), `( [name] [:L[&L...]] [{props}] )`, the
        // TCK's form throughout; edge-type patterns (`(:A)-[:R]->(:B)`) and key-label-set forms
        // (`:A => :B`) go to the opaque brace-counted body
        if matches!(self.peek().kind, TokenKind::LParen) {
            let save = self.cursor;
            self.advance();
            let name = match &self.peek().kind {
                TokenKind::Ident(s) => {
                    let s = s.clone();
                    self.advance();
                    Some(s)
                }
                _ => None,
            };
            let labels = self.try_parse_element_label_clause()?;
            // anything but a property spec or the closing paren (`=>` key label sets, ...) isn't
            // structured: back off to the opaque path
            if !matches!(self.peek().kind, TokenKind::LBrace | TokenKind::RParen) {
                self.cursor = save;
                return None;
            }
            let properties = self.try_parse_element_property_spec()?;
            if !matches!(self.peek().kind, TokenKind::RParen) {
                self.cursor = save;
                return None;
            }
            self.advance();
            // An edge-pattern continuation means this parenthesis was
            // an endpoint of an edge-type pattern, not a node type.
            if matches!(
                self.peek().kind,
                TokenKind::Minus | TokenKind::Arrow | TokenKind::LeftArrow | TokenKind::Lt
            ) {
                self.cursor = save;
                return None;
            }
            let end = self.previous_end();
            return Some(crate::ast::GraphTypeElement::Node {
                name,
                labels,
                properties,
                pattern_form: true,
                span: Span::new(start, end),
            });
        }
        // Optional leading `ANY` keyword (e.g. `ANY NODE`, `ANY EDGE`).
        if self.peek_keyword("any") {
            self.advance();
        }
        let kind = if self.peek_keyword("node") || self.peek_keyword("vertex") {
            self.advance();
            "node"
        } else if self.peek_keyword("edge") {
            self.advance();
            "edge"
        } else if self.peek_keyword("relationship") {
            self.advance();
            "relationship"
        } else {
            return None;
        };

        // Optional element-type name (a plain identifier; not a
        // reserved keyword).
        let name = if matches!(self.peek().kind, TokenKind::Ident(_)) {
            match &self.peek().kind {
                TokenKind::Ident(s) => {
                    let s = s.clone();
                    self.advance();
                    Some(s)
                }
                _ => None,
            }
        } else {
            None
        };

        let labels = self.try_parse_element_label_clause()?;
        let properties = self.try_parse_element_property_spec()?;

        let end = self.previous_end();
        let span = Span::new(start, end);
        Some(match kind {
            "node" => crate::ast::GraphTypeElement::Node {
                name,
                labels,
                properties,
                pattern_form: false,
                span,
            },
            "edge" => crate::ast::GraphTypeElement::Edge {
                kind: crate::ast::EdgeKind::Edge,
                name,
                labels,
                properties,
                span,
            },
            "relationship" => crate::ast::GraphTypeElement::Edge {
                kind: crate::ast::EdgeKind::Relationship,
                name,
                labels,
                properties,
                span,
            },
            _ => unreachable!(),
        })
    }

    /// Optional label clause `:Label[&Label ...]` on an element type.
    /// Returns an empty Vec when no clause is present.
    fn try_parse_element_label_clause(&mut self) -> Option<Vec<String>> {
        let mut labels = Vec::new();
        if matches!(self.peek().kind, TokenKind::Colon) {
            self.advance();
            let lbl = self.expect_name().ok()?;
            labels.push(lbl);
            while matches!(self.peek().kind, TokenKind::Ampersand) {
                self.advance();
                let lbl = self.expect_name().ok()?;
                labels.push(lbl);
            }
        }
        Some(labels)
    }

    /// Optional property-type spec `{ propName TYPE [NOT NULL], ... }`
    /// on an element type. Returns an empty Vec when no spec is
    /// present.
    fn try_parse_element_property_spec(&mut self) -> Option<Vec<crate::ast::PropertyDecl>> {
        let mut properties = Vec::new();
        if matches!(self.peek().kind, TokenKind::LBrace) {
            self.advance();
            if !matches!(self.peek().kind, TokenKind::RBrace) {
                loop {
                    let pd = self.try_parse_property_decl()?;
                    properties.push(pd);
                    if matches!(self.peek().kind, TokenKind::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                }
            }
            if !matches!(self.peek().kind, TokenKind::RBrace) {
                return None;
            }
            self.advance();
        }
        Some(properties)
    }

    fn try_parse_property_decl(&mut self) -> Option<crate::ast::PropertyDecl> {
        let start = self.peek().span.start;
        let name = self.expect_name().ok()?;
        // type separator optional: ISO 39075 section 17.2 writes `propName TYPE`, `::` appears in typed
        // expressions, Neo4j uses a bare `:`; all three accepted
        match self.peek().kind {
            TokenKind::ColonColon | TokenKind::Colon => {
                self.advance();
            }
            _ => {}
        }
        let value_type = self.try_parse_value_type_name()?;
        let mut not_null = false;
        if self.peek_keyword("not") {
            // save in case of backtrack
            let save = self.cursor;
            self.advance();
            if self.peek_keyword("null") {
                self.advance();
                not_null = true;
            } else {
                self.cursor = save;
            }
        }
        let end = self.previous_end();
        Some(crate::ast::PropertyDecl {
            name,
            value_type,
            not_null,
            span: Span::new(start, end),
        })
    }

    fn try_parse_value_type_name(&mut self) -> Option<crate::ast::ValueTypeName> {
        use crate::ast::ValueTypeName as V;
        // per ISO section 16.10 a value type is one predefined type or a `|`-separated union; parse the
        // first leaf, then greedily collect alternatives (one leaf -> `V::Boolean`/`V::Integer`/...,
        // several -> `V::Union`)
        let first = self.try_parse_value_type_leaf()?;
        if !matches!(self.peek().kind, TokenKind::Pipe) {
            return Some(first);
        }
        let mut alts = vec![first];
        while self.consume_if(&TokenKind::Pipe) {
            // not a type after `|`: un-consume the `|` for the outer parser, so a dangling
            // `propName :: INTEGER |` is refused
            let save = self.cursor;
            match self.try_parse_value_type_leaf() {
                Some(t) => alts.push(t),
                None => {
                    self.cursor = save - 1; // un-consume the `|`
                    break;
                }
            }
        }
        if alts.len() == 1 {
            Some(alts.into_iter().next().unwrap())
        } else {
            Some(V::Union(alts))
        }
    }

    fn try_parse_value_type_leaf(&mut self) -> Option<crate::ast::ValueTypeName> {
        use crate::ast::ValueTypeName as V;
        // Multi-word forms first.
        if self.peek_keyword("signed") {
            self.advance();
            if self.peek_keyword("integer") {
                self.advance();
                return Some(V::Integer);
            }
            // Bare `SIGNED` alone is unusual; restore.
            return None;
        }
        if self.peek_keyword("local") {
            self.advance();
            if self.peek_keyword("time") {
                self.advance();
                return Some(V::LocalTime);
            }
            if self.peek_keyword("datetime") {
                self.advance();
                return Some(V::LocalDatetime);
            }
            return None;
        }
        if self.peek_keyword("zoned") {
            self.advance();
            if self.peek_keyword("time") {
                self.advance();
                return Some(V::ZonedTime);
            }
            if self.peek_keyword("datetime") {
                self.advance();
                return Some(V::ZonedDatetime);
            }
            return None;
        }
        // Single-word forms.
        let v = if self.peek_keyword("boolean") || self.peek_keyword("bool") {
            V::Boolean
        } else if self.peek_keyword("integer") || self.peek_keyword("int") {
            V::Integer
        } else if self.peek_keyword("float") {
            V::Float
        } else if self.peek_keyword("string") || self.peek_keyword("varchar") {
            V::String
        } else if self.peek_keyword("date") {
            V::Date
        } else if self.peek_keyword("local_time") {
            V::LocalTime
        } else if self.peek_keyword("local_datetime") {
            V::LocalDatetime
        } else if self.peek_keyword("zoned_time") || self.peek_keyword("time") {
            V::ZonedTime
        } else if self.peek_keyword("zoned_datetime") || self.peek_keyword("datetime") {
            V::ZonedDatetime
        } else if self.peek_keyword("duration") {
            V::Duration
        } else if self.peek_keyword("any") {
            V::Any
        } else {
            return None;
        };
        self.advance();
        Some(v)
    }
}
