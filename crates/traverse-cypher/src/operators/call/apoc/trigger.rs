//! `apoc.trigger.*`: trigger management.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::operators::call::ProcCall;
use crate::types::CypherValue;

/// `apoc.trigger.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall { frames, symtab, .. } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // -- apoc.trigger.* - registry management. All 12 spellings
        // share one YIELD shape; install/drop/show/start/stop take a
        // leading databaseName which the process-wide registry ignores
        // (documented divergence: triggers are process-lifetime).
        "apoc.trigger.add"
        | "apoc.trigger.install"
        | "apoc.trigger.remove"
        | "apoc.trigger.drop"
        | "apoc.trigger.removeall"
        | "apoc.trigger.dropall"
        | "apoc.trigger.list"
        | "apoc.trigger.show"
        | "apoc.trigger.pause"
        | "apoc.trigger.stop"
        | "apoc.trigger.resume"
        | "apoc.trigger.start" => {
            // Argument offset: the install/drop/show/start/stop family
            // has databaseName first.
            let off = usize::from(matches!(
                name_lower.as_str(),
                "apoc.trigger.install"
                    | "apoc.trigger.drop"
                    | "apoc.trigger.dropall"
                    | "apoc.trigger.show"
                    | "apoc.trigger.start"
                    | "apoc.trigger.stop"
            ));
            let emit = |frames: &[Frame], ts: &[crate::apoc_trigger::Trigger]| -> Vec<Frame> {
                let mut out = Vec::new();
                for frame in frames {
                    for t in ts {
                        let mut nf = frame.clone();
                        for (col, val) in [
                            ("name", CypherValue::String(t.name.clone())),
                            ("query", CypherValue::String(t.statement.clone())),
                            ("selector", CypherValue::Map(t.selector.clone())),
                            ("params", CypherValue::Map(t.params.clone())),
                            ("installed", CypherValue::Bool(true)),
                            ("paused", CypherValue::Bool(t.paused)),
                        ] {
                            if let Some(sl) = cx.slot_for(col) {
                                nf[sl] = val.into();
                            }
                        }
                        out.push(nf);
                    }
                }
                out
            };
            match name_lower.as_str() {
                "apoc.trigger.add" | "apoc.trigger.install" => {
                    let mut out = Vec::new();
                    for frame in frames {
                        let name = match eval_expr(cx.arg(off)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => continue,
                        };
                        let statement = match eval_expr(cx.arg(off + 1)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => continue,
                        };
                        let selector = match cx.opt_arg(off + 2, frame, &*graph) {
                            Some(CypherValue::Map(m)) => m,
                            _ => Default::default(),
                        };
                        let params = match cx.opt_arg(off + 3, frame, &*graph) {
                            Some(CypherValue::Map(m)) => match m.get("params") {
                                Some(CypherValue::Map(p)) => p.clone(),
                                _ => Default::default(),
                            },
                            _ => Default::default(),
                        };
                        // Validate the statement parses NOW (clear error
                        // at registration, not at first fire).
                        crate::parser::parse(&statement)?;
                        let t = crate::apoc_trigger::Trigger {
                            name,
                            statement,
                            selector,
                            params,
                            paused: false,
                        };
                        crate::apoc_trigger::install(t.clone());
                        out.extend(emit(std::slice::from_ref(frame), &[t]));
                    }
                    Ok(out)
                }
                "apoc.trigger.remove" | "apoc.trigger.drop" => {
                    let mut out = Vec::new();
                    for frame in frames {
                        let name = match eval_expr(cx.arg(off)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => continue,
                        };
                        if let Some(t) = crate::apoc_trigger::remove(&name) {
                            out.extend(emit(std::slice::from_ref(frame), &[t]));
                        }
                    }
                    Ok(out)
                }
                "apoc.trigger.removeall" | "apoc.trigger.dropall" => {
                    let ts = crate::apoc_trigger::remove_all();
                    Ok(emit(frames, &ts))
                }
                "apoc.trigger.list" | "apoc.trigger.show" => {
                    Ok(emit(frames, &crate::apoc_trigger::list()))
                }
                _ => {
                    // pause/stop (true) vs resume/start (false)
                    let paused = matches!(
                        name_lower.as_str(),
                        "apoc.trigger.pause" | "apoc.trigger.stop"
                    );
                    let mut out = Vec::new();
                    for frame in frames {
                        let name = match eval_expr(cx.arg(off)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => continue,
                        };
                        if let Some(t) = crate::apoc_trigger::set_paused(&name, paused) {
                            out.extend(emit(std::slice::from_ref(frame), &[t]));
                        }
                    }
                    Ok(out)
                }
            }
        }
        other => Err(super::unknown(other)),
    }
}
