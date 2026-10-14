use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgGroup, Parser};
use rayon::prelude::*;

use nbripout::fsutil::write_atomic;
use nbripout::git::{self, Scope};
use nbripout::strip::{ExtraKeys, Options, OutputType, DEFAULT_EXTRA_KEYS};
use nbripout::{filter_process, py, strip_notebook, Error, Mode};

const AFTER_HELP: &str = "\
Examples:
  nbripout notebook.ipynb                 strip in place (atomically)
  cat in.ipynb | nbripout > out.ipynb     use as a pipe
  nbripout --verify *.ipynb               exit 1 if anything would be stripped (CI)
  nbripout --install                      set up the git filter in this repository
  nbripout --status                       show the git filter configuration

nbripout configures git exactly like nbstripout (filter name `nbstripout`,
diff driver `ipynb`), so existing .gitattributes keep working, and it honors
`git config filter.nbstripout.extrakeys` and `filter.nbstripout.keepmetadatakeys`.";

#[derive(Parser, Debug)]
#[command(
    name = "nbripout",
    version,
    about = "Strip output from Jupyter and Zeppelin notebooks.\n\
             A safer, faster, drop-in replacement for nbstripout.",
    after_help = AFTER_HELP,
    infer_long_args = true,
    group(ArgGroup::new("task").args(
        ["dry_run", "install", "uninstall", "is_installed", "status", "filter_process"]
    )),
    group(ArgGroup::new("location").args(["global", "system"])),
)]
struct Cli {
    /// Print which notebooks would have been stripped
    #[arg(long)]
    dry_run: bool,

    /// Install the git filter and attributes in the current repository
    #[arg(long)]
    install: bool,

    /// Uninstall the git filter and attributes from the current repository
    #[arg(long)]
    uninstall: bool,

    /// Check if the git filter is installed (exit code 0 if installed, 1 otherwise)
    #[arg(long)]
    is_installed: bool,

    /// Print the installation status and configuration summary
    #[arg(long)]
    status: bool,

    /// Serve git's long-running filter process protocol on stdin/stdout
    #[arg(long)]
    filter_process: bool,

    /// With --install: only configure the per-file clean command, exactly like
    /// nbstripout, instead of git's long-running filter process
    #[arg(long, requires = "install")]
    no_process: bool,

    /// Return a non-zero exit code if any files would be changed (implies --dry-run)
    #[arg(long)]
    verify: bool,

    /// Do not strip the execution count/prompt number
    #[arg(long)]
    keep_count: bool,

    /// Do not strip output
    #[arg(long)]
    keep_output: bool,

    /// Types of outputs to drop, e.g. "error" or "stream:stderr"
    #[arg(long, num_args = 1.., value_name = "TYPE")]
    drop_output_type: Vec<OutputType>,

    /// Types of outputs to keep, e.g. "error" or "stream"; works without --keep-output
    #[arg(long, num_args = 1.., value_name = "TYPE")]
    keep_output_type: Vec<OutputType>,

    /// Keep the randomly generated cell ids instead of numbering cells sequentially
    #[arg(long)]
    keep_id: bool,

    /// Space separated list of extra keys to strip, e.g. "metadata.foo cell.metadata.bar"
    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        value_name = "KEYS"
    )]
    extra_keys: String,

    /// Space separated list of metadata keys to keep, e.g. "metadata.foo cell.metadata.bar"
    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        value_name = "KEYS"
    )]
    keep_metadata_keys: String,

    /// Remove cells whose source is empty or contains only whitespace
    #[arg(long)]
    drop_empty_cells: bool,

    /// Space separated list of cell tags; cells carrying any of them are removed
    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        value_name = "TAGS"
    )]
    drop_tagged_cells: String,

    /// Remove the output of cells with `init_cell: true` metadata
    #[arg(long)]
    strip_init_cells: bool,

    /// Attributes file to use with --install/--uninstall (default: .git/info/attributes)
    #[arg(long, value_name = "FILEPATH")]
    attributes: Option<PathBuf>,

    /// Use the global git config (default is the repository config)
    #[arg(long)]
    global: bool,

    /// Use the system git config (default is the repository config)
    #[arg(long)]
    system: bool,

    /// Strip files even if they do not end in .ipynb or .zpln
    #[arg(short, long)]
    force: bool,

    /// Keep outputs smaller than SIZE (bytes; K, M and G suffixes allowed)
    #[arg(long, default_value = "0", value_name = "SIZE", value_parser = parse_size)]
    max_size: i64,

    /// Notebook format
    #[arg(short, long, value_enum, default_value_t = Mode::Jupyter)]
    mode: Mode,

    /// Print stripped files to stdout
    #[arg(short, long)]
    textconv: bool,

    /// Force UNIX line endings in the output (default: the platform's line endings)
    #[arg(long)]
    unix_newlines: bool,

    /// Number of files to process in parallel (default: number of CPUs)
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,

    /// Files to strip; reads from stdin and writes to stdout if none are given
    files: Vec<PathBuf>,
}

