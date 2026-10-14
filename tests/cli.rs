use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_nbripout");

const NOTEBOOK: &str = r##"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": 7,
   "id": "first",
   "metadata": {"collapsed": true, "tags": ["t1"]},
   "outputs": [{"name": "stdout", "output_type": "stream", "text": ["secret output\n"]}],
   "source": ["print('hi')"]
  },
  {
   "cell_type": "markdown",
   "id": "second",
   "metadata": {"tags": ["t2"]},
   "source": ["# Title"]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "third",
   "metadata": {},
   "outputs": [],
   "source": ["x = 1"]
  }
 ],
 "metadata": {"kernelspec": {"display_name": "Python 3", "name": "python3"}},
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

/// nbformat's rendering of NOTEBOOK after stripping with default options.
const STRIPPED: &str = r##"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "0",
   "metadata": {
    "tags": [
     "t1"
    ]
   },
   "outputs": [],
   "source": [
    "print('hi')"
   ]
  },
  {
   "cell_type": "markdown",
   "id": "1",
   "metadata": {
    "tags": [
     "t2"
    ]
   },
   "source": [
    "# Title"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "2",
   "metadata": {},
   "outputs": [],
   "source": [
    "x = 1"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "name": "python3"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

fn nbripout(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = isolated(Command::new(BIN))
        .args(["--unix-newlines"])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin.unwrap_or_default().as_bytes())
        .unwrap();
    drop(pipe);
    child.wait_with_output().unwrap()
}

/// Keep the user's git configuration out of the tests.
fn isolated(mut cmd: Command) -> Command {
    let no_config = std::env::temp_dir().join("nbripout-tests-no-gitconfig");
    cmd.env("GIT_CONFIG_GLOBAL", no_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let out = isolated(Command::new("git"))
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    out
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

#[test]
fn strips_files_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.ipynb", NOTEBOOK);
    let b = write(dir.path(), "b.ipynb", NOTEBOOK);
    let out = nbripout(dir.path(), &["a.ipynb", "b.ipynb"], None);
    assert!(out.status.success());
    assert_eq!(fs::read_to_string(a).unwrap(), STRIPPED);
    assert_eq!(fs::read_to_string(b).unwrap(), STRIPPED);
}

#[test]
fn pipes_stdin_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let out = nbripout(dir.path(), &[], Some(NOTEBOOK));
    assert!(out.status.success());
    assert_eq!(stdout(&out), STRIPPED);
}

#[test]
fn leaves_clean_files_untouched() {
    let dir = tempfile::tempdir().unwrap();
    // Already stripped, but not in nbformat's layout: nbstripout leaves it alone too.
    let compact: String = STRIPPED.split_whitespace().collect::<Vec<_>>().join(" ");
    let path = write(dir.path(), "clean.ipynb", &compact);
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(nbripout(dir.path(), &["clean.ipynb"], None)
        .status
        .success());
    assert_eq!(fs::read_to_string(&path).unwrap(), compact);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
}

#[test]
fn never_destroys_a_notebook_it_cannot_write() {
    // nbstripout truncates this file to 0 bytes: the lone surrogate only fails
    // once it has already started rewriting the file.
    let dir = tempfile::tempdir().unwrap();
    let broken = r#"{"cells": [{"cell_type": "markdown", "id": "a", "metadata": {}, "source": ["\ud800 keep me"]}],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#;
    let bad = write(dir.path(), "bad.ipynb", broken);
    let good = write(dir.path(), "good.ipynb", NOTEBOOK);
    let out = nbripout(dir.path(), &["bad.ipynb", "good.ipynb"], None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_to_string(bad).unwrap(), broken);
    assert_eq!(
        fs::read_to_string(good).unwrap(),
        STRIPPED,
        "other files are still processed"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No valid notebook detected in 'bad.ipynb'")
    );
    let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(leftovers.len(), 2, "no temporary files are left behind");
}

#[test]
fn reports_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let out = nbripout(dir.path(), &["missing.ipynb"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("file not found"));
}

#[cfg(unix)]
#[test]
fn keeps_symlinks_and_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let target = write(dir.path(), "target.ipynb", NOTEBOOK);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("target.ipynb", dir.path().join("link.ipynb")).unwrap();
    assert!(nbripout(dir.path(), &["link.ipynb"], None).status.success());
    assert!(fs::symlink_metadata(dir.path().join("link.ipynb"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), STRIPPED);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn dry_run_and_verify() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.ipynb", NOTEBOOK);
    write(dir.path(), "clean.ipynb", STRIPPED);

    let out = nbripout(dir.path(), &["--dry-run", "a.ipynb", "clean.ipynb"], None);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "Dry run: would have stripped a.ipynb\n");
    assert_eq!(fs::read_to_string(&path).unwrap(), NOTEBOOK);

    let out = nbripout(dir.path(), &["--verify", "a.ipynb"], None);
    assert_eq!(out.status.code(), Some(1));
    let out = nbripout(dir.path(), &["--verify", "clean.ipynb"], None);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn skips_other_extensions_unless_forced() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "notebook.json", NOTEBOOK);
    assert!(nbripout(dir.path(), &["notebook.json"], None)
        .status
        .success());
    assert_eq!(fs::read_to_string(&path).unwrap(), NOTEBOOK);
    assert!(nbripout(dir.path(), &["-f", "notebook.json"], None)
        .status
        .success());
    assert_eq!(fs::read_to_string(&path).unwrap(), STRIPPED);
}

