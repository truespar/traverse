//! Expression bytecode compiler and evaluator.
//!
//! Simple filter predicates (comparisons, boolean logic, property access)
//! compile to flat bytecode, skipping [`eval_expr`]'s recursion. Complex
//! expressions (function calls, subqueries, comprehensions, ...) aren't compiled;
//! the filter falls back to `eval_expr`.

use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, CmpOp, Expr, Literal, StringOp, UnaryOp};
use crate::error::CypherError;
use crate::executor;
use crate::frame::{Frame, SymbolTable};
use crate::types::CypherValue;

/// One bytecode instruction.
#[derive(Clone, Copy)]
enum ByteOp {
    /// Push `frame[slot]` onto the stack.
    PushSlot(u16),
    /// Push `constants[idx]` onto the stack.
    PushConst(u16),
    /// Push query parameter `param_keys[idx]` onto the stack.
    PushParam(u16),
    /// Pop a value, push `get_node_property(value, prop_keys[idx])`. Specialized for NodeRef, the
    /// usual case in filters.
    GetNodeProp(u16),
    // -- comparisons (pop 2, push bool/null) --
    CmpEq,
    CmpNeq,
    CmpLt,
    CmpLte,
    CmpGt,
    CmpGte,
    // -- boolean ops --
    And,
    Or,
    Not,
    // -- null checks --
    IsNull,
    IsNotNull,
    // -- arithmetic (pop 2, push result) --
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// Unary minus.
    Negate,
    // -- string ops (pop 2, push bool) --
    StartsWith,
    EndsWith,
    Contains,
    // -- membership (pop value + list, push bool) --
    In,
    // -- control flow --
    /// Jump to `pc` if top of stack is false (AND short-circuit).
    JumpIfFalse(u16),
    /// Jump to `pc` if top of stack is true (OR short-circuit).
    JumpIfTrue(u16),
    /// Fused: read the string property `prop_keys[prop_idx]` of an edge/node and test NOT IN
    /// `string_lists[list_idx]`: true if absent from the list. Zero-allocation via get_str.
    PropNotInStrList {
        slot: u16,
        prop_idx: u16,
        list_idx: u16,
    },
}

/// Compiled filter expression.
pub(crate) struct CompiledExpr {
    ops: Vec<ByteOp>,
    constants: Vec<CypherValue>,
    prop_keys: Vec<String>,
    param_keys: Vec<String>,
    /// String lists for PropNotInStrList.
    string_lists: Vec<Vec<String>>,
}

// -- Compilation ---------------------------------------------------------

/// Compiles an expression to bytecode; `None` for unsupported forms (fall back to `eval_expr`).
pub(crate) fn try_compile(expr: &Expr, symtab: &SymbolTable) -> Option<CompiledExpr> {
    let mut compiler = Compiler {
        ops: Vec::new(),
        constants: Vec::new(),
        prop_keys: Vec::new(),
        param_keys: Vec::new(),
        string_lists: Vec::new(),
    };
    compiler.compile_expr(expr, symtab)?;
    // sanity: a well-formed expression leaves exactly 1 value
    Some(CompiledExpr {
        ops: compiler.ops,
        constants: compiler.constants,
        prop_keys: compiler.prop_keys,
        param_keys: compiler.param_keys,
        string_lists: compiler.string_lists,
    })
}

struct Compiler {
    ops: Vec<ByteOp>,
    constants: Vec<CypherValue>,
    prop_keys: Vec<String>,
    param_keys: Vec<String>,
    string_lists: Vec<Vec<String>>,
}