/// nbstripout's `_parse_size`: an integer with an optional K/M/G suffix (SI units).
fn parse_size(s: &str) -> Result<i64, String> {
    let upper = s.trim().to_uppercase();
    let (digits, factor) = match upper.chars().last() {
        Some('K') => (&upper[..upper.len() - 1], 1_000),
        Some('M') => (&upper[..upper.len() - 1], 1_000_000),
        Some('G') => (&upper[..upper.len() - 1], 1_000_000_000),
        Some(c) if c.is_ascii_digit() => (upper.as_str(), 1),
        _ => {
            return Err(format!(
                "unknown size `{s}`; expected e.g. 500, 10K, 2M or 1G"
            ))
        }
    };
    digits
        .replace('_', "")
        .parse::<i64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .ok_or_else(|| format!("invalid size `{s}`"))
}

/// What to do with each stripped notebook.
#[derive(Debug, Clone, Copy)]
enum Action {
    InPlace,
    DryRun,
    Textconv,
}

struct Job<'a> {
    options: &'a Options,
    mode: Mode,
    crlf: bool,
    action: Action,
}

/// Result of processing one input. Output is buffered so that parallel workers
/// still print in command-line order.
#[derive(Default)]
struct Report {
    stdout: Vec<u8>,
    error: Option<String>,
    changed: bool,
}

impl Job<'_> {
    fn process(&self, input: &[u8], name: &str, path: Option<&Path>) -> Report {
        let mut report = Report::default();
        let stripped = match strip_notebook(input, self.mode, self.options) {
            Ok(stripped) => stripped,
            Err(Error::NotJson(msg)) => {
                report.error = Some(match path {
                    Some(_) => format!("No valid notebook detected in '{name}': {msg}"),
                    None => format!("No valid notebook detected on stdin: {msg}"),
                });
                return report;
            }
            Err(Error::Invalid(msg)) => {
                report.error = Some(format!("Could not strip '{name}': {msg}"));
                return report;
            }
        };
        report.changed = stripped.changed;
        match (self.action, path) {
            (Action::DryRun, _) => {
                if stripped.changed {
                    report.stdout = format!("Dry run: would have stripped {name}\n").into_bytes();
                }
            }
            (Action::InPlace, Some(path)) => {
                // Like nbstripout, leave files whose content would not change alone.
                if stripped.changed {
                    if let Err(e) = write_atomic(path, &stripped.render(self.crlf)) {
                        report.error = Some(format!("Could not write '{name}': {e}"));
                    }
                }
            }
            (Action::Textconv, _) | (Action::InPlace, None) => {
                report.stdout = stripped.render(self.crlf);
            }
        }
        report
    }

    fn process_file(&self, path: &Path) -> Report {
        let name = path.display().to_string();
        match fs::read(path) {
            Ok(input) => self.process(&input, &name, Some(path)),
            Err(e) => Report {
                error: Some(match e.kind() {
                    io::ErrorKind::NotFound => format!("Could not strip '{name}': file not found"),
                    _ => format!("Could not strip '{name}': {e}"),
                }),
                ..Report::default()
            },
        }
    }
}

