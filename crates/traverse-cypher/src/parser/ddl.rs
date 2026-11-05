//! Schema DDL: CREATE / DROP INDEX (range, full-text, point, text, vector, relationship),
//! constraints, and ANALYZE GRAPH.

use crate::ast::Query;
use crate::error::CypherError;
use crate::lexer::TokenKind;

use super::Parser;

impl Parser {
    /// DDL object name: a symbolic name or a `$parameter` (Cypher 25 dynamic DDL
    /// names). Parameters become a marker `\u{1}param\u{1}<name>` resolved at
    /// execution (`vex::ops::schema::resolve_ddl_name`), since DDL plans before
    /// parameter values are looked at.
    fn parse_ddl_name(&mut self) -> Result<String, CypherError> {
        if let TokenKind::Parameter(pname) = self.peek().clone() {
            self.advance();
            return Ok(format!("\u{1}param\u{1}{pname}"));
        }
        self.parse_symbolic_name()
    }

    /// CREATE INDEX name [IF NOT EXISTS] FOR (n:Label) ON (n.property)   -- Neo4j 4.x+
    /// CREATE INDEX ON :Label(property)                                  -- Memgraph / Neo4j 3.x
    pub(super) fn parse_create_index(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Create)?;
        self.expect(&TokenKind::Index)?;

        // Memgraph syntax: CREATE INDEX ON :Label(property)
        if *self.peek() == TokenKind::On {
            self.advance(); // ON
            self.expect(&TokenKind::Colon)?;
            let label = self.parse_symbolic_name()?;
            self.expect(&TokenKind::LParen)?;
            let property = self.parse_symbolic_name()?;
            self.expect(&TokenKind::RParen)?;
            let name = format!("idx_{label}_{property}");
            return Ok(Query::CreateIndex {
                name,
                label,
                property,
                extra_properties: Vec::new(),
            });
        }

