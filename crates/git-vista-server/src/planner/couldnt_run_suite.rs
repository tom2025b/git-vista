//! #666: exercise the real helper and environment reader in isolated child
//! processes, so parallel tests never race a process-global flag mutation.

use super::{couldnt_run, RunFailure};
use axum::http::StatusCode;
use std::path::Path;
use std::process::Command;

const PROBE: &str = "GV_666_PROBE";
const DETAIL: &str = "  cannot open /home/operator/private-repo/.git/config\npermission denied  ";
const CONTEXT: &str = "/home/operator/private-endpoint";

#[test]
fn subprocess_probe() {
    if std::env::var_os(PROBE).is_none() {
        return;
    }
    let error = std::io::Error::other(DETAIL);
    let replies: Vec<_> = [
        RunFailure::Spawn,
        RunFailure::VerifyPlan,
        RunFailure::PushUnobserved,
    ]
    .into_iter()
    .map(|reason| {
        let (status, body) = couldnt_run(CONTEXT, reason, &error);
        (status.as_u16(), body)
    })
    .collect();
    println!("\nGV666_BODY={}", serde_json::to_string(&replies).unwrap());
}

fn probe(flag: Option<&str>) -> (Vec<(u16, String)>, String) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "planner::couldnt_run_suite::subprocess_probe",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .env_remove("GIT_VISTA_EXPOSE_PATHS");
    if let Some(flag) = flag {
        command.env("GIT_VISTA_EXPOSE_PATHS", flag);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let body = stdout
        .lines()
        .find_map(|line| line.strip_prefix("GV666_BODY="))
        .expect("the child must call the real helper and emit its responses");
    (
        serde_json::from_str(body).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn path_bearing_errors_are_withheld_for_every_off_spelling() {
    let (unset, _) = probe(None);
    for (_, body) in &unset {
        assert!(
            !body.contains("/home/operator"),
            "detail or context leaked: {body}"
        );
        assert!(!body.contains("permission denied"), "detail leaked: {body}");
    }
    for flag in ["", "0", "false", "no", "off", " off "] {
        assert_eq!(probe(Some(flag)).0, unset, "off spelling: {flag:?}");
    }
}

#[test]
fn opted_in_clients_receive_the_detail_and_the_same_useful_reason() {
    let (withheld, _) = probe(None);
    for flag in ["1", "true"] {
        let (disclosed, _) = probe(Some(flag));
        for ((off_status, summary), (status, body)) in withheld.iter().zip(disclosed) {
            assert_eq!(status, *off_status);
            assert_eq!(body, format!("{summary} {}", DETAIL.trim()));
            assert!(!body.contains(CONTEXT), "log-only context leaked: {body}");
        }
    }
}

/// Paired positive: hiding a path by deleting the useful explanation must
/// fail here, independently of the leak assertion above and the source pins.
#[test]
fn withholding_preserves_the_failure_stage_and_a_useful_next_step() {
    let (replies, _) = probe(None);
    assert_eq!(replies.len(), 3);
    for (status, _) in &replies {
        assert_eq!(*status, StatusCode::INTERNAL_SERVER_ERROR.as_u16());
    }
    assert_eq!(
        replies[0].1,
        "Couldn't run git. Check the server log for details."
    );
    assert!(replies[1]
        .1
        .contains("plan cannot be re-verified before executing"));
    assert!(replies[2].1.contains("The push ran"));
    assert!(replies[2]
        .1
        .contains("what it did to the remote is unknown"));
    assert!(replies[2].1.contains("Check its state before retrying."));
}

#[test]
fn complete_diagnostics_are_logged_unconditionally() {
    let (_, off_log) = probe(None);
    let (_, on_log) = probe(Some("1"));
    assert_eq!(
        off_log, on_log,
        "client opt-in must not affect the operator log"
    );
    assert_eq!(off_log.matches(DETAIL).count(), 3, "{off_log}");
    assert_eq!(off_log.matches(CONTEXT).count(), 3, "{off_log}");
}

fn normalise(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Same exact-body mechanism as worktree_add_suite / contract_suite: changing
/// the one disclosure boundary or widening the reason type is a reviewed diff.
#[test]
fn the_helper_and_closed_reason_vocabulary_are_pinned() {
    let source = include_str!("../planner.rs");
    let helper = source
        .split_once("pub(crate) fn couldnt_run<")
        .unwrap()
        .1
        .split_once("\n}\n")
        .unwrap()
        .0;
    assert_eq!(
        normalise(helper),
        "E: std::fmt::Display + ?Sized>( endpoint: &str, reason: RunFailure, detail: &E, ) -> (StatusCode, String) { ( StatusCode::INTERNAL_SERVER_ERROR, crate::state::withheld_detail(endpoint, reason.summary(), &detail.to_string()), )",
        "Review the helper's disclosure/logging contract before updating this pin."
    );
    assert_eq!(
        normalise(include_str!("run_failure.rs")),
        include_str!("fixtures/couldnt_run_reasons.txt").trim(),
        "Review every reason: fixed client-safe text, no arbitrary payload or raw escape hatch."
    );
}

/// Recursively discover source files, including new modules; a hard-coded file
/// list would miss caller 53. The only excluded file is this cfg(test) suite.
fn collect_calls(root: &Path, dir: &Path, calls: &mut Vec<String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_calls(root, &path, calls);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && path.file_name().unwrap() != "couldnt_run_suite.rs"
        {
            let source = normalise(&std::fs::read_to_string(&path).unwrap());
            for (start, _) in source.match_indices("couldnt_run(") {
                let tail = &source[start..];
                let mut depth = 0;
                let mut quoted = false;
                let mut escaped = false;
                let end = tail
                    .char_indices()
                    .find_map(|(i, ch)| {
                        if quoted {
                            if escaped {
                                escaped = false;
                            } else if ch == '\\' {
                                escaped = true;
                            } else if ch == '"' {
                                quoted = false;
                            }
                        } else if ch == '"' {
                            quoted = true;
                        } else if ch == '(' {
                            depth += 1;
                        } else if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + 1);
                            }
                        }
                        None
                    })
                    .expect("unclosed couldnt_run call");
                calls.push(format!(
                    "{}: {}",
                    path.strip_prefix(root).unwrap().display(),
                    &tail[..end]
                ));
            }
        }
    }
}

#[test]
fn every_caller_is_censused_with_its_reason_and_unmodified_detail() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut calls = Vec::new();
    collect_calls(&root, &root, &mut calls);
    assert_eq!(
        calls.join("\n"),
        include_str!("fixtures/couldnt_run_calls.txt").trim(),
        "A caller changed or was added. Review its reason, preserve the full diagnostic, then update this census deliberately."
    );
}
