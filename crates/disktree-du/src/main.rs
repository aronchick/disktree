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
#[cfg(unix)]
mod index;
#[cfg(unix)]
mod output;
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

/// One operand, ready to report: its walk, and the snapshot that helped.
#[cfg(unix)]
struct Prepared {
    root: Option<walk::Root>,
    snapshot: Option<index::Snapshot>,
    /// When the index answered for this operand, as of when.
    from_index: Option<std::time::SystemTime>,
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
            from_index: None,
        };
    };
    let mut root = walker.root(path);
    // The snapshot is read only when it can help: to answer for `--max-age`,
    // or where the walk can reuse its listings.
    let wanted = options.max_age.is_some() || walker.reuses_listings();
    let snapshot = match (index_dir, root.meta) {
        (Some(dir), Ok(meta)) if wanted && walker.descends(&root) => {
            index::Snapshot::load(index::Snapshot::file(dir, options, &meta))
        }
        _ => None,
    };
    if let (Some(snapshot), Some(max_age), Ok(meta)) =
        (&snapshot, options.max_age, root.meta)
        && !options.fresh
        // The index keeps no access times to answer `--time=atime` with.
        && !matches!(options.time, Some((args::TimeKind::Accessed, _)))
        && snapshot.trusted(max_age, &meta)
    {
        root.dir = Some(std::sync::Arc::clone(&snapshot.tree));
        return Prepared {
            root: Some(root),
            snapshot: None,
            from_index: Some(snapshot.confirmed),
        };
    }
    let previous = snapshot
        .as_ref()
        .filter(|_| !options.fresh)
        .map(index::Snapshot::previous);
    walker.fill(&mut root, previous.as_ref());
    Prepared {
        root: Some(root),
        snapshot,
        from_index: None,
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
            let Some(root) = p.root.as_ref().filter(|_| p.from_index.is_none())
            else {
                return;
            };
            match &p.snapshot {
                Some(snapshot)
                    if !options.fresh && index::same(snapshot, root) =>
                {
                    snapshot.confirm();
                }
                _ => {
                    let _ = index::save(dir, options, root, started);
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
        let (as_of, source) = provenance(prepared, started);
        let reused = prepared
            .iter()
            .filter(|p| p.from_index.is_none())
            .filter_map(|p| p.root.as_ref())
            .map(reused_listings)
            .sum();
        sink.write(as_of, source, reused).ok()?;
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

/// When the numbers were true, and where they came from, for `--json`.
/// Answers from the index are as old as the oldest snapshot used.
#[cfg(unix)]
fn provenance(
    prepared: &[Prepared],
    started: walk::Time,
) -> (std::time::SystemTime, &'static str) {
    let walked = prepared
        .iter()
        .any(|p| p.from_index.is_none() && p.root.is_some());
    match prepared.iter().filter_map(|p| p.from_index).min() {
        None => (index::system_time(started), "walk"),
        Some(oldest) => (oldest, if walked { "mixed" } else { "index" }),
    }
}

#[cfg(unix)]
fn reused_listings(root: &walk::Root) -> usize {
    let mut count = 0;
    let mut stack: Vec<&walk::Listing> = root
        .dir
        .as_deref()
        .and_then(std::sync::OnceLock::get)
        .into_iter()
        .collect();
    while let Some(listing) = stack.pop() {
        count += usize::from(listing.reused);
        stack.extend(
            listing
                .entries
                .iter()
                .filter_map(|entry| entry.dir.as_deref())
                .filter_map(std::sync::OnceLock::get),
        );
    }
    count
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
