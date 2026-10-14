//! Git integration: filter configuration, installation and status.
//!
//! nbripout registers itself under nbstripout's filter name (`filter=nbstripout`,
//! `diff=ipynb`), so repositories that already carry nbstripout's
//! `.gitattributes` work unchanged, and teammates can mix both tools.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::fsutil::write_atomic;
use crate::py;

const ATTRIBUTE_PATTERNS: [&str; 3] = ["*.ipynb filter", "*.zpln filter", "*.ipynb diff"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Local,
    Global,
    System,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::Local => "--local",
            Scope::Global => "--global",
            Scope::System => "--system",
        }
    }
}

#[derive(Debug)]
enum GitError {
    NotInstalled,
    Failed(String),
}

/// Run git with a closed stdin: nbripout's own stdin carries notebook data (or the
/// filter protocol) and must never be consumed by a child process.
fn git(args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => GitError::NotInstalled,
            _ => GitError::Failed(e.to_string()),
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(GitError::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

/// Key lists configured via `filter.nbstripout.extrakeys` and
/// `filter.nbstripout.keepmetadatakeys`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilterSettings {
    pub extra_keys: Vec<String>,
    pub keep_metadata_keys: Vec<String>,
}

/// Read the filter settings with a single `git config` call. `None` reads the
/// effective configuration of all scopes, like nbstripout does by default.
pub fn filter_settings(scope: Option<Scope>) -> FilterSettings {
    let mut args = vec!["config"];
    args.extend(scope.map(Scope::flag));
    args.extend([
        "-z",
        "--get-regexp",
        r"^filter\.nbstripout\.(extrakeys|keepmetadatakeys)$",
    ]);
    let Ok(output) = git(&args) else {
        return FilterSettings::default();
    };
    let mut extra_keys = "";
    let mut keep_metadata_keys = "";
    // Entries are `key\nvalue\0`; for multi-valued keys the last value wins, as
    // with `git config <key>`.
    for entry in output.split('\0').filter(|e| !e.is_empty()) {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        match key {
            "filter.nbstripout.extrakeys" => extra_keys = value,
            "filter.nbstripout.keepmetadatakeys" => keep_metadata_keys = value,
            _ => {}
        }
    }
    let words = |s: &str| py::split_whitespace(s).map(str::to_owned).collect();
    FilterSettings {
        extra_keys: words(extra_keys),
        keep_metadata_keys: words(keep_metadata_keys),
    }
}

/// The command git should run to invoke this executable, quoted for git's shell.
fn self_command() -> Result<String, String> {
    let exe = invoked_executable()
        .or_else(|| std::env::current_exe().ok())
        .ok_or("cannot determine the path of the nbripout executable")?;
    // Backslashes would be eaten by the POSIX shell git uses, also on Windows.
    Ok(format!("\"{}\"", exe.to_string_lossy().replace('\\', "/")))
}

/// The path this executable was started from, without resolving symlinks, so that
/// installs via package managers (`/opt/homebrew/bin/nbripout`, `~/.cargo/bin`)
/// keep working after upgrades. Only used if it really points at this executable.
fn invoked_executable() -> Option<PathBuf> {
    let current = fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    let arg0 = PathBuf::from(std::env::args_os().next()?);
    let candidate = if arg0.components().count() > 1 {
        arg0
    } else {
        let names = [arg0.clone(), arg0.with_extension("exe")];
        std::env::split_paths(&std::env::var_os("PATH")?)
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|path| path.is_file())?
    };
    let candidate = std::path::absolute(candidate).ok()?;
    (fs::canonicalize(&candidate).ok()? == current).then_some(candidate)
}

fn expand_tilde(path: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

fn attributes_file(scope: Scope, explicit: Option<&Path>) -> Result<PathBuf, GitError> {
    let path = match (explicit, scope) {
        (Some(path), _) => expand_tilde(path),
        // `--git-path` resolves the common directory, so linked worktrees write to
        // the attributes file git actually reads.
        (None, Scope::Local) => {
            PathBuf::from(git(&["rev-parse", "--git-path", "info/attributes"])?.trim())
        }
        (None, Scope::Global) => match git(&["var", "GIT_ATTR_GLOBAL"]) {
            Ok(path) => PathBuf::from(path.trim()),
            Err(_) => {
                let config = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| expand_tilde(Path::new("~/.config")));
                config.join("git").join("attributes")
            }
        },
        (None, Scope::System) => PathBuf::from(git(&["var", "GIT_ATTR_SYSTEM"])?.trim()),
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| GitError::Failed(e.to_string()))?;
    }
    Ok(path)
}

fn read_optional(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        result => result,
    }
}

