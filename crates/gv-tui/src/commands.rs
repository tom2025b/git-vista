//! Closed terminal command grammar for M10.06 (#461).
//!
//! This is an operation builder, not an argv parser: every accepted command
//! constructs one existing protocol value (or the read-only tag listing), and
//! every other token is rejected before the data layer can make a request.
//!
//! ```text
//! branch create NAME OID        branch checkout|merge|delete|force-delete NAME
//! commit [--allow-empty] MESSAGE
//! amend EXPECTED_HEAD [--allow-empty] MESSAGE
//! tag list|delete NAME          tag create NAME OID
//! tag annotate|sign NAME OID MESSAGE
//! tag push|delete-remote NAME REMOTE
//! fetch REMOTE                 pull REMOTE BRANCH merge|rebase
//! push BRANCH REMOTE [--set-upstream] [--force-with-lease=OID]
//! ```

use git_vista_protocol::{
    BranchName, CommitMessage, CommitOid, ForcePublish, GitOperation, MergeStrategy, RemoteName,
    TagAnnotation, TagMessage, TagName,
};

pub const HELP: &str = "branch|commit|amend|tag|fetch|pull|push — type :help for forms";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Plan(GitOperation),
    ListTags,
    Help,
}

/// Parse the command text after `:` into the closed protocol vocabulary.
pub fn parse(input: &str) -> Result<Command, String> {
    let mut words = Words::new(input);
    let verb = words.required("command")?;
    match verb {
        "help" => words.done(Command::Help),
        "branch" => parse_branch(words),
        "commit" => parse_commit(words),
        "amend" => parse_amend(words),
        "tag" => parse_tag(words),
        "fetch" => {
            let remote = remote(words.required("remote")?)?;
            words.done(Command::Plan(GitOperation::FetchRemote { remote }))
        }
        "pull" => parse_pull(words),
        "push" => parse_push(words),
        other => Err(format!("unknown command '{other}' · {HELP}")),
    }
}

fn parse_branch(mut words: Words<'_>) -> Result<Command, String> {
    let operation = match words.required("branch action")? {
        "create" => GitOperation::CreateBranch {
            name: branch(words.required("branch name")?)?,
            at: oid(words.required("commit oid")?)?,
        },
        "checkout" => GitOperation::CheckoutBranch {
            branch: branch(words.required("branch name")?)?,
        },
        "merge" => GitOperation::MergeBranch {
            branch: branch(words.required("branch name")?)?,
        },
        "delete" => GitOperation::DeleteBranch {
            branch: branch(words.required("branch name")?)?,
        },
        "force-delete" => GitOperation::ForceDeleteBranch {
            branch: branch(words.required("branch name")?)?,
        },
        action => {
            return Err(format!(
                "unknown branch action '{action}' · use create|checkout|merge|delete|force-delete"
            ));
        }
    };
    words.done(Command::Plan(operation))
}

fn parse_commit(mut words: Words<'_>) -> Result<Command, String> {
    let allow_empty = words.take_if("--allow-empty");
    let message = CommitMessage::new(words.rest_required("commit message")?)
        .map_err(|error| error.to_string())?;
    Ok(Command::Plan(GitOperation::CommitOnHead {
        message,
        allow_empty,
    }))
}

fn parse_amend(mut words: Words<'_>) -> Result<Command, String> {
    let expected_tip = oid(words.required("expected HEAD oid")?)?;
    let allow_empty = words.take_if("--allow-empty");
    let message = CommitMessage::new(words.rest_required("commit message")?)
        .map_err(|error| error.to_string())?;
    Ok(Command::Plan(GitOperation::AmendCommit {
        message,
        expected_tip,
        allow_empty,
    }))
}