impl Compiler {
    /// Compiles an expression; None if any part is unsupported.
    #[recursive::recursive]
    fn compile_expr(&mut self, expr: &Expr, symtab: &SymbolTable) -> Option<()> {
        match expr {
            Expr::Literal(lit) => {
                let val = match lit {
                    Literal::Null => CypherValue::Null,
                    Literal::Bool(b) => CypherValue::Bool(*b),
                    Literal::Integer(i) => CypherValue::Integer(*i),
                    Literal::Float(f) => CypherValue::Float(*f),
                    Literal::String(s) => CypherValue::String(s.clone()),
                };
                let idx = self.add_const(val);
                self.ops.push(ByteOp::PushConst(idx));
                Some(())
            }

            Expr::Variable(name) => {
                let slot = symtab.slot(name)?;
                if slot > u16::MAX as usize {
                    return None;
                }
                self.ops.push(ByteOp::PushSlot(slot as u16));
                Some(())
            }

            Expr::Parameter(name) => {
                let idx = self.add_param(name);
                self.ops.push(ByteOp::PushParam(idx));
                Some(())
            }

            Expr::Property(base, key) => {
                // only Property(Variable, key), the common case; Property(Property(...)) or
                // Property(FunctionCall(...)) fall back to eval_expr
                match base.as_ref() {
                    Expr::Variable(var_name) => {
                        let slot = symtab.slot(var_name)?;
                        if slot > u16::MAX as usize {
                            return None;
                        }
                        let prop_idx = self.add_prop_key(key);
                        self.ops.push(ByteOp::PushSlot(slot as u16));
                        // GetNodeProp fast path (most filters read NodeRef properties)
                        self.ops.push(ByteOp::GetNodeProp(prop_idx));
                        Some(())
                    }
                    _ => None, // nested property chains unsupported
                }
            }

            Expr::Comparison(left, cmps) => {
                if cmps.len() != 1 {
                    // chained comparisons (a < b < c) are rare in filters: fall back for
                    // correctness
                    return None;
                }
                let (op, right) = &cmps[0];
                self.compile_expr(left, symtab)?;
                self.compile_expr(right, symtab)?;
                let cmp_op = match op {
                    CmpOp::Eq => ByteOp::CmpEq,
                    CmpOp::Neq => ByteOp::CmpNeq,
                    CmpOp::Lt => ByteOp::CmpLt,
                    CmpOp::Lte => ByteOp::CmpLte,
                    CmpOp::Gt => ByteOp::CmpGt,
                    CmpOp::Gte => ByteOp::CmpGte,
                };
                self.ops.push(cmp_op);
                Some(())
            }

            Expr::BinaryOp(left, op, right) => {
                match op {
                    BinOp::And => {
                        self.compile_expr(left, symtab)?;
                        // short-circuit: if false, jump past the right operand
                        let jump_idx = self.ops.len();
                        self.ops.push(ByteOp::JumpIfFalse(0)); // placeholder
                        self.compile_expr(right, symtab)?;
                        self.ops.push(ByteOp::And);
                        // patch the jump target
                        let target = self.ops.len() as u16;
                        self.ops[jump_idx] = ByteOp::JumpIfFalse(target);
                        Some(())
                    }
                    BinOp::Or => {
                        self.compile_expr(left, symtab)?;
                        let jump_idx = self.ops.len();
                        self.ops.push(ByteOp::JumpIfTrue(0)); // placeholder
                        self.compile_expr(right, symtab)?;
                        self.ops.push(ByteOp::Or);
                        let target = self.ops.len() as u16;
                        self.ops[jump_idx] = ByteOp::JumpIfTrue(target);
                        Some(())
                    }
                    BinOp::Add => self.compile_binary(left, right, ByteOp::Add, symtab),
                    BinOp::Sub => self.compile_binary(left, right, ByteOp::Sub, symtab),
                    BinOp::Mul => self.compile_binary(left, right, ByteOp::Mul, symtab),
                    BinOp::Div => self.compile_binary(left, right, ByteOp::Div, symtab),
                    BinOp::Mod => self.compile_binary(left, right, ByteOp::Mod, symtab),
                    // Pow, Xor: rare in filters, bail
                    _ => None,
                }
            }

            Expr::UnaryOp(op, inner) => {
                // try the fused PropNotInStrList for `NOT prop IN [str_list]`
                if matches!(op, UnaryOp::Not)
                    && let Some(()) = self.try_compile_not_in_str_list(inner, symtab)
                {
                    return Some(());
                }
                self.compile_expr(inner, symtab)?;
                match op {
                    UnaryOp::Not => self.ops.push(ByteOp::Not),
                    UnaryOp::Neg => self.ops.push(ByteOp::Negate),
                }
                Some(())
            }

            Expr::IsNull(inner) => {
                self.compile_expr(inner, symtab)?;
                self.ops.push(ByteOp::IsNull);
                Some(())
            }

            Expr::IsNotNull(inner) => {
                self.compile_expr(inner, symtab)?;
                self.ops.push(ByteOp::IsNotNull);
                Some(())
            }

            Expr::StringOp(left, op, right) => {
                // non-regex string ops only (regex allocates)
                match op {
                    StringOp::Regex => None,
                    StringOp::StartsWith => {
                        self.compile_binary(left, right, ByteOp::StartsWith, symtab)
                    }
                    StringOp::EndsWith => {
                        self.compile_binary(left, right, ByteOp::EndsWith, symtab)
                    }
                    StringOp::Contains => {
                        self.compile_binary(left, right, ByteOp::Contains, symtab)
                    }
                }
            }

            Expr::In(left, right) => {
                self.compile_expr(left, symtab)?;
                self.compile_expr(right, symtab)?;
                self.ops.push(ByteOp::In);
                Some(())
            }

            Expr::ListLiteral(items) => {
                // constant lists (common in IN); every element must be a literal
                let mut vals = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Expr::Literal(Literal::String(s)) => {
                            vals.push(CypherValue::String(s.clone()))
                        }
                        Expr::Literal(Literal::Integer(i)) => vals.push(CypherValue::Integer(*i)),
                        Expr::Literal(Literal::Float(f)) => vals.push(CypherValue::Float(*f)),
                        Expr::Literal(Literal::Bool(b)) => vals.push(CypherValue::Bool(*b)),
                        Expr::Literal(Literal::Null) => vals.push(CypherValue::Null),
                        _ => return None, // non-literal elements unsupported
                    }
                }
                let idx = self.add_const(CypherValue::List(vals));
                self.ops.push(ByteOp::PushConst(idx));
                Some(())
            }

