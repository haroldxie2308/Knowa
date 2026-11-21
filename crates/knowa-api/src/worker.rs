//! External workers (DESIGN §7.1): feed → command → write, with a persisted cursor.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use knowa_config::{Actor, Home};
use knowa_model::{ActorKind, ClaimView, Draft, Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};

use crate::feed::{Cursor, check_limit, feed};
use crate::select::{Space, current};
use crate::write::{Txn, stamp_worker};
use crate::{Sel, WorkerRunResult};

/// DESIGN §7.1 / CLI docs: `--follow` polls every 2 s.
const POLL: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorFile {
    cursor: String,
}

pub(crate) fn worker_run(
    home: &Home,
    sel: &Sel,
    actor_name: &str,
    cmd: &[String],
    batch: u32,
    follow: bool,
) -> Result<WorkerRunResult> {
    let Some(program) = cmd.first() else {
        return Err(Error::usage("worker run: --cmd is empty"));
    };
    check_limit(batch, "batch")?;
    let actor = home.actor(actor_name).map_err(|e| match e.code {
        ErrorCode::NotFound => Error::usage(format!(
            "worker run: no actor named {actor_name:?}; create it with `knowa actor new {actor_name} --kind worker`"
        )),
        _ => e,
    })?;
    if actor.kind == ActorKind::Human {
        return Err(Error::usage(format!(
            "worker run: actor {actor_name:?} is a human; workers run as an agent or worker actor so their output stays distinguishable"
        )));
    }
    let space = current(home, sel)?;
    space.ensure_writable()?;
    // Machine-local, next to the index (DESIGN §7.1), although the cursor itself would
    // hold on any machine reading the replica.
    let cursor_file = space.index_dir.join("cursors").join(format!("{actor_name}.json"));
    let mut cursor = load_cursor(&cursor_file)?;
    let mut run = WorkerRunResult { batches: 0, claims_in: 0, claims_out: 0, cursor: String::new() };
    loop {
        let page = feed(&space, cursor.as_deref(), batch).map_err(|e| expired(e, &cursor_file))?;
        if page.claims.is_empty() {
            run.cursor = page.cursor;
            if follow {
                std::thread::sleep(POLL);
                continue;
            }
            return Ok(run);
        }
        // The worker's own output is never fed back to it (no feedback loops); the
        // cursor still moves past it.
        let input: Vec<ClaimView> = page.claims.into_iter().filter(|c| c.claim.body.actor != actor.id).collect();
        if !input.is_empty() {
            let drafts = run_command(home, &space, &actor, program, &cmd[1..], &page.cursor, &input)?;
            run.batches += 1;
            run.claims_in += input.len() as u64;
            if !drafts.is_empty() {
                let mut txn = Txn::begin(&space, &[&actor])?;
                run.claims_out += txn.write(&actor, drafts)?.created;
            }
        }
        // Only after the command succeeded and its output is written.
        save_cursor(&cursor_file, &page.cursor)?;
        run.cursor = page.cursor.clone();
        cursor = Some(page.cursor);
    }
}

/// Runs the command once. Stdin: `{space, cursor, claims}` where `cursor` is the
/// position after this batch (what is persisted when the batch succeeds).
fn run_command(
    home: &Home,
    space: &Space,
    actor: &Actor,
    program: &str,
    args: &[String],
    cursor: &str,
    claims: &[ClaimView],
) -> Result<Vec<Draft>> {
    let input = serde_json::to_vec(&serde_json::json!({ "space": space.id(), "cursor": cursor, "claims": claims }))?;
    let mut child = Command::new(program)
        .args(args)
        .env("KNOWA_HOME", home.root())
        // The canonical location, which resolves to this space's registration, blob
        // location included, when it is registered.
        .env("KNOWA_SPACE", &space.sref.location)
        .env("KNOWA_ACTOR", &actor.name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Error::not_found(format!("worker run: command not found: {program}")),
            _ => Error::internal(format!("worker run: cannot start {program}: {e}")),
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::internal("worker run: child stdin missing although it was piped"))?;
    // Written from a thread so a child that emits output before draining its input
    // cannot deadlock against us.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    match writer.join() {
        Ok(Ok(())) => {}
        // Fallback: the child exited without reading all of its input. That is the
        // command's choice (its exit status below decides success), not a failure of
        // the hand-off.
        Ok(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Ok(Err(e)) => return Err(Error::internal(format!("worker run: writing the batch to {program}: {e}"))),
        Err(_) => return Err(Error::internal("worker run: stdin writer thread panicked")),
    }
    if !output.status.success() {
        return Err(Error::internal(format!("worker run: {program} failed ({}); cursor not advanced", output.status))
            .with_detail(serde_json::json!({
                "cmd": std::iter::once(program).chain(args.iter().map(String::as_str)).collect::<Vec<_>>(),
                "exitCode": output.status.code(),
                "stderr": String::from_utf8_lossy(&output.stderr),
            })));
    }
    parse_drafts(&output.stdout, &actor.name)
}

