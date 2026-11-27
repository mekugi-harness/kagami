pub mod normalize;
pub mod similar;

use crate::model::{Language, Span};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LitClass {
    Zero,
    One,
    Int,
    Float,
    Str,
    Bool(bool),
    Null,
    Exact(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpClass {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Other,
}

impl OpClass {
    pub fn is_commutative(self) -> bool {
        matches!(
            self,
            OpClass::Add
                | OpClass::Mul
                | OpClass::Eq
                | OpClass::Neq
                | OpClass::BitAnd
                | OpClass::BitOr
                | OpClass::BitXor
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UnOpClass {
    Not,
    Neg,
    Deref,
    Ref,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LoopKind {
    IterRange {
        start: Box<IrNode>,
        end: Box<IrNode>,
        step: Box<IrNode>,
    },
    IterSeq {
        seq: Box<IrNode>,
    },
    While {
        cond: Box<IrNode>,
    },
    Infinite,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IrNode {
    Seq(Vec<IrNode>),
    Loop {
        kind: LoopKind,
        var: Option<String>,
        body: Box<IrNode>,
    },
    MultiBranch {
        scrutinee: Option<Box<IrNode>>,
        arms: Vec<(IrNode, IrNode)>,
        default: Option<Box<IrNode>>,
    },
    Branch {
        cond: Box<IrNode>,
        then_br: Box<IrNode>,
        else_br: Option<Box<IrNode>>,
    },
    Return(Option<Box<IrNode>>),
    Break,
    Continue,
    PropagateErr(Box<IrNode>),
    Assign {
        dst: String,
        val: Box<IrNode>,
    },
    Call {
        callee: String,
        args: Vec<IrNode>,
    },
    BinOp {
        op: OpClass,
        lhs: Box<IrNode>,
        rhs: Box<IrNode>,
    },
    UnOp {
        op: UnOpClass,
        expr: Box<IrNode>,
    },
    Member {
        obj: Box<IrNode>,
        field: String,
    },
    Index {
        obj: Box<IrNode>,
        idx: Box<IrNode>,
    },
    Var(String),
    Lit(LitClass),
    Opaque(String),
}

impl IrNode {
    pub fn node_count(&self) -> usize {
        match self {
            IrNode::Seq(items) => 1 + items.iter().map(IrNode::node_count).sum::<usize>(),
            IrNode::Loop { kind, body, .. } => {
                let k = match kind {
                    LoopKind::IterRange { start, end, step } => {
                        start.node_count() + end.node_count() + step.node_count()
                    }
                    LoopKind::IterSeq { seq } => seq.node_count(),
                    LoopKind::While { cond } => cond.node_count(),
                    LoopKind::Infinite => 0,
                };
                1 + k + body.node_count()
            }
            IrNode::MultiBranch {
                scrutinee,
                arms,
                default,
            } => {
                1 + scrutinee.as_ref().map_or(0, |s| s.node_count())
                    + arms
                        .iter()
                        .map(|(k, v)| k.node_count() + v.node_count())
                        .sum::<usize>()
                    + default.as_ref().map_or(0, |d| d.node_count())
            }
            IrNode::Branch {
                cond,
                then_br,
                else_br,
            } => {
                1 + cond.node_count()
                    + then_br.node_count()
                    + else_br.as_ref().map_or(0, |e| e.node_count())
            }
            IrNode::Return(v) => 1 + v.as_ref().map_or(0, |x| x.node_count()),
            IrNode::Break | IrNode::Continue | IrNode::Var(_) | IrNode::Lit(_) | IrNode::Opaque(_) => 1,
            IrNode::PropagateErr(e) | IrNode::UnOp { expr: e, .. } => 1 + e.node_count(),
            IrNode::Assign { val, .. } => 1 + val.node_count(),
            IrNode::Call { args, .. } => 1 + args.iter().map(IrNode::node_count).sum::<usize>(),
            IrNode::BinOp { lhs, rhs, .. } => 1 + lhs.node_count() + rhs.node_count(),
            IrNode::Member { obj, .. } => 1 + obj.node_count(),
            IrNode::Index { obj, idx } => 1 + obj.node_count() + idx.node_count(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrFunc {
    pub symbol_id: String,
    pub name: String,
    pub file: String,
    pub lang: Language,
    pub span: Span,
    pub params: Vec<String>,
    pub body: IrNode,
}
