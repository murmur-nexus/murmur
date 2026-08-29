//! `mur conversation`: what the record store holds, and what an operator can take out of it.
//!
//! A conversation record is the agent's memory — durable and outside every workdir. `ls` is what
//! makes a store an operator can reason about; `rm` and `truncate` are what makes one they can
//! act on. Between them they are also the only way to reclaim a record no capsule owns:
//! automatic retention skips a record whose header line names no capsule, so an abandoned
//! conversation is reachable here and nowhere else.

use capsule_runtime::{
    list_records, locate_message, remove_record, truncate_record, MessageStatus, RecordSummary,
};
use clap::Subcommand;
use serde_json::json;

use crate::error::{CliError, E_CNV_001, E_CNV_002, E_CNV_003, E_IO_003};

#[derive(Debug, Subcommand)]
pub(crate) enum ConversationCommand {
    /// List conversation records: message count, size, last touched, truncation state
    Ls {
        /// Limit to one record store (the directory under ~/.murmur/conversations/)
        #[arg(long, value_name = "NAME")]
        record: Option<String>,
        /// Report where one message id stands: present, truncated, or unknown
        #[arg(long, value_name = "MSG-ID")]
        message: Option<String>,
        /// Print the same values as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove one context's record directory, whole
    Rm {
        /// Context id: the directory under a record store
        context_id: String,
        /// The record store to act on, when the context id appears under more than one
        #[arg(long, value_name = "NAME")]
        record: Option<String>,
    },
    /// Drop the oldest messages from a record, keeping the newest N
    Truncate {
        /// Context id: the directory under a record store
        context_id: String,
        /// Messages to keep. Must be at least 1 — truncating to nothing is `rm`.
        #[arg(long, value_name = "N")]
        keep: u32,
        /// The record store to act on, when the context id appears under more than one
        #[arg(long, value_name = "NAME")]
        record: Option<String>,
    },
}

// ── ls ────────────────────────────────────────────────────────────────────────

pub(crate) fn run_conversation_ls(
    record: Option<String>,
    message: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    if let Some(message_id) = message {
        return report_message(&message_id, json);
    }

    let mut summaries = list_records().map_err(|reason| CliError::new(E_IO_003, reason))?;
    if let Some(ref record) = record {
        summaries.retain(|summary| &summary.record == record);
    }

    if json {
        let rows: Vec<_> = summaries.iter().map(summary_json).collect();
        println!("{}", serde_json::Value::Array(rows));
        return Ok(());
    }

    if summaries.is_empty() {
        match record {
            Some(record) => println!("No conversation records under record store '{record}'."),
            None => println!("No conversation records."),
        }
        return Ok(());
    }

    println!(
        "{:<24} {:<28} {:>8} {:>10}  {:<20} TRUNCATED",
        "RECORD", "CONTEXT", "MESSAGES", "SIZE", "LAST TOUCHED"
    );
    for summary in &summaries {
        println!(
            "{:<24} {:<28} {:>8} {:>10}  {:<20} {}",
            summary.record,
            summary.context_id,
            summary.messages,
            fmt_bytes(summary.bytes),
            summary
                .last_touched_ms
                .map(fmt_timestamp)
                .unwrap_or_else(|| "-".to_string()),
            summary
                .truncation
                .as_ref()
                .map(|marker| format!("{} dropped", marker.dropped))
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    Ok(())
}

/// `ls --message`: the three answers, and why the middle one exists.
///
/// An artifact that stored `source_id: msg_X` and now finds nothing must be told the record was
/// trimmed, not that its reference was never real — a dangling reference that reads as corruption
/// is not acceptable, and unbounded growth is worse than a dangling reference.
fn report_message(message_id: &str, json: bool) -> Result<(), CliError> {
    let found = locate_message(message_id).map_err(|reason| CliError::new(E_IO_003, reason))?;

    if json {
        let rows: Vec<_> = found
            .iter()
            .map(|location| {
                let mut row = json!({
                    "record": location.record,
                    "context_id": location.context_id,
                    "path": location.path.display().to_string(),
                });
                match &location.status {
                    MessageStatus::Present { position, total } => {
                        row["status"] = json!("present");
                        row["position"] = json!(position);
                        row["total"] = json!(total);
                    }
                    MessageStatus::Truncated {
                        dropped,
                        oldest_surviving_id,
                    } => {
                        row["status"] = json!("truncated");
                        row["dropped"] = json!(dropped);
                        row["oldest_surviving_id"] = json!(oldest_surviving_id);
                    }
                    MessageStatus::Unknown => row["status"] = json!("unknown"),
                }
                row
            })
            .collect();
        println!(
            "{}",
            json!({"message_id": message_id, "locations": rows,
                   "status": if found.is_empty() { "unknown" } else { "found" }})
        );
        return Ok(());
    }

    if found.is_empty() {
        println!("unknown: {message_id} is in no record on this host");
        return Ok(());
    }
    for location in &found {
        match &location.status {
            MessageStatus::Present { position, total } => println!(
                "present:   {message_id} is message {position} of {total} in {}/{}",
                location.record, location.context_id
            ),
            MessageStatus::Truncated {
                dropped,
                oldest_surviving_id,
            } => println!(
                "truncated: {message_id} was dropped from {}/{} ({dropped} messages dropped; \
                 oldest surviving is {oldest_surviving_id})",
                location.record, location.context_id
            ),
            MessageStatus::Unknown => {}
        }
    }
    Ok(())
}

// ── rm ────────────────────────────────────────────────────────────────────────

pub(crate) fn run_conversation_rm(
    context_id: &str,
    record: Option<String>,
) -> Result<(), CliError> {
    let target = resolve_context(context_id, record.as_deref())?;
    let removed = remove_record(&target.record, &target.context_id)
        .map_err(|reason| CliError::new(E_IO_003, reason))?;
    println!(
        "removed {} ({} messages)",
        removed.path.display(),
        removed.messages
    );
    Ok(())
}

// ── truncate ──────────────────────────────────────────────────────────────────

pub(crate) fn run_conversation_truncate(
    context_id: &str,
    keep: u32,
    record: Option<String>,
) -> Result<(), CliError> {
    if keep == 0 {
        return Err(CliError::with_hint(
            E_CNV_003,
            "--keep must be at least 1",
            format!("truncating a record to nothing is `mur conversation rm {context_id}`"),
        ));
    }
    let target = resolve_context(context_id, record.as_deref())?;
    let outcome = truncate_record(&target.path, keep, &target.record)
        .map_err(|reason| CliError::new(E_IO_003, reason))?;
    if outcome.dropped == 0 {
        println!(
            "{} already holds {} messages; nothing dropped",
            target.path.display(),
            outcome.kept
        );
        return Ok(());
    }
    println!(
        "dropped {} messages from {} ({} kept; oldest surviving is {})",
        outcome.dropped,
        target.path.display(),
        outcome.kept,
        outcome.oldest_surviving_id
    );
    Ok(())
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// The one record a `<context-id>` names, refusing ambiguity and absence by name.
///
/// A context id is unique inside a record store and nowhere else: two capsules can be handed the
/// same id, and `--record` is how an operator says which one they mean. Guessing between them
/// would delete or rewrite the wrong conversation.
fn resolve_context(context_id: &str, record: Option<&str>) -> Result<RecordSummary, CliError> {
    let summaries = list_records().map_err(|reason| CliError::new(E_IO_003, reason))?;
    let matches: Vec<RecordSummary> = summaries
        .into_iter()
        .filter(|summary| summary.context_id == context_id)
        .filter(|summary| record.is_none_or(|name| summary.record == name))
        .collect();

    match matches.len() {
        0 => {
            Err(CliError::with_hint(
                E_CNV_001,
                match record {
                    Some(record) => format!(
                        "no context '{context_id}' under record store '{record}' \
                     in ~/.murmur/conversations/"
                    ),
                    None => {
                        format!("no context '{context_id}' in any record under ~/.murmur/conversations/")
                    }
                },
                "`mur conversation ls` lists every record and context on this host",
            ))
        }
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => {
            let stores: Vec<&str> = matches
                .iter()
                .map(|summary| summary.record.as_str())
                .collect();
            Err(CliError::with_hint(
                E_CNV_002,
                format!(
                    "context '{context_id}' is present under {} record stores: {}",
                    stores.len(),
                    stores.join(", ")
                ),
                format!(
                    "pass --record <NAME> to say which one, e.g. --record {}",
                    stores[0]
                ),
            ))
        }
    }
}

// ── Formatting ────────────────────────────────────────────────────────────────

fn summary_json(summary: &RecordSummary) -> serde_json::Value {
    json!({
        "record": summary.record,
        "context_id": summary.context_id,
        "path": summary.path.display().to_string(),
        "messages": summary.messages,
        "bytes": summary.bytes,
        "last_touched_ms": summary.last_touched_ms,
        "capsule": summary.capsule,
        "truncated": summary.truncation.as_ref().map(|marker| json!({
            "dropped": marker.dropped,
            "oldest_surviving_id": marker.oldest_surviving_id,
            "last_dropped_id": marker.last_dropped_id,
            "at_ms": marker.at_ms,
        })),
    })
}

fn fmt_bytes(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
    }
}

/// A millisecond timestamp as `YYYY-MM-DD HH:MM:SS` in UTC.
///
/// Written out rather than taken from a date crate: this is the only place in `mur` that formats
/// a wall-clock time, and a dependency for one column is a dependency the whole binary carries.
fn fmt_timestamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_timestamp_reads_as_utc_wall_clock() {
        assert_eq!(fmt_timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(fmt_timestamp(1_756_400_000_000), "2025-08-28 16:53:20");
    }

    #[test]
    fn fmt_bytes_scales_at_the_binary_boundaries() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MiB");
    }
}
