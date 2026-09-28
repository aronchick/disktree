//! disktree-du against GNU du itself: the same fixture, the same arguments,
//! and the output, the diagnostics and the exit status compared byte for
//! byte. Each case runs three times: without an index, with the index the
//! first run left, and with `--max-age` answering from it.
//!
//! GNU du is found as `gdu` (Homebrew's coreutils) or as a `du` that says it
//! is GNU. Without one the comparison is skipped, loudly.
#![cfg(unix)]

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gnu_du() -> Option<PathBuf> {
    for name in ["gdu", "du"] {
        let Ok(output) = Command::new(name).arg("--version").output() else {
            continue;
        };
        if String::from_utf8_lossy(&output.stdout).contains("GNU coreutils") {
            let path = Command::new("sh")
                .args(["-c", &format!("command -v {name}")])
                .output()
                .ok()?;
            let path = String::from_utf8_lossy(&path.stdout).trim().to_owned();
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, vec![b'x'; bytes]).expect("write");
}

/// A tree with each thing du treats specially.
fn fixture(root: &Path) {
    write(&root.join("a/small"), 10);
    write(&root.join("a/four-k"), 4096);
    write(&root.join("a/b/c/deep"), 70_000);
    write(&root.join("a/b/mid"), 5000);
    write(&root.join("big"), 3 * 1024 * 1024 + 7);
    write(&root.join("with space/x y"), 100);
    write(&root.join("quote's/f"), 1);
    write(&root.join("keep/logs/app.log"), 12_345);
    write(&root.join("keep/logs/old.log.gz"), 999);
    std::fs::create_dir_all(root.join("empty")).expect("mkdir");

    // A sparse file: allocation and apparent size disagree.
    let sparse = std::fs::File::create(root.join("sparse")).expect("sparse");
    sparse.set_len(10 * 1024 * 1024).expect("set_len");

    // Hard links: within one directory, across directories, and so across
    // operands.
    write(&root.join("links/original"), 50_000);
    std::fs::hard_link(root.join("links/original"), root.join("links/second"))
        .expect("link");
    std::fs::create_dir_all(root.join("elsewhere")).expect("mkdir");
    std::fs::hard_link(
        root.join("links/original"),
        root.join("elsewhere/third"),
    )
    .expect("link");

    // Symbolic links: to a file, to a directory, to nothing, and a loop.
    symlink("../big", root.join("a/to-big")).expect("symlink");
    symlink("../a/b", root.join("keep/to-b")).expect("symlink");
    symlink("nowhere", root.join("a/dangling")).expect("symlink");
    symlink("..", root.join("a/b/c/up")).expect("symlink");

    // More than 10,000 entries: fts sorts such a directory by inode.
    let many = root.join("many");
    std::fs::create_dir_all(&many).expect("mkdir");
    for n in 0..10_050 {
        std::fs::write(many.join(format!("f{n:05}")), b"").expect("write");
    }
    // A subdirectory among them, so the order of directories shows.
    write(&many.join("zz-sub/inner"), 3000);
    write(&many.join("aa-sub/inner"), 3000);
}

struct Harness {
    ours: PathBuf,
    gnu: PathBuf,
    cwd: PathBuf,
    index: PathBuf,
}

fn run(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &Path)],
) -> Output {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env_remove("DU_BLOCK_SIZE")
        .env_remove("BLOCK_SIZE")
        .env_remove("BLOCKSIZE")
        .env_remove("POSIXLY_CORRECT")
        .env_remove("TIME_STYLE")
        .env_remove("DISKTREE_DU_MAX_AGE")
        .env_remove("DISKTREE_DU_FRESH")
        .env_remove("DISKTREE_DU_JSON");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("run")
}

fn show(output: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}--- stderr\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Standard error with the binary's full path, which `getopt` and the
/// "Try ... --help" line repeat, replaced by a placeholder.
fn stderr(output: &Output, program: &Path) -> Vec<u8> {
    String::from_utf8_lossy(&output.stderr)
        .replace(&program.display().to_string(), "DU")
        .into_bytes()
}

impl Harness {
    fn check(&self, args: &[&str]) {
        let gnu = run(&self.gnu, &self.cwd, args, &[]);
        let index = self.index.as_path();
        let one = Path::new("1");
        let hour = Path::new("1h");
        // Through the environment, as behind a `du` symlink: appended
        // flags would land after a `--`.
        let passes: [(&str, Vec<(&str, &Path)>); 3] = [
            (
                "cold",
                vec![
                    ("DISKTREE_DU_INDEX_DIR", index),
                    ("DISKTREE_DU_FRESH", one),
                ],
            ),
            ("indexed", vec![("DISKTREE_DU_INDEX_DIR", index)]),
            (
                "--max-age",
                vec![
                    ("DISKTREE_DU_INDEX_DIR", index),
                    ("DISKTREE_DU_MAX_AGE", hour),
                ],
            ),
        ];
        for (pass, env) in passes {
            let ours = run(&self.ours, &self.cwd, args, &env);
            assert!(
                ours.stdout == gnu.stdout
                    && stderr(&ours, &self.ours) == stderr(&gnu, &self.gnu)
                    && ours.status.code() == gnu.status.code(),
                "du {} ({pass})\n=== GNU\n{}\n=== disktree\n{}",
                args.join(" "),
                show(&gnu),
                show(&ours)
            );
        }
    }
}