            // unsupported: FunctionCall, Case, ListComprehension, subqueries, PatternPredicate,
            // ListIndex, ListSlice, MapLiteral, QuantifierExpr, PatternComprehension
            _ => None,
        }
    }

    fn compile_binary(
        &mut self,
        left: &Expr,
        right: &Expr,
        op: ByteOp,
        symtab: &SymbolTable,
    ) -> Option<()> {
        self.compile_expr(left, symtab)?;
        self.compile_expr(right, symtab)?;
        self.ops.push(op);
        Some(())
    }

    fn add_const(&mut self, val: CypherValue) -> u16 {
        let idx = self.constants.len();
        self.constants.push(val);
        idx as u16
    }

    fn add_prop_key(&mut self, key: &str) -> u16 {
        // dedup property keys
        for (i, k) in self.prop_keys.iter().enumerate() {
            if k == key {
                return i as u16;
            }
        }
        let idx = self.prop_keys.len();
        self.prop_keys.push(key.to_string());
        idx as u16
    }

    fn add_param(&mut self, name: &str) -> u16 {
        for (i, k) in self.param_keys.iter().enumerate() {
            if k == name {
                return i as u16;
            }
        }
        let idx = self.param_keys.len();
        self.param_keys.push(name.to_string());
        idx as u16
    }

    /// Compiles `NOT (Property(Variable(v), key) IN ListLiteral([String...]))` into one fused
    /// instruction.
    fn try_compile_not_in_str_list(&mut self, inner: &Expr, symtab: &SymbolTable) -> Option<()> {
        let Expr::In(left, right) = inner else {
            return None;
        };
        let Expr::Property(base, key) = left.as_ref() else {
            return None;
        };
        let Expr::Variable(var) = base.as_ref() else {
            return None;
        };
        let Expr::ListLiteral(items) = right.as_ref() else {
            return None;
        };

        // every item must be a string literal
        let mut strings = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Expr::Literal(Literal::String(s)) => strings.push(s.clone()),
                _ => return None,
            }
        }

        let slot = symtab.slot(var)?;
        if slot > u16::MAX as usize {
            return None;
        }
        let prop_idx = self.add_prop_key(key);
        let list_idx = self.string_lists.len() as u16;
        self.string_lists.push(strings);

        self.ops.push(ByteOp::PropNotInStrList {
            slot: slot as u16,
            prop_idx,
            list_idx,
        });
        Some(())
    }
}

/// `NOT var.prop IN [str_list]` from an AST expression: `(variable, property, values)` for
/// `UnaryOp(Not, In(Property(Variable(v), key), ListLiteral([String...])))`; string literals or
/// parameters (from the thread-local query params).
pub(crate) fn extract_not_in_str_list(expr: &Expr) -> Option<(&str, &str, Vec<String>)> {
    let Expr::UnaryOp(UnaryOp::Not, inner) = expr else {
        return None;
    };
    let Expr::In(left, right) = inner.as_ref() else {
        return None;
    };
    let Expr::Property(base, key) = left.as_ref() else {
        return None;
    };
    let Expr::Variable(var) = base.as_ref() else {
        return None;
    };
    let Expr::ListLiteral(items) = right.as_ref() else {
        return None;
    };

    let mut strings = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Expr::Literal(Literal::String(s)) => strings.push(s.clone()),
            Expr::Parameter(name) => {
                // parameter from the thread-local query params
                match crate::executor::get_query_param(name) {
                    Some(crate::types::CypherValue::String(s)) => strings.push(s),
                    _ => return None,
                }
            }
            _ => return None,
        }
    }

    Some((var.as_str(), key.as_str(), strings))
}

