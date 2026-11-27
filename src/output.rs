use crate::graph::blast::BlastRadiusReport;
use crate::graph::dead::DeadReport;
use crate::ir::similar::SimilarPair;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Md,
    Json,
}

pub fn render_blast(report: &BlastRadiusReport, fmt: Format) -> Result<String> {
    match fmt {
        Format::Json => Ok(serde_json::to_string_pretty(report)?),
        Format::Md => {
            let mut out = String::new();
            out.push_str(&format!(
                "## Blast Radius: `{}` ({} — `{}:{}-{}`)\n\n",
                report.target.qualified_name,
                report.target.lang.as_str(),
                report.target.file,
                report.target.span.start_line,
                report.target.span.end_line
            ));
            out.push_str(&format!("Total impacted: **{}**", report.total_impacted));
            if !report.by_language.is_empty() {
                let langs: Vec<String> = report
                    .by_language
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect();
                out.push_str(&format!(" ({})", langs.join(", ")));
            }
            out.push_str("\n\n");
            if report.impacts.is_empty() {
                out.push_str("_No references or relationships found._\n");
                return Ok(out);
            }
            out.push_str("| Hop | Dir | Lang | Symbol | Location | Edge | Confidence | Bridge |\n");
            out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- |\n");
            for item in &report.impacts {
                let bridge = item.via_bridge.as_deref().unwrap_or("-");
                out.push_str(&format!(
                    "| {} | {} | {} | `{}` | `{}:{}` | {:?} | {:?} | {} |\n",
                    item.hop,
                    item.direction,
                    item.lang.as_str(),
                    item.symbol,
                    item.file,
                    item.span.start_line,
                    item.edge_kind,
                    item.confidence,
                    bridge
                ));
            }
            Ok(out)
        }
    }
}

pub fn render_dead(report: &DeadReport, fmt: Format, limit: usize) -> Result<String> {
    match fmt {
        Format::Json => Ok(serde_json::to_string_pretty(report)?),
        Format::Md => {
            let mut out = String::new();
            out.push_str(&format!(
                "## Dead Symbols: **{}** (unreferenced: {}, unreachable clusters: {}, total indexed symbols: {})\n\n",
                report.total_dead, report.unreferenced, report.unreachable_clusters, report.total_symbols
            ));
            if report.symbols.is_empty() {
                out.push_str("_No dead symbols found._\n");
                return Ok(out);
            }
            out.push_str("| Lang | Symbol | Kind | Location | Reason | Confidence |\n");
            out.push_str("| --- | --- | --- | --- | --- | --- |\n");
            let iter: Box<dyn Iterator<Item = _>> = if limit > 0 {
                Box::new(report.symbols.iter().take(limit))
            } else {
                Box::new(report.symbols.iter())
            };
            for sym in iter {
                out.push_str(&format!(
                    "| {} | `{}` | {:?} | `{}:{}` | {} | {:?} |\n",
                    sym.lang.as_str(),
                    sym.symbol,
                    sym.kind,
                    sym.file,
                    sym.span.start_line,
                    sym.reason,
                    sym.confidence
                ));
            }
            if limit > 0 && report.symbols.len() > limit {
                out.push_str(&format!(
                    "\n_Showing {} of {} dead symbols. Use `--limit 0` or `--format json` for all._\n",
                    limit, report.symbols.len()
                ));
            }
            Ok(out)
        }
    }
}

pub fn render_similar(pairs: &[SimilarPair], fmt: Format) -> Result<String> {
    match fmt {
        Format::Json => Ok(serde_json::to_string_pretty(pairs)?),
        Format::Md => {
            let mut out = String::new();
            out.push_str(&format!("## Similar Functions ({} pairs)\n\n", pairs.len()));
            if pairs.is_empty() {
                out.push_str("_No similar pairs found above threshold._\n");
                return Ok(out);
            }
            out.push_str("| Score | Kind | Symbol A | Symbol B | Nodes | Reason |\n");
            out.push_str("| --- | --- | --- | --- | --- | --- |\n");
            for p in pairs {
                out.push_str(&format!(
                    "| {:.3} | {} | `{}` (`{}:{}`) | `{}` (`{}:{}`) | {}/{} | {} |\n",
                    p.score,
                    p.match_kind,
                    p.symbol_a,
                    p.file_a,
                    p.span_a.start_line,
                    p.symbol_b,
                    p.file_b,
                    p.span_b.start_line,
                    p.ir_nodes_a,
                    p.ir_nodes_b,
                    p.reason
                ));
            }
            Ok(out)
        }
    }
}