#[test]
fn matches_gnu_du() {
    let Some(gnu) = gnu_du() else {
        eprintln!("SKIPPED: no GNU du (install coreutils for gdu)");
        return;
    };
    let temp = tempfile::TempDir::new().expect("tempdir");
    // Canonical, so both tools print the same path when given it.
    let base = temp.path().canonicalize().expect("canonical");
    let tree = base.join("tree");
    fixture(&tree);
    let locked = tree.join("locked");
    write(&locked.join("secret"), 100);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("chmod");

    // Named as GNU's binary is, so both prefix diagnostics the same way.
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let ours = bin.join(gnu.file_name().expect("name"));
    symlink(env!("CARGO_BIN_EXE_disktree-du"), &ours).expect("symlink");
    let harness = Harness {
        ours,
        gnu,
        cwd: tree,
        index: base.join("index"),
    };

    let exclude_file = base.join("excludes");
    std::fs::write(&exclude_file, "*.gz\nmid\n").expect("write");
    let exclude_from = format!("--exclude-from={}", exclude_file.display());
    let names0 = base.join("names0");
    std::fs::write(&names0, b"a\0big\0\0links\0").expect("write");
    let files0 = format!("--files0-from={}", names0.display());

    let cases: &[&[&str]] = &[
        &[],
        &["."],
        &["-a"],
        &["-a", "."],
        &["-s"],
        &["-sh", "a", "big", "sparse", "links", "many"],
        &["-ah"],
        &["--si", "-a"],
        &["-ab"],
        &["-a", "--apparent-size"],
        &["-ak"],
        &["-am"],
        &["-a", "-B1K"],
        &["-a", "-BK"],
        &["-a", "-BKB"],
        &["-a", "-B", "MiB"],
        &["-a", "--block-size=human-readable"],
        &["-c", "a", "links", "elsewhere"],
        &["-sc", "links", "elsewhere", "links"],
        &["-al", "links", "elsewhere"],
        &["-d", "1"],
        &["--max-depth=0", "-a"],
        &["-d", "2", "-a", "a"],
        &["-S"],
        &["-Sa"],
        &["-S", "--apparent-size", "-a"],
        &["--inodes"],
        &["--inodes", "-a", "-h"],
        &["-a", "--exclude=*.log"],
        &["-a", "--exclude=b"],
        &["-a", "--exclude=a/b/c"],
        &["-a", &exclude_from],
        &["-a", "-t", "4K"],
        &["-a", "-t", "-4K"],
        &["-a", "-0"],
        &["-aL", "a"],
        &["-aL", "keep"],
        &["-aH", "keep/to-b"],
        &["-a", "keep/to-b"],
        &["-aD", "a/to-big"],
        &["-s", "a/dangling"],
        &["-sL", "a/dangling"],
        &["-aLl", "a"],
        &["-ax"],
        &["-a", "--time"],
        &["-a", "--time=ctime", "--time-style=full-iso"],
        &["-s", "--time", "--time-style=+%Y %j %N"],
        &["-a", "--time-style=iso", "--time"],
        &["-s", "missing", "a", "", "big"],
        &["-s", "a//"],
        &["-s", &files0],
        &["-s", "locked"],
        &["-a", "locked"],
        &["-as"],
        &["-s", "-d", "1"],
        &["-s", "-d", "0"],
        &["-d", "x"],
        &["-d", "-1"],
        &["-B", "0"],
        &["-t", "1x"],
        &["-t", "-0"],
        &["--s"],
        &["--foo", "-z"],
        &["--all=1"],
        &["--time=x"],
        &["--time-style=bogus", "--time"],
        &["--files0-from=/nonexistent"],
        &["-X", "/nonexistent"],
        &["-s", "--files0-from=/dev/null", "a"],
        &["--max-d=1", "--sum"],
        &["--", "-a"],
        &["a", "-s"],
    ];
    for case in cases {
        harness.check(case);
    }

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
}

#[test]
fn json_describes_what_du_prints() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path();
    write(&root.join("proj/Cargo.toml"), 10);
    write(&root.join("proj/target/debug/app"), 50_000);
    write(&root.join("proj/src/main.rs"), 100);
    let output = run(
        Path::new(env!("CARGO_BIN_EXE_disktree-du")),
        root,
        &["--json", "-d", "1", "proj"],
        &[("DISKTREE_DU_INDEX_DIR", &root.join("index"))],
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}", show(&output));
    assert!(text.contains("\"source\": \"walk\""), "{text}");
    assert!(
        text.contains("\"reclaim\": \"build output\"")
            && text.contains("cargo clean --manifest-path proj/Cargo.toml"),
        "{text}"
    );
}

#[test]
fn max_age_answers_from_the_index_and_says_so() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path();
    write(&root.join("d/f"), 10_000);
    let index = root.join("index");
    let du = Path::new(env!("CARGO_BIN_EXE_disktree-du"));
    let env = [("DISKTREE_DU_INDEX_DIR", index.as_path())];
    let first = run(du, root, &["-s", "d"], &env);
    // A file grows; its directory's timestamps do not move.
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("d/f"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, &vec![0; 100_000]))
        .expect("grow");
    let trusted = run(du, root, &["-s", "--max-age=1h", "--json", "d"], &env);
    let text = String::from_utf8_lossy(&trusted.stdout);
    assert!(text.contains("\"source\": \"index\""), "{text}");
    let exact = run(du, root, &["-s", "d"], &env);
    assert_ne!(
        first.stdout, exact.stdout,
        "without --max-age the growth is seen"
    );
}