/// Positive `var.prop IN [str_list]`: `(variable, property, values)` for `In(Property(Variable(v),
/// key), ListLiteral([String...]))`; string literals or parameters (thread-local query params).
pub(crate) fn extract_in_str_list(expr: &Expr) -> Option<(&str, &str, Vec<String>)> {
    let Expr::In(left, right) = expr else {
        return None;
    };
    let Expr::Property(base, key) = left.as_ref() else {
        return None;
    };
    let Expr::Variable(var) = base.as_ref() else {
        return None;
    };
    let Expr::ListLiteral(items) = right.as_ref() else {
        return None;
    };

    let mut strings = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Expr::Literal(Literal::String(s)) => strings.push(s.clone()),
            Expr::Parameter(name) => match crate::executor::get_query_param(name) {
                Some(crate::types::CypherValue::String(s)) => strings.push(s),
                _ => return None,
            },
            _ => return None,
        }
    }

    Some((var.as_str(), key.as_str(), strings))
}

/// OR-chains of `var.prop = 'str'` read as `var.prop IN [str_list]`: `r.positionType = 'MB' OR
/// r.positionType = 'STY' OR r.positionType = 'VD'` -> `("r", "positionType", ["MB", "STY", "VD"])`.
pub(crate) fn extract_or_eq_as_in_str_list(expr: &Expr) -> Option<(&str, &str, Vec<String>)> {
    let disjuncts = flatten_or_disjuncts(expr);
    if disjuncts.len() < 2 {
        return None;
    }

    let mut var_name: Option<&str> = None;
    let mut prop_key: Option<&str> = None;
    let mut values = Vec::with_capacity(disjuncts.len());

    for disj in &disjuncts {
        let Expr::Comparison(lhs, cmps) = disj else {
            return None;
        };
        if cmps.len() != 1 || cmps[0].0 != CmpOp::Eq {
            return None;
        }
        let Expr::Property(base, key) = lhs.as_ref() else {
            return None;
        };
        let Expr::Variable(var) = base.as_ref() else {
            return None;
        };

        let val = match &cmps[0].1 {
            Expr::Literal(Literal::String(s)) => s.clone(),
            Expr::Parameter(name) => match crate::executor::get_query_param(name) {
                Some(crate::types::CypherValue::String(s)) => s,
                _ => return None,
            },
            _ => return None,
        };

        match (var_name, prop_key) {
            (None, None) => {
                var_name = Some(var.as_str());
                prop_key = Some(key.as_str());
            }
            (Some(v), Some(k)) => {
                if v != var.as_str() || k != key.as_str() {
                    return None;
                }
            }
            _ => unreachable!(),
        }

        values.push(val);
    }

    Some((var_name.unwrap(), prop_key.unwrap(), values))
}

#[recursive::recursive]
fn flatten_or_disjuncts<'a>(expr: &'a Expr) -> Vec<&'a Expr> {
    match expr {
        Expr::BinaryOp(left, BinOp::Or, right) => {
            let mut out = flatten_or_disjuncts(left);
            out.extend(flatten_or_disjuncts(right));
            out
        }
        _ => vec![expr],
    }
}

// -- Evaluation ----------------------------------------------------------

