//! `du`, answered by disktree: GNU du's command line and output, byte for
//! byte, from a walk that uses every core and remembers what it saw.
//!
//! Installed as `disktree-du`, and meant to be linked as `du` somewhere
//! early on `PATH` so that whatever already runs `du` gets it.

mod args;
mod exclude;
mod num;
mod quote;

#[cfg(target_os = "macos")]
mod bulk;
#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(unix)]
mod index;
#[cfg(unix)]
mod output;
#[cfg(unix)]
mod refresh;
#[cfg(unix)]
mod report;
#[cfg(unix)]
mod walk;

use std::process::ExitCode;

/// `strerror` for an I/O error, without Rust's " (os error N)" suffix: du
/// prints the C library's text.
pub fn strerror(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

#[cfg(unix)]
pub fn errno_text(errno: rustix::io::Errno) -> String {
    strerror(&std::io::Error::from_raw_os_error(errno.raw_os_error()))
}

fn main() -> ExitCode {
    let mut argv = std::env::args_os();
    let argv0 = argv
        .next()
        .map(std::ffi::OsString::into_encoded_bytes)
        .unwrap_or_default();
    let program = args::Program::new(&argv0);
    let rest: Vec<_> = argv.collect();
    match args::parse(&program, &rest) {
        args::Parsed::Exit(code) => {
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        args::Parsed::Run(options) => run(&program.short, &options),
    }
}

/// How an operand's numbers were arrived at.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A walk of the whole operand.
    Walk,
    /// A snapshot, caught up through the change journal.
    Journal,
    /// A snapshot, caught up by statting every directory.
    Directories,
}

#[cfg(unix)]
impl Source {
    const fn label(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Journal => "journal",
            Self::Directories => "directories",
        }
    }
}

/// One operand, ready to report: its walk, and the snapshot that helped.
#[cfg(unix)]
struct Prepared {
    root: Option<walk::Root>,
    /// The snapshot file this answer started from, if any.
    snapshot: Option<std::path::PathBuf>,
    source: Source,
    /// Whether the answer differs from the snapshot it started from, and
    /// so has to be written back rather than just marked as confirmed.
    changed: bool,
    /// The change journal's position from before this operand was looked
    /// at, for the next catch-up to start from.
    journal: Option<index::Position>,
}

/// Where the change journal of `root`'s volume stands, if it keeps one.
#[cfg(unix)]
fn journal_position(root: &walk::Root) -> Option<index::Position> {
    #[cfg(target_os = "macos")]
    if let Ok(meta) = root.meta {
        let uuid = fsevents::volume_uuid(meta.dev)?;
        return Some((uuid, fsevents::current_event()));
    }
    let _ = root;
    None
}

/// What the journal recorded under `root` since `snapshot`, when it can
/// vouch for the whole interval.
#[cfg(unix)]
fn replay(
    snapshot: &index::Snapshot,
    root: &walk::Root,
    now: Option<index::Position>,
) -> Option<refresh::Journal> {
    #[cfg(target_os = "macos")]
    {
        // An escape hatch, and how the tests reach the other path.
        if std::env::var_os("DISKTREE_DU_JOURNAL").is_some_and(|v| v == "0") {
            return None;
        }
        let ((then_uuid, since), (now_uuid, _)) = (snapshot.journal?, now?);
        if then_uuid != now_uuid {
            return None;
        }
        let path = walk::bytes_path(&root.path);
        let base = std::fs::canonicalize(path).ok()?;
        let base = base.as_os_str().as_encoded_bytes();
        let changes = fsevents::changes_since(base, since)?;
        Some(refresh::Journal::new(base, &changes))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (snapshot, root, now);
        None
    }
}

