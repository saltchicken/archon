use archon::cli::{CliArgs, ScheduleCommand, SkillCommand};
use std::path::PathBuf;

fn parse_sched(args: &[&str]) -> anyhow::Result<ScheduleCommand> {
    CliArgs::parse_schedule_args(args.iter().map(|s| (*s).to_string()))
}

fn parse_skill(args: &[&str]) -> anyhow::Result<SkillCommand> {
    CliArgs::parse_skill_args(args.iter().map(|s| (*s).to_string()))
}

#[test]
fn parse_skill_subcommands_and_flags() {
    assert_eq!(
        parse_skill(&["check"]).unwrap(),
        SkillCommand::Check {
            name: None,
            dir: None,
            global: false,
            strict: false,
        }
    );
    assert_eq!(
        parse_skill(&["check", "coder", "--strict", "--dir", "./skills"]).unwrap(),
        SkillCommand::Check {
            name: Some("coder".to_string()),
            dir: Some(PathBuf::from("./skills")),
            global: false,
            strict: true,
        }
    );
    assert_eq!(
        parse_skill(&["list", "--global"]).unwrap(),
        SkillCommand::List {
            dir: None,
            global: true,
        }
    );
    assert!(parse_skill(&["check", "a", "b"]).is_err());
    assert!(parse_skill(&["check", "--dir", "./skills", "--global"]).is_err());
    assert!(parse_skill(&["list", "--strict"]).is_err());
    assert!(parse_skill(&["list", "extra"]).is_err());
    assert!(parse_skill(&["bogus"]).is_err());
}

#[test]
fn parse_schedule_subcommands_and_flags() {
    assert_eq!(
        parse_sched(&["sync"]).unwrap(),
        ScheduleCommand::Sync { global: false }
    );
    assert_eq!(
        parse_sched(&["sync", "--global"]).unwrap(),
        ScheduleCommand::Sync { global: true }
    );
    assert_eq!(
        parse_sched(&["run", "daily", "-g"]).unwrap(),
        ScheduleCommand::Run {
            name: "daily".to_string(),
            global: true,
        }
    );
    assert_eq!(
        parse_sched(&["logs", "daily", "-n", "100"]).unwrap(),
        ScheduleCommand::Logs {
            name: Some("daily".to_string()),
            lines: 100,
            global: false,
        }
    );
    assert_eq!(
        parse_sched(&["logs", "-n", "25", "-g"]).unwrap(),
        ScheduleCommand::Logs {
            name: None,
            lines: 25,
            global: true,
        }
    );
}

#[test]
fn parse_schedule_rejects_extra_or_missing_positional_args() {
    assert!(parse_sched(&["sync", "extra"]).is_err());
    assert!(parse_sched(&["list", "extra"]).is_err());
    assert!(parse_sched(&["clean", "extra"]).is_err());
    assert!(parse_sched(&["run"]).is_err());
    assert!(parse_sched(&["run", "one", "two"]).is_err());
    assert!(parse_sched(&["logs", "one", "two"]).is_err());
}