/// Command stdout: JSONL drafts; each gets `via.worker = <actor name>`.
pub(crate) fn parse_drafts(stdout: &[u8], worker: &str) -> Result<Vec<Draft>> {
    let text = std::str::from_utf8(stdout).map_err(|e| Error::invalid(format!("worker output is not UTF-8: {e}")))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let at_line = |mut e: Error| {
            e.message = format!("worker output line {}: {}", i + 1, e.message);
            e
        };
        // JSONL: blank lines (e.g. a trailing newline) separate nothing and carry no draft.
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| at_line(e.into()))?;
        let mut d = Draft::from_json(&v).map_err(at_line)?;
        stamp_worker(&mut d, worker).map_err(at_line)?;
        out.push(d);
    }
    Ok(out)
}

fn load_cursor(path: &Path) -> Result<Option<String>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        // First run of this worker in this space: start at the beginning of the feed.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let bad = |why: String| {
        Error::invalid(format!(
            "worker cursor file {} is malformed ({why}); delete it to restart the worker from the beginning",
            path.display()
        ))
    };
    let f: CursorFile = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    Cursor::parse(&f.cursor).map_err(|e| bad(e.message))?;
    Ok(Some(f.cursor))
}

/// Atomic replace (temp file + rename).
fn save_cursor(path: &Path, cursor: &str) -> Result<()> {
    let dir = path.parent().ok_or_else(|| Error::internal("worker cursor path has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let tmp: PathBuf = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(&CursorFile { cursor: cursor.to_string() })?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The persisted cursor predates a new rebuild generation of the replica.
fn expired(e: Error, cursor_file: &Path) -> Error {
    if e.code != ErrorCode::CursorExpired {
        return e;
    }
    Error::cursor_expired(format!(
        "worker cursor is from an earlier rebuild generation of the replica (a merge has filled gaps in its logs since, possibly below seqs the worker has passed); delete {} to restart the worker from the beginning. The whole feed is then replayed through the command: a draft equal to an active, non-stale claim the worker already wrote is a no-op, but a draft using placeholders (_:x) mints new entities and is written again, duplicating its earlier output",
        cursor_file.display()
    ))
    .with_detail(serde_json::json!({ "cursorFile": cursor_file }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRAFT: &str = r#"{"s":"e_01arz3ndektsv4rrffq69g5fav","p":"knowa:summary","o":{"text":"hi"}}"#;

    #[test]
    fn drafts_get_the_worker_stamped() {
        let out = format!("{DRAFT}\n\n{DRAFT}\n");
        let drafts = parse_drafts(out.as_bytes(), "summarizer").unwrap();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().all(|d| d.via.get("worker").map(String::as_str) == Some("summarizer")));
    }

    #[test]
    fn empty_output_is_no_drafts() {
        assert!(parse_drafts(b"", "w").unwrap().is_empty());
        assert!(parse_drafts(b"\n  \n", "w").unwrap().is_empty());
    }

    #[test]
    fn foreign_worker_name_is_invalid() {
        let line = r#"{"s":"e_01arz3ndektsv4rrffq69g5fav","p":"knowa:summary","o":{"text":"hi"},"via":{"worker":"other"}}"#;
        let e = parse_drafts(line.as_bytes(), "summarizer").unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.message.starts_with("worker output line 1:"), "{}", e.message);
    }

    #[test]
    fn bad_json_names_the_line() {
        let out = format!("{DRAFT}\nnot json\n");
        let e = parse_drafts(out.as_bytes(), "w").unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.message.starts_with("worker output line 2:"), "{}", e.message);
    }

    #[test]
    fn cursor_file_round_trip_and_validation() {
        let dir = std::env::temp_dir().join(format!("knowa-api-cursor-{}", std::process::id()));
        let file = dir.join("cursors").join("w.json");
        assert_eq!(load_cursor(&file).unwrap(), None);
        let actor = knowa_model::Signer::from_seed([7; 32]).actor_id();
        let cursor = format!("fc1.01jd5k8v0m3q7x9yz2c4b6n8pr.{actor}.12");
        save_cursor(&file, &cursor).unwrap();
        assert_eq!(load_cursor(&file).unwrap(), Some(cursor));
        // The retired `<epoch>:<n>` form is malformed like any other text.
        for stale in [r#"{"cursor":"7f3a9c0e1d2b4a5f8e9d0c1b2a3f4e5d:12"}"#, r#"{"cursor":"nonsense"}"#] {
            fs::write(&file, stale).unwrap();
            let e = load_cursor(&file).unwrap_err();
            assert_eq!(e.code, ErrorCode::Invalid, "{stale}");
            assert!(e.message.contains("delete it"), "{}", e.message);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expired_cursor_names_the_file() {
        let file = Path::new("/home/index/s_x/k/cursors/w.json");
        let e = expired(Error::cursor_expired("x"), file);
        assert_eq!(e.code, ErrorCode::CursorExpired);
        assert!(e.message.contains("/home/index/s_x/k/cursors/w.json"));
        let other = expired(Error::invalid("y"), Path::new("/f"));
        assert_eq!(other.message, "y");
    }

    /// A replay re-mints placeholder entities, so it duplicates that output; the
    /// message must not promise it is safe.
    #[test]
    fn expired_cursor_message_warns_that_placeholder_output_is_duplicated() {
        let m = expired(Error::cursor_expired("x"), Path::new("/c.json")).message;
        assert!(!m.contains("safe"), "{m}");
        assert!(m.contains("placeholders (_:x)") && m.contains("duplicating"), "{m}");
    }
}
