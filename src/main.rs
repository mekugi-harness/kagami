use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use kagami::config::{build_path_filter, load_allowlist, KagamiConfig};
use kagami::graph::blast::{analyze_blast_radius, BlastOptions, Direction};
use kagami::graph::dead::{
    analyze_dead, analyze_dead_snapshot, DeadKinds, DeadOptions, DeadScope,
};
use kagami::graph::scip::{apply_scip_overlay, load_scip_file};
use kagami::graph::CodeGraph;
use kagami::ir::similar::{find_similar, SimilarOptions};
use kagami::model::{Confidence, Language};
use kagami::output::{render_blast, render_dead, render_similar, Format};
use kagami::walker::discover_files;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "kagami",
    version,
    about = "Polyglot code-graph & canonical-IR analyzer (blast-radius, dead, similar)"
)]
struct Cli {
    /// Repository root directory
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    /// Restrict scope to matching relative path globs (repeatable)
    #[arg(long = "path", global = true)]
    paths: Vec<String>,

    /// Disable incremental cache (.kagami/)
    #[arg(long, global = true)]
    no_cache: bool,

    /// Optional SCIP index file(s) (.scip or .scip.json) for compiler-exact edge precision
    #[arg(long = "scip", global = true)]
    scip_files: Vec<PathBuf>,

    /// Output format
    #[arg(long, global = true, value_enum, default_value = "md")]
    format: CliFormat,