        // Neo4j syntax: CREATE INDEX name [IF NOT EXISTS] FOR (n:Label) ON (n.p), or FOR
        // ()-[r:TYPE]-() ON (r.p)
        self.parse_index_body()
    }

    /// Index body shared by `CREATE INDEX` and `CREATE RANGE INDEX`:
    /// `name [IF NOT EXISTS] FOR <pattern> ON (var.prop[, ...])`. A node pattern
    /// `(n:Label)` gives `CreateIndex`, a relationship `()-[r:TYPE]-()` gives
    /// `CreateEdgeIndex` (direction-agnostic, like Neo4j).
    fn parse_index_body(&mut self) -> Result<Query, CypherError> {
        // the name is optional (`CREATE INDEX FOR (n:L) ON (n.p)`); Neo4j generates one, we derive
        // a deterministic one below
        let name = if *self.peek() == TokenKind::For {
            String::new()
        } else {
            self.parse_ddl_name()?
        };
        if matches!(self.peek(), TokenKind::Ident(s) if s.to_uppercase() == "IF") {
            self.advance(); // IF
            self.expect(&TokenKind::Not)?; // NOT
            self.expect(&TokenKind::Exists)?; // EXISTS
        }
        self.expect(&TokenKind::For)?;
        self.expect(&TokenKind::LParen)?;
        // node `(n:Label)` vs relationship `()-[r:TYPE]-()`: an empty `()` starts a relationship
        if *self.peek() == TokenKind::RParen {
            self.advance(); // ')': first empty node
            // left connector `-[` or `<-[` (direction-agnostic)
            self.eat(&TokenKind::Lt);
            self.expect(&TokenKind::Minus)?;
            self.expect(&TokenKind::LBracket)?;
            let _rvar = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            let edge_type = self.parse_symbolic_name()?;
            self.expect(&TokenKind::RBracket)?;
            // right connector `]-` or `]->`, then the trailing empty node
            self.expect(&TokenKind::Minus)?;
            self.eat(&TokenKind::Gt);
            self.expect(&TokenKind::LParen)?;
            self.expect(&TokenKind::RParen)?;
            let mut properties = self.parse_index_on_properties()?;
            let property = properties.remove(0);
            let name = if name.is_empty() {
                format!("index_{edge_type}_{property}")
            } else {
                name
            };
            return Ok(Query::CreateEdgeIndex {
                name,
                edge_type,
                property: Some(property),
                extra_properties: properties,
            });
        }
        let _node_var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Colon)?;
        let label = self.parse_symbolic_name()?;
        self.expect(&TokenKind::RParen)?;
        let mut properties = self.parse_index_on_properties()?;
        let property = properties.remove(0);
        let name = if name.is_empty() {
            format!("index_{label}_{property}")
        } else {
            name
        };
        Ok(Query::CreateIndex {
            name,
            label,
            property,
            extra_properties: properties,
        })
    }

    /// `ON (var.prop)` tail shared by node and relationship indexes.
    fn parse_index_on_property(&mut self) -> Result<String, CypherError> {
        let mut props = self.parse_index_on_properties()?;
        if props.len() > 1 {
            return Err(CypherError::syntax(
                "composite properties are not supported for this index type".to_string(),
                self.span_pos(),
            ));
        }
        Ok(props.remove(0))
    }

    /// `ON (var.p1 [, var.p2 ...])`: one or more keys (composite b-tree indexes take several).
    fn parse_index_on_properties(&mut self) -> Result<Vec<String>, CypherError> {
        self.expect(&TokenKind::On)?;
        self.expect(&TokenKind::LParen)?;
        let mut props = Vec::new();
        loop {
            let _prop_var = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Dot)?;
            props.push(self.parse_symbolic_name()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(props)
    }

    /// `CREATE <RANGE|TEXT|POINT|LOOKUP|FULLTEXT> INDEX ...`. RANGE is the b-tree index, TEXT the
    /// trigram index, POINT the R-tree, FULLTEXT BM25; LOOKUP gets a clear error (label/type lookup
    /// is built in).
    pub(super) fn parse_typed_create_index(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Create)?;
        let type_kw = match self.peek().clone() {
            TokenKind::Ident(s) => s.to_uppercase(),
            _ => unreachable!("dispatch guarantees an index-type identifier"),
        };
        self.advance(); // <type> keyword
        self.expect(&TokenKind::Index)?;
        match type_kw.as_str() {
            "RANGE" => self.parse_index_body(),
            "TEXT" => self.parse_create_text_index(),
            "POINT" => self.parse_create_point_index(),
            "FULLTEXT" => self.parse_create_fulltext_index(),
            "LOOKUP" => Err(CypherError::syntax(
                "LOOKUP INDEX is not required - Traverse indexes labels and \
                 relationship types automatically",
                self.span_pos(),
            )),
            _ => unreachable!(),
        }
    }

    /// DROP INDEX name
    pub(super) fn parse_drop_index(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Drop)?;
        self.expect(&TokenKind::Index)?;
        let name = self.parse_ddl_name()?;
        // optional IF EXISTS
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF")) {
            self.advance();
            self.expect(&TokenKind::Exists)?;
        }
        Ok(Query::DropIndex { name })
    }

    /// `CREATE FULLTEXT INDEX name [IF NOT EXISTS] FOR (n:L1|L2) ON EACH
    /// [n.p1, n.p2] [OPTIONS { indexConfig: { 'fulltext.analyzer': 'english' }}]`.
    /// The dispatcher already consumed `CREATE FULLTEXT INDEX`.
    fn parse_create_fulltext_index(&mut self) -> Result<Query, CypherError> {
        let name = self.parse_ddl_name()?;
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF")) {
            self.advance();
            self.expect(&TokenKind::Not)?;
            self.expect(&TokenKind::Exists)?;
        }
        self.expect(&TokenKind::For)?;
        self.expect(&TokenKind::LParen)?;
        if *self.peek() == TokenKind::RParen {
            return Err(CypherError::syntax(
                "relationship full-text indexes are not supported yet - full-text indexes are node-only"
                    .to_string(),
                self.span_pos(),
            ));
        }
        let _var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Colon)?;
        let mut labels = vec![self.parse_symbolic_name()?];
        while self.eat(&TokenKind::Pipe) {
            self.eat(&TokenKind::Colon); // tolerate `:A|:B`
            labels.push(self.parse_symbolic_name()?);
        }
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::On)?;
        // EACH (contextual keyword)
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("EACH")) {
            self.advance();
        }
        self.expect(&TokenKind::LBracket)?;
        let mut properties = Vec::new();
        loop {
            let _pvar = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Dot)?;
            properties.push(self.parse_symbolic_name()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RBracket)?;

        // optional OPTIONS { indexConfig: { 'fulltext.analyzer': '<name>' } }
        let mut analyzer: Option<String> = None;
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("OPTIONS")) {
            self.advance();
            self.expect(&TokenKind::LBrace)?;
            while *self.peek() != TokenKind::RBrace && *self.peek() != TokenKind::Eof {
                let key = self.parse_option_key()?;
                self.expect(&TokenKind::Colon)?;
                if key.eq_ignore_ascii_case("indexConfig") {
                    self.expect(&TokenKind::LBrace)?;
                    while *self.peek() != TokenKind::RBrace && *self.peek() != TokenKind::Eof {
                        let ck = self.parse_option_key()?;
                        self.expect(&TokenKind::Colon)?;
                        if ck.eq_ignore_ascii_case("fulltext.analyzer") {
                            if let TokenKind::StringLit(s) = self.peek().clone() {
                                self.advance();
                                analyzer = Some(s);
                            } else {
                                self.skip_option_value()?;
                            }
                        } else {
                            self.skip_option_value()?;
                        }
                        if *self.peek() == TokenKind::Comma {
                            self.advance();
                        }
                    }
                    self.expect(&TokenKind::RBrace)?;
                } else {
                    self.skip_option_value()?;
                }
                if *self.peek() == TokenKind::Comma {
                    self.advance();
                }
            }
            self.expect(&TokenKind::RBrace)?;
        }

        Ok(Query::CreateFulltextIndex {
            name,
            labels,
            properties,
            analyzer,
        })
    }

    /// `CREATE POINT INDEX name [IF NOT EXISTS] FOR (n:Label) ON (n.prop)
    /// [OPTIONS { ... }]` (dispatcher consumed `CREATE POINT INDEX`). Node-only, one
    /// label + one property, like Neo4j. OPTIONS are accepted and ignored (the R-tree
    /// has no build parameters).
    fn parse_create_point_index(&mut self) -> Result<Query, CypherError> {
        let name = self.parse_ddl_name()?;
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF")) {
            self.advance();
            self.expect(&TokenKind::Not)?;
            self.expect(&TokenKind::Exists)?;
        }
        self.expect(&TokenKind::For)?;
        self.expect(&TokenKind::LParen)?;
        // relationship form `()-[r:T]-()` -> edge index (the R-tree is node-only)
        if *self.peek() == TokenKind::RParen {
            self.advance(); // ')'
            self.eat(&TokenKind::Lt);
            self.expect(&TokenKind::Minus)?;
            self.expect(&TokenKind::LBracket)?;
            let _rvar = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            let edge_type = self.parse_symbolic_name()?;
            self.expect(&TokenKind::RBracket)?;
            self.expect(&TokenKind::Minus)?;
            self.eat(&TokenKind::Gt);
            self.expect(&TokenKind::LParen)?;
            self.expect(&TokenKind::RParen)?;
            let property = self.parse_index_on_property()?;
            return Ok(Query::CreateEdgeIndex {
                name,
                edge_type,
                property: Some(property),
                extra_properties: Vec::new(),
            });
        }
        let _var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Colon)?;
        let label = self.parse_symbolic_name()?;
        self.expect(&TokenKind::RParen)?;
        let property = self.parse_index_on_property()?;

        // optional trailing OPTIONS { ... }, ignored
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("OPTIONS")) {
            self.advance();
            self.skip_option_value()?;
        }

        Ok(Query::CreatePointIndex {
            name,
            label,
            property,
        })
    }

    /// `CREATE TEXT INDEX name [IF NOT EXISTS] FOR (n:Label) ON (n.prop)
    /// [OPTIONS { ... }]`: a trigram index (node form). The relationship form
    /// `FOR ()-[r:T]-()` goes to the edge b-tree index. OPTIONS are ignored.
    fn parse_create_text_index(&mut self) -> Result<Query, CypherError> {
        let name = self.parse_ddl_name()?;
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF")) {
            self.advance();
            self.expect(&TokenKind::Not)?;
            self.expect(&TokenKind::Exists)?;
        }
        self.expect(&TokenKind::For)?;
        self.expect(&TokenKind::LParen)?;
        // relationship form `()-[r:T]-()` -> edge index
        if *self.peek() == TokenKind::RParen {
            self.advance(); // ')'
            self.eat(&TokenKind::Lt);
            self.expect(&TokenKind::Minus)?;
            self.expect(&TokenKind::LBracket)?;
            let _rvar = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            let edge_type = self.parse_symbolic_name()?;
            self.expect(&TokenKind::RBracket)?;
            self.expect(&TokenKind::Minus)?;
            self.eat(&TokenKind::Gt);
            self.expect(&TokenKind::LParen)?;
            self.expect(&TokenKind::RParen)?;
            let property = self.parse_index_on_property()?;
            return Ok(Query::CreateEdgeIndex {
                name,
                edge_type,
                property: Some(property),
                extra_properties: Vec::new(),
            });
        }
        let _var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Colon)?;
        let label = self.parse_symbolic_name()?;
        self.expect(&TokenKind::RParen)?;
        let property = self.parse_index_on_property()?;
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("OPTIONS")) {
            self.advance();
            self.skip_option_value()?;
        }
        Ok(Query::CreateTextIndex {
            name,
            label,
            property,
        })
    }

    /// `CREATE VECTOR INDEX name [IF NOT EXISTS] FOR (n:Label) ON (n.prop)
    ///   OPTIONS { indexConfig: { 'vector.dimensions': N,
    ///                            'vector.similarity_function': 'cosine' } }`
    ///
    /// Neo4j's vector-index DDL. `vector.dimensions` is optional (inferred from the
    /// data at build time, as in Neo4j 5.23+); `vector.similarity_function`
    /// defaults to `cosine`.
    pub(super) fn parse_create_vector_index(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Create)?;
        self.advance(); // VECTOR (Ident)
        self.expect(&TokenKind::Index)?;

        let name = self.parse_ddl_name()?;
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF")) {
            self.advance(); // IF
            self.expect(&TokenKind::Not)?;
            self.expect(&TokenKind::Exists)?;
        }
        self.expect(&TokenKind::For)?;
        self.expect(&TokenKind::LParen)?;
        if *self.peek() == TokenKind::RParen {
            return Err(CypherError::syntax(
                "relationship vector indexes are not supported yet - vector indexes are node-only"
                    .to_string(),
                self.span_pos(),
            ));
        }
        let _node_var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Colon)?;
        let label = self.parse_symbolic_name()?;
        if *self.peek() == TokenKind::Pipe {
            return Err(CypherError::syntax(
                "multi-label vector indexes are not supported yet - create one index per label"
                    .to_string(),
                self.span_pos(),
            ));
        }
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::On)?;
        // Neo4j's canonical form has no parens (`ON m.embedding`); parenthesised is accepted too
        let parens = self.eat(&TokenKind::LParen);
        let _prop_var = self.parse_symbolic_name()?;
        self.expect(&TokenKind::Dot)?;
        let property = self.parse_symbolic_name()?;
        if parens {
            self.expect(&TokenKind::RParen)?;
        }

        let mut dimensions: Option<usize> = None;
        let mut similarity = "cosine".to_string();
        let mut quantization: Option<bool> = None;

        // optional OPTIONS { indexConfig: { '<key>': <value>, ... } }
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("OPTIONS")) {
            self.advance(); // OPTIONS
            self.expect(&TokenKind::LBrace)?;
            // one `indexConfig:` entry expected, others ignored
            while *self.peek() != TokenKind::RBrace && *self.peek() != TokenKind::Eof {
                let key = self.parse_option_key()?;
                self.expect(&TokenKind::Colon)?;
                if key.eq_ignore_ascii_case("indexConfig") {
                    self.parse_vector_index_config(
                        &mut dimensions,
                        &mut similarity,
                        &mut quantization,
                    )?;
                } else {
                    // skip an unknown option value (map or scalar)
                    self.skip_option_value()?;
                }
                if *self.peek() == TokenKind::Comma {
                    self.advance();
                }
            }
            self.expect(&TokenKind::RBrace)?;
        }

        Ok(Query::CreateVectorIndex {
            name,
            label,
            property,
            dimensions,
            similarity,
            quantization,
        })
    }

    /// OPTIONS map key: bare or dotted identifier, or quoted string (Neo4j uses
    /// `'vector.dimensions'`).
    fn parse_option_key(&mut self) -> Result<String, CypherError> {
        match self.peek().clone() {
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(s)
            }
            TokenKind::Ident(mut s) => {
                self.advance();
                // unquoted dotted keys (`vector.dimensions`)
                while *self.peek() == TokenKind::Dot {
                    self.advance();
                    s.push('.');
                    s.push_str(&self.parse_symbolic_name()?);
                }
                Ok(s)
            }
            other => Err(CypherError::syntax(
                format!("expected option key, found {other:?}"),
                self.span_pos(),
            )),
        }
    }

    /// Inner `{ 'vector.dimensions': N, 'vector.similarity_function': 'cosine' }` config map.
    fn parse_vector_index_config(
        &mut self,
        dimensions: &mut Option<usize>,
        similarity: &mut String,
        quantization: &mut Option<bool>,
    ) -> Result<(), CypherError> {
        self.expect(&TokenKind::LBrace)?;
        while *self.peek() != TokenKind::RBrace && *self.peek() != TokenKind::Eof {
            let key = self.parse_option_key()?;
            self.expect(&TokenKind::Colon)?;
            match key.to_ascii_lowercase().as_str() {
                "vector.dimensions" => match self.peek().clone() {
                    TokenKind::Integer(n) if n > 0 => {
                        self.advance();
                        *dimensions = Some(n as usize);
                    }
                    other => {
                        return Err(CypherError::syntax(
                            format!(
                                "vector.dimensions must be a positive integer, found {other:?}"
                            ),
                            self.span_pos(),
                        ));
                    }
                },
                "vector.similarity_function" => match self.peek().clone() {
                    TokenKind::StringLit(s) => {
                        self.advance();
                        *similarity = s;
                    }
                    other => {
                        return Err(CypherError::syntax(
                            format!("vector.similarity_function must be a string, found {other:?}"),
                            self.span_pos(),
                        ));
                    }
                },
                "vector.quantization.enabled" => match self.peek().clone() {
                    TokenKind::True => {
                        self.advance();
                        *quantization = Some(true);
                    }
                    TokenKind::False => {
                        self.advance();
                        *quantization = Some(false);
                    }
                    // Neo4j also accepts 'true'/'false' strings
                    TokenKind::StringLit(s) => {
                        self.advance();
                        *quantization = Some(s.eq_ignore_ascii_case("true"));
                    }
                    other => {
                        return Err(CypherError::syntax(
                            format!(
                                "vector.quantization.enabled must be a boolean, found {other:?}"
                            ),
                            self.span_pos(),
                        ));
                    }
                },
                _ => {
                    self.skip_option_value()?;
                }
            }
            if *self.peek() == TokenKind::Comma {
                self.advance();
            }
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(())
    }

    /// Skips one OPTIONS value (a scalar token or a balanced `{...}` map) we don't use.
    fn skip_option_value(&mut self) -> Result<(), CypherError> {
        if *self.peek() == TokenKind::LBrace {
            let mut depth = 0;
            loop {
                match self.peek() {
                    TokenKind::LBrace => depth += 1,
                    TokenKind::RBrace => {
                        depth -= 1;
                        if depth == 0 {
                            self.advance();
                            break;
                        }
                    }
                    TokenKind::Eof => break,
                    _ => {}
                }
                self.advance();
            }
        } else {
            self.advance();
        }
        Ok(())
    }

    /// CREATE EDGE INDEX ON :EdgeType(property): property index
    /// CREATE EDGE INDEX ON :EdgeType: type-only index
    pub(super) fn parse_create_edge_index(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Create)?;
        // EDGE (as Ident)
        self.advance();
        self.expect(&TokenKind::Index)?;
        self.expect(&TokenKind::On)?;
        self.expect(&TokenKind::Colon)?;
        let edge_type = self.parse_symbolic_name()?;

        if *self.peek() == TokenKind::LParen {
            // property index
            self.advance(); // (
            let property = self.parse_symbolic_name()?;
            self.expect(&TokenKind::RParen)?;
            let name = format!("eidx_{}_{}", edge_type, property);
            Ok(Query::CreateEdgeIndex {
                name,
                edge_type,
                property: Some(property),
                extra_properties: Vec::new(),
            })
        } else {
            // type-only index
            let name = format!("eidx_{}", edge_type);
            Ok(Query::CreateEdgeIndex {
                name,
                edge_type,
                property: None,
                extra_properties: Vec::new(),
            })
        }
    }

    /// DROP EDGE INDEX name
    pub(super) fn parse_drop_edge_index(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Drop)?;
        // EDGE (as Ident)
        self.advance();
        self.expect(&TokenKind::Index)?;
        let name = self.parse_symbolic_name()?;
        Ok(Query::DropEdgeIndex { name })
    }

    /// ANALYZE GRAPH [LABEL label1, label2, ...]
    pub(super) fn parse_analyze_graph(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Analyze)?;
        self.expect(&TokenKind::Graph)?;
        let mut labels = Vec::new();
        // optional LABEL keyword (an identifier)
        if matches!(self.peek(), TokenKind::Ident(s) if s.to_uppercase() == "LABEL") {
            self.advance();
            // comma-separated labels
            labels.push(self.parse_symbolic_name()?);
            while self.eat(&TokenKind::Comma) {
                labels.push(self.parse_symbolic_name()?);
            }
        }
        Ok(Query::AnalyzeGraph { labels })
    }

    /// CREATE CONSTRAINT name [IF NOT EXISTS] FOR (var:Label) REQUIRE var.prop IS UNIQUE
    /// CREATE CONSTRAINT name [IF NOT EXISTS] FOR (var:Label) REQUIRE var.prop IS NOT NULL
    pub(super) fn parse_create_constraint(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Create)?;
        self.expect(&TokenKind::Constraint)?;
        let name = self.parse_ddl_name()?;

        // optional IF NOT EXISTS
        let mut if_not_exists = false;
        if *self.peek() == TokenKind::Ident("IF".to_string())
            || matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF"))
        {
            self.advance(); // IF
            self.expect(&TokenKind::Not)?;
            self.expect(&TokenKind::Exists)?;
            if_not_exists = true;
        }

        self.expect(&TokenKind::For)?;
        self.expect(&TokenKind::LParen)?;
        // node `(n:Label)` vs relationship `()-[r:TYPE]-()`: an empty `()` starts a relationship
        let is_relationship;
        let label;
        if *self.peek() == TokenKind::RParen {
            is_relationship = true;
            self.advance(); // ')': first empty node
            // left connector `-[` or `<-[` (direction-agnostic)
            self.eat(&TokenKind::Lt);
            self.expect(&TokenKind::Minus)?;
            self.expect(&TokenKind::LBracket)?;
            let _rvar = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            label = self.parse_symbolic_name()?; // relationship type
            self.expect(&TokenKind::RBracket)?;
            // right connector `]-` or `]->`, then the trailing empty node
            self.expect(&TokenKind::Minus)?;
            self.eat(&TokenKind::Gt);
            self.expect(&TokenKind::LParen)?;
            self.expect(&TokenKind::RParen)?;
        } else {
            is_relationship = false;
            let _var = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Colon)?;
            label = self.parse_symbolic_name()?;
            self.expect(&TokenKind::RParen)?;
        }
        self.expect(&TokenKind::Require)?;
        // property reference, two forms:
        //   * `var.prop`         single property
        //   * `(var.p1, var.p2)` composite (`IS NODE KEY` / `IS UNIQUE`)
        // all are captured; the constraint kind decides whether composite is legal
        let mut properties: Vec<String> = Vec::new();
        let paren = self.eat(&TokenKind::LParen);
        loop {
            let _var_ref = self.parse_symbolic_name()?;
            self.expect(&TokenKind::Dot)?;
            properties.push(self.parse_symbolic_name()?);
            if paren {
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            } else {
                break;
            }
        }
        if paren {
            self.expect(&TokenKind::RParen)?;
        }
        let property = properties[0].clone();
        self.expect(&TokenKind::Is)?;

        // IS UNIQUE | IS NOT NULL | IS :: <TYPE> | IS NODE KEY
        let constraint_type = if *self.peek() == TokenKind::Unique {
            self.advance();
            "UNIQUE".to_string()
        } else if matches!(self.peek(), TokenKind::Ident(s)
            if s.eq_ignore_ascii_case("NODE")
                || s.eq_ignore_ascii_case("RELATIONSHIP")
                || s.eq_ignore_ascii_case("KEY"))
        {
            // `IS [NODE|RELATIONSHIP] KEY`, bare `IS KEY` too; node vs relationship comes from the
            // FOR pattern
            let first = match self.peek() {
                TokenKind::Ident(s) => s.to_uppercase(),
                _ => unreachable!(),
            };
            self.advance();
            if first != "KEY" {
                match self.peek() {
                    TokenKind::Ident(s) if s.eq_ignore_ascii_case("KEY") => {
                        self.advance();
                    }
                    _ => {
                        return Err(CypherError::syntax(
                            "expected KEY after NODE/RELATIONSHIP in constraint",
                            self.span_pos(),
                        ));
                    }
                }
            }
            "NODE KEY".to_string()
        } else if *self.peek() == TokenKind::Not {
            self.advance(); // NOT
            self.expect(&TokenKind::Null)?;
            "NOT NULL".to_string()
        } else if *self.peek() == TokenKind::DoubleColon {
            // `IS :: STRING` / `IS :: LOCAL DATETIME` ...: take ident tokens until one can't be part
            // of a type name (end of input, `OPTIONS`)
            self.advance(); // consume ::
            let mut parts = Vec::new();
            while let TokenKind::Ident(s) = self.peek() {
                parts.push(s.clone());
                self.advance();
                if parts.len() >= 4 {
                    // longest valid type is "ZONED DATETIME"
                    break;
                }
            }
            if parts.is_empty() {
                return Err(CypherError::syntax(
                    "expected a type name after `IS ::`",
                    self.span_pos(),
                ));
            }
            // type parameters (`VECTOR<FLOAT32>(384)`, `LIST<STRING>`) are accepted and dropped:
            // the base kind is enforced, and vectors are LIST<FLOAT>
            if self.eat(&TokenKind::Lt) {
                while !matches!(self.peek(), TokenKind::Gt | TokenKind::Eof) {
                    self.advance();
                }
                self.expect(&TokenKind::Gt)?;
            }
            if *self.peek() == TokenKind::LParen {
                self.advance();
                while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
                    self.advance();
                }
                self.expect(&TokenKind::RParen)?;
            }
            // `| NULL` suffix (nullable type): type checks only apply when the property is present
            // anyway, so accept and drop it
            while *self.peek() == TokenKind::Pipe {
                self.advance();
                match self.peek().clone() {
                    TokenKind::Null => {
                        self.advance();
                    }
                    TokenKind::Ident(s) if s.eq_ignore_ascii_case("NULL") => {
                        self.advance();
                    }
                    _ => {
                        return Err(CypherError::syntax(
                            "only `| NULL` is supported in IS :: type unions".to_string(),
                            self.span_pos(),
                        ));
                    }
                }
            }
            let joined = parts.join(" ");
            // validate against the closed set; canonical surface form
            let kind = traverse_core::graph::constraint::PropertyTypeKind::from_surface(&joined)
                .ok_or_else(|| {
                    CypherError::syntax(
                        format!("unknown property type `{joined}` in IS :: constraint"),
                        0,
                    )
                })?;
            format!("TYPE {}", kind.surface())
        } else {
            return Err(CypherError::syntax(
                "expected UNIQUE, NOT NULL, or :: <TYPE> after IS",
                self.span_pos(),
            ));
        };

        // composite form only for UNIQUE and KEY (Neo4j); existence and type constraints are
        // single-property
        if properties.len() > 1 && !matches!(constraint_type.as_str(), "UNIQUE" | "NODE KEY") {
            return Err(CypherError::syntax(
                "composite constraints support only IS UNIQUE and IS [NODE|RELATIONSHIP] KEY",
                self.span_pos(),
            ));
        }
        Ok(Query::CreateConstraint {
            name,
            label,
            property,
            extra_properties: properties[1..].to_vec(),
            constraint_type,
            is_relationship,
            if_not_exists,
        })
    }

    /// DROP CONSTRAINT name [IF EXISTS]
    pub(super) fn parse_drop_constraint(&mut self) -> Result<Query, CypherError> {
        self.expect(&TokenKind::Drop)?;
        self.expect(&TokenKind::Constraint)?;
        let name = self.parse_ddl_name()?;
        // optional IF EXISTS
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case("IF")) {
            self.advance(); // IF
            self.expect(&TokenKind::Exists)?;
        }
        Ok(Query::DropConstraint { name })
    }
}