/// Configure the git filter and attributes (nbstripout's `--install`).
pub fn install(scope: Scope, attributes: Option<&Path>, process: bool) -> Result<(), String> {
    let failed = |e: GitError| match e {
        GitError::NotInstalled => "Installation failed: git is not on path!".to_owned(),
        GitError::Failed(msg) => format!("Installation failed: not a git repository! ({msg})"),
    };
    let exe = self_command().map_err(|e| format!("Installation failed: {e}"))?;
    let flag = scope.flag();
    let set = |key: &str, value: &str| git(&["config", flag, key, value]).map(drop);
    set("filter.nbstripout.clean", &exe).map_err(failed)?;
    set("filter.nbstripout.smudge", "cat").map_err(failed)?;
    set("filter.nbstripout.required", "true").map_err(failed)?;
    set("diff.ipynb.textconv", &format!("{exe} -t")).map_err(failed)?;
    if process {
        set(
            "filter.nbstripout.process",
            &format!("{exe} --filter-process"),
        )
        .map_err(failed)?;
    } else {
        // Re-installing without --process must not leave a stale process filter,
        // which git would prefer over `clean`.
        let _ = git(&["config", flag, "--unset-all", "filter.nbstripout.process"]);
    }

    let path = attributes_file(scope, attributes).map_err(failed)?;
    let existing = read_optional(&path).map_err(|e| {
        format!(
            "Installation failed: could not read {}: {e}",
            path.display()
        )
    })?;
    let [has_filter, has_zeppelin, has_diff] = ATTRIBUTE_PATTERNS.map(|p| existing.contains(p));
    if has_filter && has_diff {
        return Ok(());
    }
    let mut addition = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        addition.push('\n');
    }
    for (present, line) in [
        (has_filter, "*.ipynb filter=nbstripout\n"),
        (has_zeppelin, "*.zpln filter=nbstripout\n"),
        (has_diff, "*.ipynb diff=ipynb\n"),
    ] {
        if !present {
            addition.push_str(line);
        }
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(addition.as_bytes()))
        .map_err(|e| {
            let hint = if scope == Scope::System {
                " Did you forget to sudo?"
            } else {
                ""
            };
            format!(
                "Installation failed: could not write to {}: {e}.{hint}",
                path.display()
            )
        })
}

/// Remove the git filter and attributes (nbstripout's `--uninstall`).
pub fn uninstall(scope: Scope, attributes: Option<&Path>) -> Result<(), String> {
    let failed = |e: GitError| match e {
        GitError::NotInstalled => "Uninstall failed: git is not on path!".to_owned(),
        GitError::Failed(msg) => format!("Uninstall failed: not a git repository! ({msg})"),
    };
    let flag = scope.flag();
    for key in ["clean", "smudge", "required", "process"] {
        let key = format!("filter.nbstripout.{key}");
        if let Err(GitError::NotInstalled) = git(&["config", flag, "--unset-all", &key]) {
            return Err(failed(GitError::NotInstalled));
        }
    }
    let _ = git(&["config", flag, "--remove-section", "diff.ipynb"]);

    let path = attributes_file(scope, attributes).map_err(failed)?;
    let existing = read_optional(&path)
        .map_err(|e| format!("Uninstall failed: could not read {}: {e}", path.display()))?;
    let kept: String = existing
        .split_inclusive('\n')
        .filter(|line| !ATTRIBUTE_PATTERNS.iter().any(|p| line.starts_with(p)))
        .collect();
    if kept != existing {
        write_atomic(&path, kept.as_bytes())
            .map_err(|e| format!("Uninstall failed: could not write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether the filter is installed; prints a summary if `verbose`
/// (nbstripout's `--is-installed` / `--status`).
pub fn status(scope: Scope, verbose: bool) -> bool {
    let location = match scope {
        Scope::System => "system-wide".to_owned(),
        Scope::Global => "globally".to_owned(),
        Scope::Local => match git(&["rev-parse", "--show-toplevel"]) {
            Ok(top) => format!("in repository '{}'", top.trim()),
            Err(GitError::NotInstalled) => {
                eprintln!("Cannot determine status: git is not on path!");
                return false;
            }
            Err(GitError::Failed(msg)) => {
                if verbose {
                    eprintln!("Cannot determine status: {msg}");
                }
                return false;
            }
        },
    };
    let flag = scope.flag();
    let get = |key: &str| {
        git(&["config", flag, key])
            .ok()
            .map(|v| v.trim().to_owned())
    };
    let not_installed = || {
        if verbose {
            println!("nbstripout filter is not installed {location}");
        }
        false
    };
    let (Some(clean), Some(smudge), Some(diff)) = (
        get("filter.nbstripout.clean"),
        get("filter.nbstripout.smudge"),
        get("diff.ipynb.textconv"),
    ) else {
        return not_installed();
    };

    let (attributes, diff_attributes) = if scope == Scope::Local {
        let check = |attr: &str| {
            git(&["check-attr", attr, "--", "*.ipynb"])
                .map(|out| out.trim().to_owned())
                .unwrap_or_default()
        };
        (check("filter"), check("diff"))
    } else {
        let lines = attributes_file(scope, None)
            .ok()
            .and_then(|path| read_optional(&path).ok())
            .unwrap_or_default();
        let pick = |word: &str| {
            lines
                .lines()
                .filter(|line| line.contains(word))
                .collect::<Vec<_>>()
                .join("\n")
        };
        (pick("filter"), pick("diff"))
    };
    if attributes.is_empty() || attributes.ends_with("unspecified") {
        return not_installed();
    }

    if verbose {
        println!("nbstripout filter is installed {location}");
        println!("\nFilter:");
        println!("  clean = {clean}");
        println!("  smudge = {smudge}");
        if let Some(process) = get("filter.nbstripout.process") {
            println!("  process = {process}");
        }
        println!("  diff= {diff}");
        println!(
            "  extrakeys= {}",
            get("filter.nbstripout.extrakeys").unwrap_or_default()
        );
        println!("\nAttributes:\n  {attributes}");
        println!("\nDiff Attributes:\n  {diff_attributes}");
    }
    true
}