fn parse_tag(mut words: Words<'_>) -> Result<Command, String> {
    let action = words.required("tag action")?;
    let operation = match action {
        "list" => return words.done(Command::ListTags),
        "create" => GitOperation::CreateTag {
            name: tag(words.required("tag name")?)?,
            target: oid(words.required("target oid")?)?,
            annotation: None,
        },
        "annotate" | "sign" => {
            let name = tag(words.required("tag name")?)?;
            let target = oid(words.required("target oid")?)?;
            let message = TagMessage::new(words.rest_required("tag message")?)
                .map_err(|error| error.to_string())?;
            return Ok(Command::Plan(GitOperation::CreateTag {
                name,
                target,
                annotation: Some(TagAnnotation {
                    message,
                    sign: action == "sign",
                }),
            }));
        }
        "delete" => GitOperation::DeleteLocalTag {
            name: tag(words.required("tag name")?)?,
        },
        "push" => GitOperation::PushTag {
            name: tag(words.required("tag name")?)?,
            remote: remote(words.required("remote")?)?,
        },
        "delete-remote" => GitOperation::DeleteRemoteTag {
            name: tag(words.required("tag name")?)?,
            remote: remote(words.required("remote")?)?,
        },
        other => {
            return Err(format!(
                "unknown tag action '{other}' · use list|create|annotate|sign|delete|push|delete-remote"
            ));
        }
    };
    words.done(Command::Plan(operation))
}

fn parse_pull(mut words: Words<'_>) -> Result<Command, String> {
    let remote = remote(words.required("remote")?)?;
    let branch = branch(words.required("remote branch")?)?;
    let strategy = match words.required("merge strategy")? {
        "merge" => MergeStrategy::Merge,
        "rebase" => MergeStrategy::Rebase,
        other => {
            return Err(format!(
                "unknown pull strategy '{other}' · use merge|rebase"
            ))
        }
    };
    words.done(Command::Plan(GitOperation::PullBranch {
        remote,
        branch,
        strategy,
    }))
}

fn parse_push(mut words: Words<'_>) -> Result<Command, String> {
    let branch = branch(words.required("branch name")?)?;
    let remote = remote(words.required("remote")?)?;
    let mut set_upstream = false;
    let mut force = ForcePublish::None;
    while let Some(flag) = words.next() {
        if flag == "--set-upstream" {
            if set_upstream {
                return Err(String::from("--set-upstream was supplied twice"));
            }
            set_upstream = true;
        } else if let Some(value) = flag.strip_prefix("--force-with-lease=") {
            if !matches!(force, ForcePublish::None) {
                return Err(String::from("--force-with-lease was supplied twice"));
            }
            force = ForcePublish::WithLease {
                expected_remote_tip: oid(value)?,
            };
        } else {
            return Err(format!("unknown push flag '{flag}'"));
        }
    }
    Ok(Command::Plan(GitOperation::PushBranch {
        branch,
        remote,
        set_upstream,
        force,
    }))
}

fn branch(value: &str) -> Result<BranchName, String> {
    BranchName::new(value).map_err(|error| error.to_string())
}

fn remote(value: &str) -> Result<RemoteName, String> {
    RemoteName::new(value).map_err(|error| error.to_string())
}

fn tag(value: &str) -> Result<TagName, String> {
    TagName::new(value).map_err(|error| error.to_string())
}

fn oid(value: &str) -> Result<CommitOid, String> {
    CommitOid::new(value).map_err(|error| error.to_string())
}

struct Words<'a> {
    rest: &'a str,
}

impl<'a> Words<'a> {
    fn new(input: &'a str) -> Self {
        Self { rest: input.trim() }
    }

    fn next(&mut self) -> Option<&'a str> {
        let rest = self.rest.trim_start();
        if rest.is_empty() {
            self.rest = "";
            return None;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        self.rest = tail.trim_start();
        Some(word)
    }

