use crate::ir::{IrFunc, IrNode, LitClass, LoopKind, OpClass, UnOpClass};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Default)]
pub struct NormalizeOptions {
    pub strict_literals: bool,
}

pub fn normalize_func(func: &IrFunc, opts: NormalizeOptions) -> IrNode {
    let mut node = desugar_node(func.body.clone());
    let mut env = AlphaEnv::new();
    for p in &func.params {
        env.bind(p);
    }
    node = alpha_rename(node, &mut env, opts);
    canonical_sort(node)
}

fn flatten_seq(items: Vec<IrNode>) -> IrNode {
    let mut flat = Vec::new();
    for item in items {
        match item {
            IrNode::Seq(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    if flat.len() == 1 {
        flat.pop().unwrap()
    } else {
        IrNode::Seq(flat)
    }
}

fn desugar_node(node: IrNode) -> IrNode {
    match node {
        IrNode::Seq(items) => {
            let mut processed: Vec<IrNode> = items.into_iter().map(desugar_node).collect();
            processed = desugar_seq_patterns(processed);
            flatten_seq(processed)
        }
        IrNode::Loop { kind, var, body } => {
            let kind = match kind {
                LoopKind::IterRange { start, end, step } => LoopKind::IterRange {
                    start: Box::new(desugar_node(*start)),
                    end: Box::new(desugar_node(*end)),
                    step: Box::new(desugar_node(*step)),
                },
                LoopKind::IterSeq { seq } => LoopKind::IterSeq {
                    seq: Box::new(desugar_node(*seq)),
                },
                LoopKind::While { cond } => LoopKind::While {
                    cond: Box::new(desugar_node(*cond)),
                },
                LoopKind::Infinite => LoopKind::Infinite,
            };
            IrNode::Loop {
                kind,
                var,
                body: Box::new(desugar_node(*body)),
            }
        }
        IrNode::Branch {
            cond,
            then_br,
            else_br,
        } => {
            let cond = desugar_node(*cond);
            let then_br = desugar_node(*then_br);
            let else_br = else_br.map(|e| desugar_node(*e));
            if let Some(mb) = try_lift_if_chain_to_multibranch(&cond, &then_br, else_br.as_ref()) {
                return mb;
            }
            if is_err_check(&cond) && else_br.is_none() && is_return_only(&then_br) {
                return IrNode::PropagateErr(Box::new(IrNode::Var("err".into())));
            }
            IrNode::Branch {
                cond: Box::new(cond),
                then_br: Box::new(then_br),
                else_br: else_br.map(Box::new),
            }
        }
        IrNode::MultiBranch {
            scrutinee,
            arms,
            default,
        } => {
            let scrutinee = scrutinee.map(|s| Box::new(desugar_node(*s)));
            let arms = arms
                .into_iter()
                .map(|(k, v)| (desugar_node(k), desugar_node(v)))
                .collect();
            let default = default.map(|d| Box::new(desugar_node(*d)));
            IrNode::MultiBranch {
                scrutinee,
                arms,
                default,
            }
        }
        IrNode::Return(v) => IrNode::Return(v.map(|x| Box::new(desugar_node(*x)))),
        IrNode::PropagateErr(e) => IrNode::PropagateErr(Box::new(desugar_node(*e))),
        IrNode::Assign { dst, val } => IrNode::Assign {
            dst,
            val: Box::new(desugar_node(*val)),
        },
        IrNode::Call { callee, args } => IrNode::Call {
            callee,
            args: args.into_iter().map(desugar_node).collect(),
        },
        IrNode::BinOp { op, lhs, rhs } => IrNode::BinOp {
            op,
            lhs: Box::new(desugar_node(*lhs)),
            rhs: Box::new(desugar_node(*rhs)),
        },
        IrNode::UnOp { op, expr } => IrNode::UnOp {
            op,
            expr: Box::new(desugar_node(*expr)),
        },
        IrNode::Member { obj, field } => IrNode::Member {
            obj: Box::new(desugar_node(*obj)),
            field,
        },
        IrNode::Index { obj, idx } => IrNode::Index {
            obj: Box::new(desugar_node(*obj)),
            idx: Box::new(desugar_node(*idx)),
        },
        other => other,
    }
}

fn desugar_seq_patterns(items: Vec<IrNode>) -> Vec<IrNode> {
    let mut out = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < items.len() {
        if i + 1 < items.len() {
            if let (
                IrNode::Assign { dst, val },
                IrNode::PropagateErr(inner),
            ) = (&items[i], &items[i + 1])
            {
                if matches!(inner.as_ref(), IrNode::Var(v) if v == dst || v == "err") {
                    out.push(IrNode::PropagateErr(val.clone()));
                    i += 2;
                    continue;
                }
            }
        }
        if i + 1 < items.len() {
            if let IrNode::Branch {
                cond,
                then_br,
                else_br: None,
            } = &items[i]
            {
                if is_return_only(then_br) {
                    if let IrNode::UnOp {
                        op: UnOpClass::Not,
                        expr,
                    } = cond.as_ref()
                    {
                        let rest = flatten_seq(items[i + 1..].to_vec());
                        out.push(IrNode::Branch {
                            cond: expr.clone(),
                            then_br: Box::new(rest),
                            else_br: Some(then_br.clone()),
                        });
                        return out;
                    }
                }
            }
        }
        out.push(items[i].clone());
        i += 1;
    }
    out
}

fn is_err_check(cond: &IrNode) -> bool {
    match cond {
        IrNode::BinOp {
            op: OpClass::Neq,
            lhs,
            rhs,
        } => {
            (matches!(lhs.as_ref(), IrNode::Var(v) if v == "err")
                && matches!(rhs.as_ref(), IrNode::Lit(LitClass::Null)))
                || (matches!(rhs.as_ref(), IrNode::Var(v) if v == "err")
                    && matches!(lhs.as_ref(), IrNode::Lit(LitClass::Null)))
        }
        _ => false,
    }
}

fn is_return_only(node: &IrNode) -> bool {
    match node {
        IrNode::Return(_) => true,
        IrNode::Seq(items) => items.len() == 1 && matches!(items[0], IrNode::Return(_)),
        _ => false,
    }
}

fn extract_eq_scrutinee<'a>(cond: &'a IrNode) -> Option<(&'a IrNode, &'a IrNode)> {
    if let IrNode::BinOp {
        op: OpClass::Eq,
        lhs,
        rhs,
    } = cond
    {
        if matches!(lhs.as_ref(), IrNode::Var(_) | IrNode::Member { .. }) {
            return Some((lhs.as_ref(), rhs.as_ref()));
        }
        if matches!(rhs.as_ref(), IrNode::Var(_) | IrNode::Member { .. }) {
            return Some((rhs.as_ref(), lhs.as_ref()));
        }
    }
    None
}

fn try_lift_if_chain_to_multibranch(
    cond: &IrNode,
    then_br: &IrNode,
    else_br: Option<&IrNode>,
) -> Option<IrNode> {
    let (scrutinee, pat) = extract_eq_scrutinee(cond)?;
    let mut arms = vec![(pat.clone(), then_br.clone())];
    let mut default = None;
    let mut cur = else_br;

    while let Some(next) = cur {
        match next {
            IrNode::MultiBranch {
                scrutinee: Some(s),
                arms: sub_arms,
                default: sub_def,
            } if s.as_ref() == scrutinee => {
                arms.extend(sub_arms.clone());
                default = sub_def.clone();
                cur = None;
            }
            IrNode::Branch {
                cond: c2,
                then_br: t2,
                else_br: e2,
            } => {
                if let Some((s2, p2)) = extract_eq_scrutinee(c2) {
                    if s2 == scrutinee {
                        arms.push((p2.clone(), (**t2).clone()));
                        cur = e2.as_deref();
                        continue;
                    }
                }
                return None;
            }
            other => {
                default = Some(Box::new(other.clone()));
                cur = None;
            }
        }
    }

    if arms.len() >= 2 {
        Some(IrNode::MultiBranch {
            scrutinee: Some(Box::new(scrutinee.clone())),
            arms,
            default,
        })
    } else {
        None
    }
}

struct AlphaEnv {
    map: HashMap<String, String>,
    next: usize,
}

impl AlphaEnv {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: 0,
        }
    }

    fn bind(&mut self, name: &str) -> String {
        if let Some(existing) = self.map.get(name) {
            return existing.clone();
        }
        let slot = format!("${}", self.next);
        self.next += 1;
        self.map.insert(name.to_string(), slot.clone());
        slot
    }

    fn resolve(&mut self, name: &str) -> String {
        if let Some(existing) = self.map.get(name) {
            existing.clone()
        } else {
            self.bind(name)
        }
    }
}