/// Shared evaluation loop; leaves the result in `stack[0]` for callers to read as bool,
/// Option<bool>, CypherValue, ...
#[inline(always)]
fn eval_bytecode_impl(
    compiled: &CompiledExpr,
    frame: &Frame,
    graph: &dyn GraphView,
    stack: &mut [CypherValue; 8],
) -> Result<(), CypherError> {
    let mut sp: usize = 0;
    let mut pc: usize = 0;
    let ops = &compiled.ops;

    while pc < ops.len() {
        match ops[pc] {
            ByteOp::PushSlot(slot) => {
                stack[sp] = frame[slot as usize].to_cypher_value();
                sp += 1;
            }

            ByteOp::PushConst(idx) => {
                stack[sp] = compiled.constants[idx as usize].clone();
                sp += 1;
            }

            ByteOp::PushParam(idx) => {
                let key = &compiled.param_keys[idx as usize];
                stack[sp] = executor::get_query_param(key).ok_or_else(|| {
                    // Neo4j ParameterMissing, as in eval_expr
                    CypherError::semantic(format!("Expected parameter(s): {key}"))
                        .with_status(crate::status::GqlStatus::InvalidParameter)
                })?;
                sp += 1;
            }

            ByteOp::GetNodeProp(idx) => {
                let key = &compiled.prop_keys[idx as usize];
                sp -= 1;
                let val = &stack[sp];
                let result = match val {
                    CypherValue::NodeRef(id) => match graph.get_node_property(*id, key) {
                        Ok(Some(pv)) => CypherValue::from_property(&pv),
                        Ok(None) => CypherValue::Null,
                        Err(_) => {
                            return Err(CypherError::semantic("deleted entity access".to_string()));
                        }
                    },
                    CypherValue::EdgeRef { id, .. } => match graph.get_edge_property(*id, key) {
                        Ok(Some(pv)) => CypherValue::from_property(&pv),
                        Ok(None) => CypherValue::Null,
                        Err(_) => {
                            return Err(CypherError::semantic("deleted entity access".to_string()));
                        }
                    },
                    _ => executor::get_property(val, key)?,
                };
                stack[sp] = result;
                sp += 1;
            }

            ByteOp::CmpEq => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = executor::eval_comparison(left, CmpOp::Eq, right);
                sp += 1;
            }

            ByteOp::CmpNeq => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = executor::eval_comparison(left, CmpOp::Neq, right);
                sp += 1;
            }

            ByteOp::CmpLt => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = executor::eval_comparison(left, CmpOp::Lt, right);
                sp += 1;
            }

            ByteOp::CmpLte => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = executor::eval_comparison(left, CmpOp::Lte, right);
                sp += 1;
            }

            ByteOp::CmpGt => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = executor::eval_comparison(left, CmpOp::Gt, right);
                sp += 1;
            }

            ByteOp::CmpGte => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = executor::eval_comparison(left, CmpOp::Gte, right);
                sp += 1;
            }

            ByteOp::And => {
                // both operands on the stack; only reached when the left side wasn't false
                // (JumpIfFalse skips us otherwise)
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                // three-valued AND: left is true or null
                let result = if matches!(right, CypherValue::Bool(false)) {
                    CypherValue::Bool(false)
                } else if left.is_null() || right.is_null() {
                    CypherValue::Null
                } else {
                    CypherValue::Bool(executor::is_truthy(left) && executor::is_truthy(right))
                };
                stack[sp] = result;
                sp += 1;
            }

            ByteOp::Or => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                // three-valued OR: left is false or null
                let result = if executor::is_truthy(right) {
                    CypherValue::Bool(true)
                } else if left.is_null() || right.is_null() {
                    CypherValue::Null
                } else {
                    CypherValue::Bool(false)
                };
                stack[sp] = result;
                sp += 1;
            }

            ByteOp::Not => {
                sp -= 1;
                let val = &stack[sp];
                stack[sp] = if val.is_null() {
                    CypherValue::Null
                } else {
                    CypherValue::Bool(!executor::is_truthy(val))
                };
                sp += 1;
            }

            ByteOp::IsNull => {
                sp -= 1;
                let val = &stack[sp];
                stack[sp] = CypherValue::Bool(val.is_null());
                sp += 1;
            }

            ByteOp::IsNotNull => {
                sp -= 1;
                let val = &stack[sp];
                stack[sp] = CypherValue::Bool(!val.is_null());
                sp += 1;
            }

            ByteOp::Add => {
                sp -= 1;
                let right = stack[sp].clone();
                sp -= 1;
                let left = stack[sp].clone();
                stack[sp] = eval_arith_add(&left, &right)?;
                sp += 1;
            }

            ByteOp::Sub => {
                sp -= 1;
                let right = stack[sp].clone();
                sp -= 1;
                let left = stack[sp].clone();
                stack[sp] = eval_arith_sub(&left, &right)?;
                sp += 1;
            }

            ByteOp::Mul => {
                sp -= 1;
                let right = stack[sp].clone();
                sp -= 1;
                let left = stack[sp].clone();
                stack[sp] = eval_arith_mul(&left, &right)?;
                sp += 1;
            }

            ByteOp::Div => {
                sp -= 1;
                let right = stack[sp].clone();
                sp -= 1;
                let left = stack[sp].clone();
                stack[sp] = eval_arith_div(&left, &right)?;
                sp += 1;
            }

            ByteOp::Mod => {
                sp -= 1;
                let right = stack[sp].clone();
                sp -= 1;
                let left = stack[sp].clone();
                stack[sp] = eval_arith_mod(&left, &right)?;
                sp += 1;
            }

            ByteOp::Negate => {
                sp -= 1;
                let val = &stack[sp];
                stack[sp] = match val {
                    CypherValue::Integer(i) => match i.checked_neg() {
                        Some(n) => CypherValue::Integer(n),
                        None => {
                            return Err(CypherError::type_error("long overflow".to_string())
                                .with_status(crate::status::GqlStatus::NumericOutOfRange));
                        }
                    },
                    CypherValue::Float(f) => CypherValue::Float(-f),
                    CypherValue::Null => CypherValue::Null,
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "cannot negate {}",
                            val.type_name()
                        )));
                    }
                };
                sp += 1;
            }

            ByteOp::StartsWith => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = match (left, right) {
                    (CypherValue::String(a), CypherValue::String(b)) => {
                        CypherValue::Bool(a.starts_with(b.as_str()))
                    }
                    _ => CypherValue::Null,
                };
                sp += 1;
            }

            ByteOp::EndsWith => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = match (left, right) {
                    (CypherValue::String(a), CypherValue::String(b)) => {
                        CypherValue::Bool(a.ends_with(b.as_str()))
                    }
                    _ => CypherValue::Null,
                };
                sp += 1;
            }

            ByteOp::Contains => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = match (left, right) {
                    (CypherValue::String(a), CypherValue::String(b)) => {
                        CypherValue::Bool(a.contains(b.as_str()))
                    }
                    _ => CypherValue::Null,
                };
                sp += 1;
            }

            ByteOp::In => {
                sp -= 1;
                let right = &stack[sp];
                sp -= 1;
                let left = &stack[sp];
                stack[sp] = eval_in(left, right);
                sp += 1;
            }

            ByteOp::JumpIfFalse(target) => {
                // peek, don't pop: the value stays for And (or is the result if we jump)
                let val = &stack[sp - 1];
                if matches!(val, CypherValue::Bool(false)) {
                    pc = target as usize;
                    continue; // no pc increment
                }
            }

            ByteOp::JumpIfTrue(target) => {
                let val = &stack[sp - 1];
                if executor::is_truthy(val) {
                    pc = target as usize;
                    continue;
                }
            }

            ByteOp::PropNotInStrList {
                slot,
                prop_idx,
                list_idx,
            } => {
                use traverse_core::graph::types::PropertyKeyId;
                let key = &compiled.prop_keys[prop_idx as usize];
                let str_list = &compiled.string_lists[list_idx as usize];
                let val = &frame[slot as usize];
                let result = match val {
                    crate::frame::SlotValue::EdgeRef { id, .. } => {
                        // zero-alloc: get_edge gives a Cow, get_str inline
                        if let Some(pid_raw) = graph.strings().get(key) {
                            let pid = PropertyKeyId(pid_raw);
                            match graph.get_edge(*id) {
                                Ok(edge) => match edge.properties.get_str(pid) {
                                    Some(s) => !str_list.iter().any(|item| item == s),
                                    None => true, // null NOT IN list -> true
                                },
                                Err(_) => true,
                            }
                        } else {
                            true // unknown property key -> null -> NOT IN -> true
                        }
                    }
                    crate::frame::SlotValue::NodeRef(id) => {
                        // zero-alloc path for nodes too
                        if let Some(pid_raw) = graph.strings().get(key) {
                            let pid = PropertyKeyId(pid_raw);
                            match graph.get_node(*id) {
                                Ok(node) => match node.properties.get_str(pid) {
                                    Some(s) => !str_list.iter().any(|item| item == s),
                                    None => true,
                                },
                                Err(_) => true,
                            }
                        } else {
                            true
                        }
                    }
                    _ => true, // non-ref slot -> null -> NOT IN -> true
                };
                stack[sp] = CypherValue::Bool(result);
                sp += 1;
            }
        }

        pc += 1;
    }

    // the result is stack[0]
    debug_assert_eq!(sp, 1, "bytecode stack should have exactly 1 element");
    Ok(())
}

