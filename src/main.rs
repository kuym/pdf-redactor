use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

mod content;
mod crypt;
mod doc;
mod filters;
mod font;
mod object;
mod pdf;
mod replace;
mod script;

use filters::Compression;
use replace::{Fit, Options, RuleStats};
use script::Rule;

/// Scripted search and replace for the text and hyperlinks of a PDF.
///
/// Only text-showing operators and URI actions are edited. Every other object
/// is copied byte-for-byte, keeping object numbers, object streams, the
/// cross-reference style, compression and encryption of the input.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Input PDF file
    #[arg(short, long)]
    input: PathBuf,

    /// Output PDF file (not needed with --dry-run or --dump-text)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// JSON script: {"rules": [{"search": "...", "replace": "...", ...}]}
    #[arg(short, long, visible_alias = "script")]
    config: Option<PathBuf>,

    /// Add a literal rule on the command line (repeatable); applied after the script's rules
    #[arg(short, long, num_args = 2, value_names = ["SEARCH", "REPLACE"], action = clap::ArgAction::Append)]
    replace: Vec<String>,

    /// Password for encrypted PDFs (user or owner)
    #[arg(short, long, default_value = "")]
    password: String,

    /// Font used when the original font has no glyph for replacement text
    #[arg(long, value_enum, default_value_t = FallbackArg::Auto)]
    fallback_font: FallbackArg,

    /// How to handle replacements wider than the text they replace
    #[arg(long, value_enum, default_value_t = FitArg::Auto)]
    fit: FitArg,

    /// Compression effort for rewritten streams
    #[arg(long, value_enum, default_value_t = CompressionArg::Max)]
    compression: CompressionArg,

    /// Skip the size guard (see below) and accept a slightly larger file
    ///
    /// When a fallback font has to be added, the output could end up a few
    /// hundred bytes larger than the input. By default that is offset by
    /// losslessly re-deflating other streams with a stronger encoder.
    #[arg(long)]
    allow_growth: bool,

    /// Report what would change without writing anything
    #[arg(long)]
    dry_run: bool,

    /// Print the text and hyperlink URIs of each page as the matcher sees them, then exit
    #[arg(long)]
    dump_text: bool,

    /// Exit with an error if any match could not be replaced
    #[arg(long)]
    strict: bool,

    /// Rewrite every matched glyph even where the replacement repeats the original text
    #[arg(long, hide = true)]
    rewrite_all: bool,

    /// Only print errors
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FallbackArg {
    /// A standard font matching the original's style (serif, bold, italic, monospace)
    Auto,
    Helvetica,
    Times,
    Courier,
    /// Never substitute: skip replacements the original font cannot show
    None,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FitArg {
    /// Condense text only when it would run into text that follows it on the line
    Auto,
    /// Never let a rewritten run become wider than the original
    Strict,
    /// Always use natural widths
    None,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompressionArg {
    /// Zopfli (smallest; output is ordinary Flate data)
    Max,
    /// zlib level 9
    Best,
    /// zlib level 6
    Fast,
}

fn run() -> Result<bool> {
    let args = Args::parse();

    let mut specs = Vec::new();
    if let Some(path) = &args.config {
        let text = fs::read_to_string(path).with_context(|| format!("reading script {}", path.display()))?;
        specs.extend(script::parse_script(&text).with_context(|| format!("parsing script {}", path.display()))?);
    }
    for pair in args.replace.chunks(2) {
        specs.push(script::literal(pair[0].clone(), pair[1].clone()));
    }
    if specs.is_empty() && !args.dump_text {
        bail!("no rules given: use --config <script.json> and/or --replace <SEARCH> <REPLACE>");
    }
    let rules: Vec<Rule> = specs.into_iter().map(Rule::compile).collect::<Result<_>>()?;

    let bytes = fs::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?;
    let input_len = bytes.len();
    let mut pdf = pdf::Pdf::load(bytes, args.password.as_bytes())
        .with_context(|| format!("loading {}", args.input.display()))?;
    pdf.compression = match args.compression {
        CompressionArg::Max => Compression::Max,
        CompressionArg::Best => Compression::Best,
        CompressionArg::Fast => Compression::Fast,
    };
    if pdf.recovered && !args.quiet {
        eprintln!("warning: the cross-reference table was damaged and has been reconstructed");
    }

    let mut units = doc::discover(&pdf);
    let mut fonts = doc::Fonts::default();

    if args.dump_text {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let (page_links, other_links) = replace::extract_links(&pdf);
        // A closed pipe (e.g. `| head`) just ends the dump.
        let mut emit = |text: String| writeln!(out, "{text}").is_ok();
        'pages: for (index, links) in page_links.iter().enumerate() {
            let number = index + 1;
            for unit in units.iter().filter(|u| u.page == number) {
                match replace::extract_text(&pdf, &mut fonts, unit) {
                    Ok(text) => {
                        if !emit(format!("===== {} =====\n{text}", unit.label)) {
                            break 'pages;
                        }
                    }
                    Err(e) => eprintln!("warning: {}: {e:#}", unit.label),
                }
            }
            if !links.is_empty() && !emit(format!("===== page {number} links =====\n{}", links.join("\n"))) {
                break;
            }
        }
        if !other_links.is_empty() {
            emit(format!("===== other links (bookmarks, document actions) =====\n{}", other_links.join("\n")));
        }
        return Ok(true);
    }
    if args.output.is_none() && !args.dry_run {
        bail!("--output is required (or use --dry-run)");
    }

    let opts = Options {
        fallback: !matches!(args.fallback_font, FallbackArg::None),
        fallback_family: match args.fallback_font {
            FallbackArg::Helvetica => Some(font::Family::Sans),
            FallbackArg::Times => Some(font::Family::Serif),
            FallbackArg::Courier => Some(font::Family::Mono),
            _ => None,
        },
        fit: match args.fit {
            FitArg::Auto => Fit::Auto,
            FitArg::Strict => Fit::Strict,
            FitArg::None => Fit::None,
        },
        minimal: !args.rewrite_all,
    };

    let mut stats: Vec<RuleStats> = rules.iter().map(|_| RuleStats::default()).collect();
    let mut warnings: Vec<String> = Vec::new();

    // Pass 1: learn which glyphs each font is known to contain.
    let mut readable = vec![true; units.len()];
    for (i, unit) in units.iter().enumerate() {
        if let Err(e) = replace::scan_usage(&pdf, &mut fonts, unit) {
            readable[i] = false;
            warnings.push(format!("{}: content not readable, left untouched ({e:#})", unit.label));
        }
    }
    // Pass 2: replace.
    let mut done: HashSet<u32> = HashSet::new();
    for (i, unit) in units.iter_mut().enumerate() {
        if readable[i] {
            replace::process_unit(&mut pdf, &mut fonts, unit, &rules, &mut stats, &opts, &mut done)
                .with_context(|| format!("processing {}", unit.label))?;
        }
    }
    replace::process_links(&mut pdf, &rules, &mut stats);

    let mut all_ok = true;
    if !args.quiet {
        for w in &warnings {
            eprintln!("warning: {w}");
        }
    }
    for (rule, st) in rules.iter().zip(&stats) {
        if !args.quiet {
            let mut line = format!("{:?} -> {:?}: {} in text, {} in links", rule.spec.search, rule.spec.replace, st.text, st.links);
            if st.fallback > 0 {
                line.push_str(&format!(", {} using a fallback font", st.fallback));
            }
            if st.actual_text > 0 {
                line.push_str(&format!(", {} in /ActualText", st.actual_text));
            }
            if st.condensed > 0 {
                line.push_str(&format!(", {} condensed to fit", st.condensed));
            }
            println!("{line}");
        }
        for s in &st.skipped {
            all_ok = false;
            eprintln!("warning: {s}");
        }
    }

    if args.dry_run {
        if !args.quiet {
            println!("dry run: nothing written");
        }
        return Ok(all_ok || !args.strict);
    }
    let output = args.output.as_ref().unwrap();
    if args.strict && !all_ok {
        bail!("some matches could not be replaced (--strict); no output written");
    }
    let out_bytes = if pdf.is_dirty() {
        let mut bytes = pdf.write()?;
        if bytes.len() > input_len && !args.allow_growth {
            // Size guard: win the difference back from streams we did not otherwise touch.
            let need = bytes.len() - input_len;
            let (saved, count) = pdf.recompress_to_save(need);
            if count > 0 {
                bytes = pdf.write()?;
                if !args.quiet {
                    println!("size guard: losslessly re-deflated {count} other stream(s), saving {saved} bytes");
                }
            }
        }
        bytes
    } else {
        // Nothing matched: the output is the input, bit for bit.
        std::mem::take(&mut pdf.data)
    };
    fs::write(output, &out_bytes).with_context(|| format!("writing {}", output.display()))?;
    if !args.quiet {
        let delta = out_bytes.len() as i64 - input_len as i64;
        println!("wrote {} ({} bytes, {delta:+} vs input)", output.display(), out_bytes.len());
    }
    Ok(true)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