fn alpha_rename(node: IrNode, env: &mut AlphaEnv, opts: NormalizeOptions) -> IrNode {
    match node {
        IrNode::Seq(items) => IrNode::Seq(
            items
                .into_iter()
                .map(|n| alpha_rename(n, env, opts))
                .collect(),
        ),
        IrNode::Loop { kind, var, body } => {
            let var = var.map(|v| env.bind(&v));
            let kind = match kind {
                LoopKind::IterRange { start, end, step } => LoopKind::IterRange {
                    start: Box::new(alpha_rename(*start, env, opts)),
                    end: Box::new(alpha_rename(*end, env, opts)),
                    step: Box::new(alpha_rename(*step, env, opts)),
                },
                LoopKind::IterSeq { seq } => LoopKind::IterSeq {
                    seq: Box::new(alpha_rename(*seq, env, opts)),
                },
                LoopKind::While { cond } => LoopKind::While {
                    cond: Box::new(alpha_rename(*cond, env, opts)),
                },
                LoopKind::Infinite => LoopKind::Infinite,
            };
            IrNode::Loop {
                kind,
                var,
                body: Box::new(alpha_rename(*body, env, opts)),
            }
        }
        IrNode::MultiBranch {
            scrutinee,
            arms,
            default,
        } => IrNode::MultiBranch {
            scrutinee: scrutinee.map(|s| Box::new(alpha_rename(*s, env, opts))),
            arms: arms
                .into_iter()
                .map(|(k, v)| (alpha_rename(k, env, opts), alpha_rename(v, env, opts)))
                .collect(),
            default: default.map(|d| Box::new(alpha_rename(*d, env, opts))),
        },
        IrNode::Branch {
            cond,
            then_br,
            else_br,
        } => IrNode::Branch {
            cond: Box::new(alpha_rename(*cond, env, opts)),
            then_br: Box::new(alpha_rename(*then_br, env, opts)),
            else_br: else_br.map(|e| Box::new(alpha_rename(*e, env, opts))),
        },
        IrNode::Return(v) => IrNode::Return(v.map(|x| Box::new(alpha_rename(*x, env, opts)))),
        IrNode::PropagateErr(e) => IrNode::PropagateErr(Box::new(alpha_rename(*e, env, opts))),
        IrNode::Assign { dst, val } => {
            let val = alpha_rename(*val, env, opts);
            let dst = env.bind(&dst);
            IrNode::Assign {
                dst,
                val: Box::new(val),
            }
        }
        IrNode::Call { callee, args } => IrNode::Call {
            callee,
            args: args
                .into_iter()
                .map(|a| alpha_rename(a, env, opts))
                .collect(),
        },
        IrNode::BinOp { op, lhs, rhs } => IrNode::BinOp {
            op,
            lhs: Box::new(alpha_rename(*lhs, env, opts)),
            rhs: Box::new(alpha_rename(*rhs, env, opts)),
        },
        IrNode::UnOp { op, expr } => IrNode::UnOp {
            op,
            expr: Box::new(alpha_rename(*expr, env, opts)),
        },
        IrNode::Member { obj, field } => IrNode::Member {
            obj: Box::new(alpha_rename(*obj, env, opts)),
            field,
        },
        IrNode::Index { obj, idx } => IrNode::Index {
            obj: Box::new(alpha_rename(*obj, env, opts)),
            idx: Box::new(alpha_rename(*idx, env, opts)),
        },
        IrNode::Var(v) => IrNode::Var(env.resolve(&v)),
        IrNode::Lit(lit) => {
            if opts.strict_literals {
                IrNode::Lit(lit)
            } else {
                let bucketed = match lit {
                    LitClass::Exact(ref s) => bucket_literal(s),
                    other => other,
                };
                IrNode::Lit(bucketed)
            }
        }
        other => other,
    }
}