    #[command(subcommand)]
    command: Commands,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum CliFormat {
    Md,
    Json,
}

impl From<CliFormat> for Format {
    fn from(f: CliFormat) -> Self {
        match f {
            CliFormat::Md => Format::Md,
            CliFormat::Json => Format::Json,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum CliDirection {
    Callers,
    Callees,
    Both,
}

impl From<CliDirection> for Direction {
    fn from(d: CliDirection) -> Self {
        match d {
            CliDirection::Callers => Direction::Callers,
            CliDirection::Callees => Direction::Callees,
            CliDirection::Both => Direction::Both,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum CliDeadScope {
    Prod,
    All,
}

impl From<CliDeadScope> for DeadScope {
    fn from(s: CliDeadScope) -> Self {
        match s {
            CliDeadScope::Prod => DeadScope::Prod,
            CliDeadScope::All => DeadScope::All,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum CliDeadKinds {
    Callables,
    Types,
    All,
}

impl From<CliDeadKinds> for DeadKinds {
    fn from(k: CliDeadKinds) -> Self {
        match k {
            CliDeadKinds::Callables => DeadKinds::Callables,
            CliDeadKinds::Types => DeadKinds::Types,
            CliDeadKinds::All => DeadKinds::All,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum CliConfidence {
    Heuristic,
    Resolved,
    Exact,
}

impl From<CliConfidence> for Confidence {
    fn from(c: CliConfidence) -> Self {
        match c {
            CliConfidence::Heuristic => Confidence::Heuristic,
            CliConfidence::Resolved => Confidence::Resolved,
            CliConfidence::Exact => Confidence::Exact,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Return a symbol, its references, and relationships across languages
    BlastRadius {
        /// File path relative to repo root
        file_path: String,
        /// Symbol name (or Type.Method / Type::method)
        symbol: String,
        /// Max traversal depth (0 = unbounded)
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Traversal direction
        #[arg(long, value_enum, default_value = "both")]
        direction: CliDirection,
        /// Disable cross-language bridge traversal
        #[arg(long)]
        no_cross_lang: bool,
        /// Include environment variable bridges
        #[arg(long)]
        include_env: bool,
        /// Exclude test symbols
        #[arg(long)]
        no_tests: bool,
    },
    /// Return symbols with no reachable call site
    Dead {
        /// Reachability scope (prod excludes test callers)
        #[arg(long, value_enum, default_value = "prod")]
        scope: CliDeadScope,
        /// Comma-separated language filter (go,rust,ts,js,svelte,just,sh)
        #[arg(long)]
        lang: Option<String>,
        /// Explicit allowlist path (defaults to .deadcode-allowlist if present)
        #[arg(long)]
        allowlist: Option<PathBuf>,
        /// Exclude unreachable call cycles (report only 0-caller symbols)
        #[arg(long)]
        no_clusters: bool,
        /// Minimum confidence level to report (heuristic, resolved, exact)
        #[arg(long = "min-confidence", value_enum, default_value = "heuristic")]
        min_confidence: CliConfidence,
        /// Symbol kinds to report (callables: functions/methods/components/recipes; types: types/structs/interfaces/enums/traits/consts/vars; all: both)
        #[arg(long, value_enum, default_value = "callables")]
        kinds: CliDeadKinds,
        /// Max rows in markdown output (0 = unlimited)
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Return a JSON/Markdown table of structurally similar functions via canonical IR
    Similar {
        /// Minimum similarity score in [0.0, 1.0]
        #[arg(long = "min-similarity", default_value_t = 0.85)]
        min_similarity: f64,
        /// Minimum canonical IR nodes per function
        #[arg(long, default_value_t = 12)]
        min_nodes: usize,
        /// Compare functions across different languages
        #[arg(long)]
        cross_lang: bool,
        /// Preserve exact literal values instead of type-class bucketing
        #[arg(long)]
        strict_literals: bool,
        /// Max pairs to return (0 = unlimited)
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.root.canonicalize().unwrap_or(cli.root.clone());
    let cfg = KagamiConfig::load(&root)?;
    let extra_ignores = cfg.build_ignore_globset()?;
    let test_paths = cfg.build_test_paths_globset()?;
    let path_filter = build_path_filter(&cli.paths)?;
    let fmt: Format = cli.format.into();

    match cli.command {
        Commands::BlastRadius {
            file_path,
            symbol,
            depth,
            direction,
            no_cross_lang,
            include_env,
            no_tests,
        } => {
            // Always index whole repo for blast-radius so cross-file/cross-lang callers are visible
            let files = discover_files(&root, &extra_ignores, path_filter.as_ref())?;
            let mut graph = if path_filter.is_some() {
                CodeGraph::build_for_scoped_graph(&root, &files, !cli.no_cache)?
            } else {
                CodeGraph::build_for_graph(&root, &files, !cli.no_cache)?
            };
            if let Some(ref tp) = test_paths {
                graph.apply_test_paths(tp);
            }
            for scip_path in &cli.scip_files {
                let idx = load_scip_file(scip_path)?;
                apply_scip_overlay(&mut graph, &idx, &root);
            }
            let report = analyze_blast_radius(
                &graph,
                &file_path,
                &symbol,
                &BlastOptions {
                    depth,
                    direction: direction.into(),
                    cross_lang: !no_cross_lang,
                    include_env,
                    include_tests: !no_tests,
                },
            )?;
            print!("{}", render_blast(&report, fmt)?);
        }
        Commands::Dead {
            scope,
            lang,
            allowlist,
            no_clusters,
            min_confidence,
            kinds,
            limit,
        } => {
            let prof = std::env::var_os("KAGAMI_PROFILE").is_some();
            let t0 = std::time::Instant::now();
            // Index whole repo so callers outside --path still keep symbols alive
            let files = discover_files(&root, &extra_ignores, None)?;
            let t_walk = t0.elapsed();
            let langs = lang.map(|l| {
                l.split(',')
                    .filter_map(Language::parse)
                    .collect::<HashSet<_>>()
            });
            let allow_path = allowlist.or_else(|| cfg.dead.allowlist.as_ref().map(PathBuf::from));
            let allow = load_allowlist(&root, allow_path.as_deref());
            let dead_opts = DeadOptions {
                scope: scope.into(),
                langs,
                allowlist: allow,
                include_clusters: !no_clusters,
                path_filter,
                min_confidence: min_confidence.into(),
                kinds: kinds.into(),
            };
            let (t_build, t_pre_dead, report) = if !cli.no_cache
                && cli.scip_files.is_empty()
                && let Some(mut snap) = CodeGraph::try_load_cached_snapshot(&root, &files)
            {
                let tb = t0.elapsed();
                if let Some(ref tp) = test_paths {
                    for sym in &mut snap.symbols {
                        if tp.is_match(&sym.file) {
                            sym.is_test = true;
                        }
                    }
                }
                let tp_dead = t0.elapsed();
                let rep = analyze_dead_snapshot(&snap, &dead_opts);
                (tb, tp_dead, rep)
            } else {
                let mut graph = CodeGraph::build_for_graph(&root, &files, !cli.no_cache)?;
                let tb = t0.elapsed();
                if let Some(ref tp) = test_paths {
                    graph.apply_test_paths(tp);
                }
                for scip_path in &cli.scip_files {
                    let idx = load_scip_file(scip_path)?;
                    apply_scip_overlay(&mut graph, &idx, &root);
                }
                let tp_dead = t0.elapsed();
                let rep = analyze_dead(&graph, &dead_opts);
                (tb, tp_dead, rep)
            };
            if prof {
                let t_dead = t0.elapsed() - t_pre_dead;
                eprintln!("[profile] discover_files: {:.3}s ({} files)", t_walk.as_secs_f64(), files.len());
                eprintln!("[profile] CodeGraph::build total: {:.3}s", (t_build - t_walk).as_secs_f64());
                eprintln!("[profile] analyze_dead (RTA): {:.3}s", t_dead.as_secs_f64());
                eprintln!("[profile] total wall: {:.3}s", t0.elapsed().as_secs_f64());
            }
            print!("{}", render_dead(&report, fmt, limit)?);
        }
        Commands::Similar {
            min_similarity,
            min_nodes,
            cross_lang,
            strict_literals,
            limit,
        } => {
            let files = discover_files(&root, &extra_ignores, path_filter.as_ref())?;
            let graph = CodeGraph::build(&root, &files, !cli.no_cache)?;
            let all_funcs: Vec<_> = graph
                .extracts
                .iter()
                .flat_map(|e| e.ir_funcs.iter().cloned())
                .collect();
            let pairs = find_similar(
                &all_funcs,
                &SimilarOptions {
                    min_similarity: min_similarity.clamp(0.0, 1.0),
                    min_nodes,
                    cross_lang,
                    strict_literals,
                    limit,
                },
            );
            print!("{}", render_similar(&pairs, fmt)?);
        }
    }

    Ok(())
}