/// Evaluates compiled bytecode against a frame; returns the truthiness (filter use).
pub(crate) fn eval_bytecode(
    compiled: &CompiledExpr,
    frame: &Frame,
    graph: &dyn GraphView,
) -> Result<bool, CypherError> {
    let mut stack: [CypherValue; 8] = [
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
    ];
    eval_bytecode_impl(compiled, frame, graph, &mut stack)?;
    Ok(executor::is_truthy(&stack[0]))
}

/// Three-valued evaluation: `Some(true)`, `Some(false)` or `None` (null), for quantifiers (`ALL`,
/// `ANY`, `NONE`, `SINGLE`) that must tell null from false.
pub(crate) fn eval_bytecode_3val(
    compiled: &CompiledExpr,
    frame: &Frame,
    graph: &dyn GraphView,
) -> Result<Option<bool>, CypherError> {
    let mut stack: [CypherValue; 8] = [
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
    ];
    eval_bytecode_impl(compiled, frame, graph, &mut stack)?;
    match &stack[0] {
        CypherValue::Null => Ok(None),
        v => Ok(Some(executor::is_truthy(v))),
    }
}

// -- Arithmetic helpers --------------------------------------------------
// Numeric cases only, as filters need. Exotic ones (String+String, List+List,
// temporal) don't compile; eval_expr handles them.