pub fn bucket_literal(raw: &str) -> LitClass {
    let t = raw.trim();
    if t == "0" {
        LitClass::Zero
    } else if t == "1" {
        LitClass::One
    } else if t == "true" {
        LitClass::Bool(true)
    } else if t == "false" {
        LitClass::Bool(false)
    } else if t == "nil" || t == "null" || t == "undefined" || t == "None" {
        LitClass::Null
    } else if t.starts_with('"') || t.starts_with('\'') || t.starts_with('`') {
        LitClass::Str
    } else if t.parse::<i64>().is_ok() {
        LitClass::Int
    } else if t.parse::<f64>().is_ok() {
        LitClass::Float
    } else {
        LitClass::Str
    }
}

fn canonical_sort(node: IrNode) -> IrNode {
    match node {
        IrNode::Seq(items) => IrNode::Seq(items.into_iter().map(canonical_sort).collect()),
        IrNode::Loop { kind, var, body } => {
            let kind = match kind {
                LoopKind::IterRange { start, end, step } => LoopKind::IterRange {
                    start: Box::new(canonical_sort(*start)),
                    end: Box::new(canonical_sort(*end)),
                    step: Box::new(canonical_sort(*step)),
                },
                LoopKind::IterSeq { seq } => LoopKind::IterSeq {
                    seq: Box::new(canonical_sort(*seq)),
                },
                LoopKind::While { cond } => LoopKind::While {
                    cond: Box::new(canonical_sort(*cond)),
                },
                LoopKind::Infinite => LoopKind::Infinite,
            };
            IrNode::Loop {
                kind,
                var,
                body: Box::new(canonical_sort(*body)),
            }
        }
        IrNode::MultiBranch {
            scrutinee,
            arms,
            default,
        } => {
            let mut arms: Vec<(IrNode, IrNode)> = arms
                .into_iter()
                .map(|(k, v)| (canonical_sort(k), canonical_sort(v)))
                .collect();
            arms.sort();
            IrNode::MultiBranch {
                scrutinee: scrutinee.map(|s| Box::new(canonical_sort(*s))),
                arms,
                default: default.map(|d| Box::new(canonical_sort(*d))),
            }
        }
        IrNode::Branch {
            cond,
            then_br,
            else_br,
        } => IrNode::Branch {
            cond: Box::new(canonical_sort(*cond)),
            then_br: Box::new(canonical_sort(*then_br)),
            else_br: else_br.map(|e| Box::new(canonical_sort(*e))),
        },
        IrNode::BinOp { op, lhs, rhs } => {
            let mut l = canonical_sort(*lhs);
            let mut r = canonical_sort(*rhs);
            if op.is_commutative() && l > r {
                std::mem::swap(&mut l, &mut r);
            }
            IrNode::BinOp {
                op,
                lhs: Box::new(l),
                rhs: Box::new(r),
            }
        }
        IrNode::UnOp { op, expr } => IrNode::UnOp {
            op,
            expr: Box::new(canonical_sort(*expr)),
        },
        IrNode::Return(v) => IrNode::Return(v.map(|x| Box::new(canonical_sort(*x)))),
        IrNode::PropagateErr(e) => IrNode::PropagateErr(Box::new(canonical_sort(*e))),
        IrNode::Assign { dst, val } => IrNode::Assign {
            dst,
            val: Box::new(canonical_sort(*val)),
        },
        IrNode::Call { callee, args } => IrNode::Call {
            callee,
            args: args.into_iter().map(canonical_sort).collect(),
        },
        IrNode::Member { obj, field } => IrNode::Member {
            obj: Box::new(canonical_sort(*obj)),
            field,
        },
        IrNode::Index { obj, idx } => IrNode::Index {
            obj: Box::new(canonical_sort(*obj)),
            idx: Box::new(canonical_sort(*idx)),
        },
        other => other,
    }
}