fn write_stdout(data: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    match stdout.write_all(data).and_then(|()| stdout.flush()) {
        // The reader went away (e.g. `| head`); nothing left to do.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

fn build_options(cli: &Cli, scope: Option<Scope>) -> Options {
    let settings = git::filter_settings(scope);
    let mut extra: Vec<String> = DEFAULT_EXTRA_KEYS.iter().map(|k| k.to_string()).collect();
    extra.extend(settings.extra_keys);
    extra.extend(py::split_whitespace(&cli.extra_keys).map(str::to_owned));
    let mut keep = settings.keep_metadata_keys;
    keep.extend(py::split_whitespace(&cli.keep_metadata_keys).map(str::to_owned));
    extra.retain(|key| !keep.contains(key));

    let (extra_keys, invalid) = ExtraKeys::parse(extra.iter().map(String::as_str));
    for key in invalid {
        eprintln!("Ignoring invalid extra key `{key}`");
    }
    Options {
        keep_output: cli.keep_output.then_some(true),
        keep_count: cli.keep_count,
        keep_id: cli.keep_id,
        extra_keys,
        drop_empty_cells: cli.drop_empty_cells,
        drop_tagged_cells: py::split_whitespace(&cli.drop_tagged_cells)
            .map(str::to_owned)
            .collect(),
        strip_init_cells: cli.strip_init_cells,
        drop_output_types: cli.drop_output_type.clone(),
        keep_output_types: cli.keep_output_type.clone(),
        max_size: cli.max_size,
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let scope = if cli.system {
        Scope::System
    } else if cli.global {
        Scope::Global
    } else {
        Scope::Local
    };
    let exit = |ok: bool| {
        if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    };

    if cli.install {
        return git::install(scope, cli.attributes.as_deref(), !cli.no_process)
            .map(|()| ExitCode::SUCCESS);
    }
    if cli.uninstall {
        return git::uninstall(scope, cli.attributes.as_deref()).map(|()| ExitCode::SUCCESS);
    }
    if cli.is_installed || cli.status {
        return Ok(exit(git::status(scope, cli.status)));
    }

    let config_scope = (scope != Scope::Local).then_some(scope);
    let options = build_options(&cli, config_scope);
    let crlf = cfg!(windows) && !cli.unix_newlines;

    if cli.filter_process {
        let served =
            filter_process::serve(io::stdin().lock(), io::stdout().lock(), |path, content| {
                let mode = if path.ends_with(".zpln") {
                    Mode::Zeppelin
                } else {
                    cli.mode
                };
                strip_notebook(content, mode, &options)
                    .map(|stripped| stripped.render(crlf))
                    .map_err(|e| e.to_string())
            });
        return served
            .map(|()| ExitCode::SUCCESS)
            .map_err(|e| format!("nbripout: filter process failed: {e}"));
    }

    let action = if cli.dry_run || cli.verify {
        Action::DryRun
    } else if cli.textconv {
        Action::Textconv
    } else {
        Action::InPlace
    };
    let job = Job {
        options: &options,
        mode: cli.mode,
        crlf,
        action,
    };

    let reports: Vec<Report> = if cli.files.is_empty() {
        let mut stdin = io::stdin().lock();
        if stdin.is_terminal() {
            return Err("nbripout: no files given and stdin is a terminal (see --help)".to_owned());
        }
        let mut input = Vec::new();
        stdin
            .read_to_end(&mut input)
            .map_err(|e| format!("nbripout: cannot read stdin: {e}"))?;
        vec![job.process(&input, "input from stdin", None)]
    } else {
        let files: Vec<&PathBuf> = cli
            .files
            .iter()
            .filter(|f| {
                let name = f.to_string_lossy();
                cli.force || name.ends_with(".ipynb") || name.ends_with(".zpln")
            })
            .collect();
        if files.len() > 1 {
            if let Some(jobs) = cli.jobs {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(jobs)
                    .build_global()
                    .map_err(|e| format!("nbripout: {e}"))?;
            }
            files.par_iter().map(|f| job.process_file(f)).collect()
        } else {
            files.iter().map(|f| job.process_file(f)).collect()
        }
    };

    let mut failed = false;
    let mut changed = false;
    for report in reports {
        if let Err(e) = write_stdout(&report.stdout) {
            return Err(format!("nbripout: cannot write to stdout: {e}"));
        }
        if let Some(error) = report.error {
            eprintln!("{error}");
            failed = true;
        }
        changed |= report.changed;
    }
    Ok(exit(!(failed || (cli.verify && changed))))
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