#[inline(always)]
fn eval_arith_add(left: &CypherValue, right: &CypherValue) -> Result<CypherValue, CypherError> {
    if left.is_null() || right.is_null() {
        return Ok(CypherValue::Null);
    }
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            a.checked_add(*b).map(CypherValue::Integer).ok_or_else(|| {
                CypherError::type_error("long overflow".to_string())
                    .with_status(crate::status::GqlStatus::NumericOutOfRange)
            })
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a + b)),
        (CypherValue::Integer(a), CypherValue::Float(b)) => Ok(CypherValue::Float(*a as f64 + b)),
        (CypherValue::Float(a), CypherValue::Integer(b)) => Ok(CypherValue::Float(a + *b as f64)),
        (CypherValue::String(a), CypherValue::String(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        // Neo4j `+` concatenates strings with numbers/booleans
        (CypherValue::String(a), CypherValue::Integer(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        (CypherValue::String(a), CypherValue::Float(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        (CypherValue::String(a), CypherValue::Bool(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        (CypherValue::Integer(a), CypherValue::String(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        (CypherValue::Float(a), CypherValue::String(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        (CypherValue::Bool(a), CypherValue::String(b)) => {
            Ok(CypherValue::String(format!("{a}{b}")))
        }
        _ => Err(CypherError::type_error(format!(
            "cannot add {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

#[inline(always)]
fn eval_arith_sub(left: &CypherValue, right: &CypherValue) -> Result<CypherValue, CypherError> {
    if left.is_null() || right.is_null() {
        return Ok(CypherValue::Null);
    }
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            a.checked_sub(*b).map(CypherValue::Integer).ok_or_else(|| {
                CypherError::type_error("long overflow".to_string())
                    .with_status(crate::status::GqlStatus::NumericOutOfRange)
            })
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a - b)),
        (CypherValue::Integer(a), CypherValue::Float(b)) => Ok(CypherValue::Float(*a as f64 - b)),
        (CypherValue::Float(a), CypherValue::Integer(b)) => Ok(CypherValue::Float(a - *b as f64)),
        _ => Err(CypherError::type_error(format!(
            "cannot subtract {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

#[inline(always)]
fn eval_arith_mul(left: &CypherValue, right: &CypherValue) -> Result<CypherValue, CypherError> {
    if left.is_null() || right.is_null() {
        return Ok(CypherValue::Null);
    }
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            a.checked_mul(*b).map(CypherValue::Integer).ok_or_else(|| {
                CypherError::type_error("long overflow".to_string())
                    .with_status(crate::status::GqlStatus::NumericOutOfRange)
            })
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a * b)),
        (CypherValue::Integer(a), CypherValue::Float(b)) => Ok(CypherValue::Float(*a as f64 * b)),
        (CypherValue::Float(a), CypherValue::Integer(b)) => Ok(CypherValue::Float(a * *b as f64)),
        _ => Err(CypherError::type_error(format!(
            "cannot multiply {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

#[inline(always)]
fn eval_arith_div(left: &CypherValue, right: &CypherValue) -> Result<CypherValue, CypherError> {
    if left.is_null() || right.is_null() {
        return Ok(CypherValue::Null);
    }
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            if *b == 0 {
                return Err(CypherError::type_error("/ by zero".to_string())
                    .with_status(crate::status::GqlStatus::DivisionByZero));
            }
            Ok(CypherValue::Integer(a / b))
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a / b)),
        (CypherValue::Integer(a), CypherValue::Float(b)) => Ok(CypherValue::Float(*a as f64 / b)),
        (CypherValue::Float(a), CypherValue::Integer(b)) => Ok(CypherValue::Float(a / *b as f64)),
        _ => Err(CypherError::type_error(format!(
            "cannot divide {} by {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

#[inline(always)]
fn eval_arith_mod(left: &CypherValue, right: &CypherValue) -> Result<CypherValue, CypherError> {
    if left.is_null() || right.is_null() {
        return Ok(CypherValue::Null);
    }
    match (left, right) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            if *b == 0 {
                return Err(CypherError::type_error("/ by zero".to_string())
                    .with_status(crate::status::GqlStatus::DivisionByZero));
            }
            Ok(CypherValue::Integer(a % b))
        }
        (CypherValue::Float(a), CypherValue::Float(b)) => Ok(CypherValue::Float(a % b)),
        (CypherValue::Integer(a), CypherValue::Float(b)) => Ok(CypherValue::Float(*a as f64 % b)),
        (CypherValue::Float(a), CypherValue::Integer(b)) => Ok(CypherValue::Float(a % *b as f64)),
        _ => Err(CypherError::type_error(format!(
            "cannot modulo {} by {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

/// Cypher `value IN list`.
#[inline(always)]
fn eval_in(left: &CypherValue, right: &CypherValue) -> CypherValue {
    match right {
        CypherValue::List(items) => {
            if items.is_empty() {
                return CypherValue::Bool(false);
            }
            if left.is_null() {
                return CypherValue::Null;
            }
            let mut has_null = false;
            for item in items {
                match executor::eval_comparison(left, CmpOp::Eq, item) {
                    CypherValue::Bool(true) => return CypherValue::Bool(true),
                    CypherValue::Null => has_null = true,
                    _ => {}
                }
            }
            if has_null {
                CypherValue::Null
            } else {
                CypherValue::Bool(false)
            }
        }
        CypherValue::Null => CypherValue::Null,
        _ => CypherValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::SymbolTable;

    fn make_symtab(vars: &[&str]) -> SymbolTable {
        let mut st = SymbolTable::new();
        for v in vars {
            st.add(v.to_string());
        }
        st
    }

    #[test]
    fn compile_simple_comparison() {
        // n.age >= 18
        let expr = Expr::Comparison(
            Box::new(Expr::Property(
                Box::new(Expr::Variable("n".to_string())),
                "age".to_string(),
            )),
            vec![(CmpOp::Gte, Expr::Literal(Literal::Integer(18)))],
        );
        let symtab = make_symtab(&["n"]);
        let compiled = try_compile(&expr, &symtab);
        assert!(compiled.is_some(), "should compile n.age >= 18");

        let compiled = compiled.unwrap();
        // frame with n = NodeRef needs a graph to evaluate; just check bytecode exists
        assert!(!compiled.ops.is_empty());
        assert_eq!(compiled.prop_keys, vec!["age"]);
        assert_eq!(compiled.constants.len(), 1);
    }

    #[test]
    fn compile_and_short_circuit() {
        // n.age >= 18 AND n.age <= 65
        let left = Expr::Comparison(
            Box::new(Expr::Property(
                Box::new(Expr::Variable("n".to_string())),
                "age".to_string(),
            )),
            vec![(CmpOp::Gte, Expr::Literal(Literal::Integer(18)))],
        );
        let right = Expr::Comparison(
            Box::new(Expr::Property(
                Box::new(Expr::Variable("n".to_string())),
                "age".to_string(),
            )),
            vec![(CmpOp::Lte, Expr::Literal(Literal::Integer(65)))],
        );
        let expr = Expr::BinaryOp(Box::new(left), BinOp::And, Box::new(right));
        let symtab = make_symtab(&["n"]);
        let compiled = try_compile(&expr, &symtab);
        assert!(compiled.is_some());
    }

    #[test]
    fn unsupported_returns_none() {
        // FunctionCall isn't supported
        let expr = Expr::FunctionCall {
            name: "size".to_string(),
            distinct: false,
            args: vec![Expr::Variable("x".to_string())],
        };
        let symtab = make_symtab(&["x"]);
        assert!(try_compile(&expr, &symtab).is_none());
    }
}