#[test]
fn drops_cells_with_any_of_several_tags() {
    // nbstripout 0.9.1 only honors the last tag of the list.
    let dir = tempfile::tempdir().unwrap();
    let out = nbripout(
        dir.path(),
        &["--drop-tagged-cells", "t1 t2"],
        Some(NOTEBOOK),
    );
    assert!(out.status.success());
    let text = stdout(&out);
    assert_eq!(text.matches("\"cell_type\"").count(), 1);
    assert!(text.contains("x = 1"));
}

#[test]
fn zeppelin_notebooks_are_detected_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let zpln = r#"{"paragraphs": [{"text": "%md hi", "results": {"code": "SUCCESS", "msg": []}}], "name": "zé"}"#;
    let out = nbripout(dir.path(), &[], Some(zpln));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "{\n  \"paragraphs\": [\n    {\n      \"text\": \"%md hi\",\n      \"results\": {}\n    }\n  ],\n  \"name\": \"z\\u00e9\"\n}\n"
    );
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git_ok(dir.path(), &["init", "-q", "."]);
    // On Windows the filter writes CRLF like nbstripout; normalize as Git for
    // Windows does by default.
    git_ok(dir.path(), &["config", "core.autocrlf", "input"]);
    dir
}

#[test]
fn git_clean_filter_and_textconv() {
    let repo = git_repo();
    let dir = repo.path();
    let out = nbripout(dir, &["--install"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(nbripout(dir, &["--is-installed"], None).status.success());
    let attributes = fs::read_to_string(dir.join(".git/info/attributes")).unwrap();
    assert_eq!(
        attributes,
        "*.ipynb filter=nbstripout\n*.zpln filter=nbstripout\n*.ipynb diff=ipynb\n"
    );
    let status = nbripout(dir, &["--status"], None);
    assert!(stdout(&status).contains("*.ipynb: filter: nbstripout"));

    write(dir, "nb.ipynb", NOTEBOOK);
    git_ok(dir, &["add", "nb.ipynb"]);
    assert_eq!(git_ok(dir, &["show", ":nb.ipynb"]), STRIPPED);
    assert_eq!(
        fs::read_to_string(dir.join("nb.ipynb")).unwrap(),
        NOTEBOOK,
        "working copy keeps outputs"
    );
    git_ok(dir, &["commit", "-q", "-m", "init"]);
    assert_eq!(
        git_ok(dir, &["status", "--porcelain"]),
        "",
        "outputs alone are not a change"
    );

    // Installing twice does not duplicate attributes.
    assert!(nbripout(dir, &["--install", "--no-process"], None)
        .status
        .success());
    assert_eq!(
        fs::read_to_string(dir.join(".git/info/attributes")).unwrap(),
        attributes
    );

    assert!(nbripout(dir, &["--uninstall"], None).status.success());
    assert_eq!(
        nbripout(dir, &["--is-installed"], None).status.code(),
        Some(1)
    );
    assert_eq!(
        fs::read_to_string(dir.join(".git/info/attributes")).unwrap(),
        ""
    );
}

#[test]
fn git_filter_process() {
    let repo = git_repo();
    let dir = repo.path();
    let out = nbripout(dir, &["--install"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(git_ok(dir, &["config", "filter.nbstripout.process"]).contains("--filter-process"));

    for i in 0..20 {
        write(dir, &format!("nb{i}.ipynb"), NOTEBOOK);
    }
    write(
        dir,
        "z.zpln",
        r#"{"paragraphs": [{"text": "x", "results": {"msg": ["out"]}}], "name": "z"}"#,
    );
    git_ok(dir, &["add", "."]);
    for i in 0..20 {
        assert_eq!(git_ok(dir, &["show", &format!(":nb{i}.ipynb")]), STRIPPED);
    }
    assert!(git_ok(dir, &["show", ":z.zpln"]).contains("\"results\": {}"));

    // Smudge is the identity: a checkout restores the stripped notebook.
    git_ok(dir, &["commit", "-q", "-m", "init"]);
    fs::remove_file(dir.join("nb0.ipynb")).unwrap();
    git_ok(dir, &["checkout", "--", "nb0.ipynb"]);
    assert_eq!(fs::read_to_string(dir.join("nb0.ipynb")).unwrap(), STRIPPED);

    // A broken notebook fails the (required) filter instead of being committed.
    write(dir, "broken.ipynb", "{ not json");
    let out = git(dir, &["add", "broken.ipynb"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("broken.ipynb"));

    // --no-process removes the process filter again.
    assert!(nbripout(dir, &["--install", "--no-process"], None)
        .status
        .success());
    assert!(!git(dir, &["config", "filter.nbstripout.process"])
        .status
        .success());
}

#[test]
fn reads_extra_keys_from_git_config() {
    let repo = git_repo();
    let dir = repo.path();
    git_ok(
        dir,
        &[
            "config",
            "filter.nbstripout.extrakeys",
            "metadata.kernelspec",
        ],
    );
    let out = nbripout(dir, &[], Some(NOTEBOOK));
    assert!(!stdout(&out).contains("kernelspec"));
    git_ok(
        dir,
        &[
            "config",
            "filter.nbstripout.keepmetadatakeys",
            "metadata.kernelspec",
        ],
    );
    let out = nbripout(dir, &[], Some(NOTEBOOK));
    assert!(stdout(&out).contains("kernelspec"));
}