    fn required(&mut self, label: &str) -> Result<&'a str, String> {
        self.next().ok_or_else(|| format!("missing {label}"))
    }

    fn take_if(&mut self, expected: &str) -> bool {
        let saved = self.rest;
        if self.next() == Some(expected) {
            true
        } else {
            self.rest = saved;
            false
        }
    }

    fn rest_required(&mut self, label: &str) -> Result<&'a str, String> {
        let rest = self.rest.trim();
        self.rest = "";
        if rest.is_empty() {
            Err(format!("missing {label}"))
        } else {
            Ok(rest)
        }
    }

    fn done<T>(&mut self, value: T) -> Result<T, String> {
        match self.next() {
            None => Ok(value),
            Some(extra) => Err(format!("unexpected extra argument '{extra}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn plan(input: &str) -> GitOperation {
        let Command::Plan(operation) = parse(input).unwrap() else {
            panic!("expected a planned operation");
        };
        operation
    }

    #[test]
    fn every_branch_write_maps_to_its_existing_operation() {
        assert!(matches!(
            plan(&format!("branch create topic {A}")),
            GitOperation::CreateBranch { .. }
        ));
        assert!(matches!(
            plan("branch checkout topic"),
            GitOperation::CheckoutBranch { .. }
        ));
        assert!(matches!(
            plan("branch merge topic"),
            GitOperation::MergeBranch { .. }
        ));
        assert!(matches!(
            plan("branch delete topic"),
            GitOperation::DeleteBranch { .. }
        ));
        assert!(matches!(
            plan("branch force-delete topic"),
            GitOperation::ForceDeleteBranch { .. }
        ));
    }

    #[test]
    fn commit_and_amend_preserve_the_message_remainder_and_explicit_empty_choice() {
        assert_eq!(
            plan("commit --allow-empty subject  with  spacing"),
            GitOperation::CommitOnHead {
                message: CommitMessage::new("subject  with  spacing").unwrap(),
                allow_empty: true,
            }
        );
        assert_eq!(
            plan(&format!("amend {A} corrected message")),
            GitOperation::AmendCommit {
                message: CommitMessage::new("corrected message").unwrap(),
                expected_tip: CommitOid::new(A).unwrap(),
                allow_empty: false,
            }
        );
    }

    #[test]
    fn all_tag_forms_are_closed_and_signing_is_never_silently_dropped() {
        assert_eq!(parse("tag list"), Ok(Command::ListTags));
        assert!(matches!(
            plan(&format!("tag create v1 {A}")),
            GitOperation::CreateTag {
                annotation: None,
                ..
            }
        ));
        assert!(matches!(
            plan(&format!("tag annotate v1 {A} release notes")),
            GitOperation::CreateTag {
                annotation: Some(TagAnnotation { sign: false, .. }),
                ..
            }
        ));
        assert!(matches!(
            plan(&format!("tag sign v1 {A} signed release")),
            GitOperation::CreateTag {
                annotation: Some(TagAnnotation { sign: true, .. }),
                ..
            }
        ));
        assert!(matches!(
            plan("tag delete v1"),
            GitOperation::DeleteLocalTag { .. }
        ));
        assert!(matches!(
            plan("tag push v1 origin"),
            GitOperation::PushTag { .. }
        ));
        assert!(matches!(
            plan("tag delete-remote v1 origin"),
            GitOperation::DeleteRemoteTag { .. }
        ));
    }

    #[test]
    fn network_forms_make_strategy_upstream_and_lease_explicit() {
        assert!(matches!(
            plan("fetch origin"),
            GitOperation::FetchRemote { .. }
        ));
        assert!(matches!(
            plan("pull origin main rebase"),
            GitOperation::PullBranch {
                strategy: MergeStrategy::Rebase,
                ..
            }
        ));
        assert_eq!(
            plan(&format!(
                "push main origin --set-upstream --force-with-lease={B}"
            )),
            GitOperation::PushBranch {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: true,
                force: ForcePublish::WithLease {
                    expected_remote_tip: CommitOid::new(B).unwrap()
                },
            }
        );
    }

    #[test]
    fn unknown_or_malformed_input_never_becomes_an_operation() {
        for input in [
            "",
            "git status",
            "branch rename old new",
            "branch create --bad aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "commit",
            "amend nope message",
            "tag sign v1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "fetch https://example.com/repo.git",
            "pull origin main auto",
            "push main origin --force",
            "push main origin --force-with-lease=nope",
        ] {
            assert!(parse(input).is_err(), "{input:?} unexpectedly parsed");
        }
    }
}