#[cfg(unix)]
fn prepare(
    options: &args::Options,
    walker: &walk::Walker<'_>,
    index_dir: Option<&std::path::Path>,
    operand: &args::Operand,
) -> Prepared {
    let args::Operand::Path(path) = operand else {
        return Prepared {
            root: None,
            snapshot: None,
            source: Source::Walk,
            changed: false,
            journal: None,
        };
    };
    let mut root = walker.root(path);
    // Taken before anything is read, so the next catch-up replays whatever
    // changes while this one runs.
    let journal = journal_position(&root);
    // The snapshot is read only when it can help: to catch up from for
    // `--max-age`, or where the walk can reuse its listings.
    let wanted = options.max_age.is_some() || walker.reuses_listings();
    let snapshot = match (index_dir, root.meta) {
        (Some(dir), Ok(meta)) if wanted && walker.descends(&root) => {
            index::Snapshot::load(index::Snapshot::file(dir, options, &meta))
        }
        _ => None,
    };
    if let (Some(snapshot), Some(max_age)) = (&snapshot, options.max_age)
        && !options.fresh
        // The index keeps no access times to answer `--time=atime` with.
        && !matches!(options.time, Some((args::TimeKind::Accessed, _)))
        && snapshot.young(max_age)
    {
        let mut changes = replay(snapshot, &root, journal);
        let source = if changes.is_some() {
            Source::Journal
        } else {
            Source::Directories
        };
        let changed = refresh::refresh(
            walker,
            &mut root,
            &snapshot.tree,
            &snapshot.root,
            snapshot.taken,
            changes.as_mut(),
        );
        return Prepared {
            root: Some(root),
            snapshot: Some(snapshot.path().to_owned()),
            source,
            changed,
            journal,
        };
    }
    let previous = snapshot
        .as_ref()
        .filter(|_| !options.fresh)
        .map(index::Snapshot::previous);
    walker.fill(&mut root, previous.as_ref());
    let changed = snapshot
        .as_ref()
        .is_none_or(|snapshot| options.fresh || !index::same(snapshot, &root));
    Prepared {
        root: Some(root),
        snapshot: snapshot.map(|snapshot| snapshot.path().to_owned()),
        source: Source::Walk,
        changed,
        journal,
    }
}

#[cfg(unix)]
fn run(program: &str, options: &args::Options) -> ExitCode {
    use rayon::prelude::*;

    let walker = walk::Walker::new(
        options.deref,
        options.one_file_system,
        &options.excludes,
    );
    let index_dir = if index::indexable(options) {
        index::directory()
    } else {
        None
    };
    let started = index::now();
    // Every operand is walked at once, then reported in order: `du -sh *`
    // is many small walks, and one at a time would leave cores idle.
    let prepared: Vec<Prepared> = options
        .operands
        .par_iter()
        .map(|operand| prepare(options, &walker, index_dir.as_deref(), operand))
        .collect();

    // The index is written while the answer is printed; it is a cache, so
    // failing to write it is not du's failure.
    let save = || {
        let Some(dir) = &index_dir else { return };
        prepared.par_iter().for_each(|p| {
            let Some(root) = &p.root else { return };
            match &p.snapshot {
                Some(snapshot) if !p.changed => index::confirm(snapshot),
                _ => {
                    let _ = index::save(dir, options, root, started, p.journal);
                }
            }
        });
        index::prune(dir);
    };
    let (ok, ()) =
        rayon::join(|| answer(program, options, &prepared, started), save);
    if ok == Some(true) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Print the answer. `None` when standard output failed.
#[cfg(unix)]
fn answer(
    program: &str,
    options: &args::Options,
    prepared: &[Prepared],
    started: walk::Time,
) -> Option<bool> {
    let roots: Vec<Option<&walk::Root>> =
        prepared.iter().map(|p| p.root.as_ref()).collect();
    if options.json {
        let sink = output::Json::new(options.units, options.inodes);
        let (ok, sink) = report_all(program, options, &roots, sink);
        sink.write(index::system_time(started), source(prepared))
            .ok()?;
        Some(ok)
    } else {
        let time_format =
            options.time.as_ref().map(|(_, format)| format.clone());
        let sink = output::Text::new(
            options.units,
            options.inodes,
            time_format,
            options.null,
        );
        let (ok, sink) = report_all(program, options, &roots, sink);
        // Whoever was reading has gone; GNU would die of SIGPIPE.
        (!sink.is_broken()).then_some(ok)
    }
}

/// How the numbers were arrived at, for `--json`: one source, or "mixed".
#[cfg(unix)]
fn source(prepared: &[Prepared]) -> &'static str {
    let mut sources = prepared
        .iter()
        .filter(|p| p.root.is_some())
        .map(|p| p.source);
    let Some(first) = sources.next() else {
        return Source::Walk.label();
    };
    if sources.all(|source| source == first) {
        first.label()
    } else {
        "mixed"
    }
}

#[cfg(unix)]
fn report_all<S: report::Sink>(
    program: &str,
    options: &args::Options,
    roots: &[Option<&walk::Root>],
    sink: S,
) -> (bool, S) {
    let mut report = report::Report::new(options, program, sink);
    for (operand, root) in options.operands.iter().zip(roots) {
        match (operand, root) {
            (args::Operand::Invalid(message), _) => report.error(message),
            (args::Operand::Path(_), Some(root)) => report.operand(root),
            (args::Operand::Path(_), None) => {}
        }
    }
    report.finish()
}

#[cfg(not(unix))]
fn run(program: &str, _options: &args::Options) -> ExitCode {
    eprintln!(
        "{program}: GNU du needs st_dev, st_ino and st_blocks, which this \
         system does not have; use the disktree window instead"
    );
    ExitCode::FAILURE
}
