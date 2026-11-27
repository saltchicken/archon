use crate::core::{
    EngineCommand, EngineEvent, TruncatedToolAction, TurnConfig, config::Config, skills,
};
use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    env,
    fs::File,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    path::PathBuf,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleCommand {
    Sync {
        global: bool,
    },
    List {
        global: bool,
    },
    Run {
        name: String,
        global: bool,
    },
    Logs {
        name: Option<String>,
        lines: usize,
        global: bool,
    },
    Clean {
        global: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillCommand {
    Check {
        name: Option<String>,
        dir: Option<PathBuf>,
        global: bool,
        strict: bool,
    },
    List {
        dir: Option<PathBuf>,
        global: bool,
    },
}

#[derive(Debug, Default)]
pub struct CliArgs {
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub system_prompt: Option<String>,
    pub skill: Option<String>,
    pub skill_args: Vec<(String, String)>,
    pub session_id: Option<String>,
    pub images: Vec<PathBuf>,
    pub allowed_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_save: bool,
    pub auto_approve_all: bool,
    pub show_thoughts: bool,
    pub headless: bool,
    pub schedule_cmd: Option<ScheduleCommand>,
    pub skill_cmd: Option<SkillCommand>,
}

impl CliArgs {
    pub fn parse() -> Result<Self> {
        let mut cli = Self::default();
        let mut args = env::args().skip(1).peekable();

        if args.peek().is_some_and(|a| a == "schedule") {
            args.next();
            cli.schedule_cmd = Some(Self::parse_schedule_args(args)?);
            return Ok(cli);
        }

        if args.peek().is_some_and(|a| a == "skill") {
            args.next();
            cli.skill_cmd = Some(Self::parse_skill_args(args)?);
            return Ok(cli);
        }

        let mut positional = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    Self::print_help();
                    std::process::exit(0);
                }
                "-m" | "--model" => {
                    cli.model = Some(Self::require_value(&mut args, arg)?);
                }
                "-p" | "--prompt" => {
                    cli.prompt = Some(Self::require_value(&mut args, arg)?);
                    cli.headless = true;
                }
                "-s" | "--system" => {
                    cli.system_prompt = Some(Self::require_value(&mut args, arg)?);
                }
                "--skill" => {
                    cli.skill = Some(Self::require_value(&mut args, arg)?);
                }
                "--arg" => {
                    let raw = args.next().ok_or_else(|| {
                        anyhow::anyhow!("Missing value for {arg} (expected KEY=VALUE)")
                    })?;
                    let Some((k, v)) = raw.split_once('=') else {
                        bail!("Invalid {arg} value '{raw}': expected KEY=VALUE");
                    };
                    let key = k.trim();
                    if key.is_empty() {
                        bail!("Invalid {arg} value '{raw}': argument name cannot be empty");
                    }
                    cli.skill_args.push((key.to_string(), v.to_string()));
                }
                "--session" => {
                    cli.session_id = Some(Self::require_value(&mut args, arg)?);
                }
                "-i" | "--image" => {
                    cli.images
                        .push(PathBuf::from(Self::require_value(&mut args, arg)?));
                }
                "--allowed-tools" | "--tools" => {
                    let raw = Self::require_value(&mut args, arg.as_str())?;
                    let list = cli.allowed_tools.get_or_insert_with(Vec::new);
                    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        let is_group_syntax =
                            item.strip_prefix('@').is_some_and(|g| !g.trim().is_empty());
                        if item != "*"
                            && !is_group_syntax
                            && !crate::tools::ALL_TOOL_NAMES.contains(&item)
                        {
                            bail!(
                                "Unknown tool '{item}' in {arg}. {}",
                                crate::tools::format_valid_tools_and_groups(&BTreeMap::new())
                            );
                        }
                        if !list.iter().any(|existing| existing == item) {
                            list.push(item.to_string());
                        }
                    }
                }
                "--no-tools" => cli.no_tools = true,
                "--no-save" | "--ephemeral" => cli.no_save = true,
                "-y" | "--yes" | "--auto-approve" => cli.auto_approve_all = true,
                "--thoughts" => cli.show_thoughts = true,
                _ if arg.starts_with('-') => bail!("Unknown flag: {arg}"),
                _ => positional.push(arg),
            }
        }

        // Backwards compatibility: if a single positional arg is passed with interactive stdin,
        // treat it as a prompt if it contains spaces or starts with `@`, otherwise treat as `<model>`.
        if cli.prompt.is_none() && !positional.is_empty() {
            if cli.model.is_none() && positional.len() == 1 && io::stdin().is_terminal() {
                if positional[0].contains(char::is_whitespace) || positional[0].starts_with('@') {
                    cli.prompt = Some(positional.remove(0));
                    cli.headless = true;
                } else {
                    cli.model = Some(positional.remove(0));
                }
            } else {
                cli.prompt = Some(positional.join(" "));
                cli.headless = true;
            }
        }

        // Read piped stdin if available
        if !io::stdin().is_terminal() {
            let mut piped = String::new();
            io::stdin().read_to_string(&mut piped)?;
            let piped = piped.trim();
            if !piped.is_empty() {
                cli.prompt = Some(match cli.prompt.take() {
                    Some(existing) => format!("{existing}\n\n{piped}"),
                    None => piped.to_string(),
                });
                cli.headless = true;
            }
        }

        Ok(cli)
    }

    fn require_value<I, F>(args: &mut I, flag: F) -> Result<String>
    where
        I: Iterator<Item = String>,
        F: AsRef<str>,
    {
        args.next()
            .ok_or_else(|| anyhow::anyhow!("Missing value for {}", flag.as_ref()))
    }

    pub fn parse_schedule_args<I>(mut args: I) -> Result<ScheduleCommand>
    where
        I: Iterator<Item = String>,
    {
        let Some(sub) = args.next() else {
            Self::print_schedule_help();
            std::process::exit(0);
        };

        if matches!(sub.as_str(), "-h" | "--help") {
            Self::print_schedule_help();
            std::process::exit(0);
        }

        let mut global = false;
        let mut lines = 50usize;
        let mut positional = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    Self::print_schedule_help();
                    std::process::exit(0);
                }
                "-g" | "--global" => global = true,
                "-n" | "--lines" => {
                    let val = Self::require_value(&mut args, arg.as_str())?;
                    lines = val
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid line count for {arg}: {val}"))?;
                }
                _ if arg.starts_with('-') => bail!("Unknown schedule flag: {arg}"),
                _ => positional.push(arg),
            }
        }

        match sub.as_str() {
            "sync" | "list" | "clean" => {
                if let Some(extra) = positional.first() {
                    bail!("Unexpected argument '{extra}' for `archon schedule {sub}`.");
                }
                match sub.as_str() {
                    "sync" => Ok(ScheduleCommand::Sync { global }),
                    "list" => Ok(ScheduleCommand::List { global }),
                    _ => Ok(ScheduleCommand::Clean { global }),
                }
            }
            "run" => {
                if positional.len() != 1 {
                    bail!("Usage: archon schedule run <NAME> [--global]");
                }
                Ok(ScheduleCommand::Run {
                    name: positional.remove(0),
                    global,
                })
            }
            "logs" => {
                if positional.len() > 1 {
                    bail!("Usage: archon schedule logs [NAME] [-n LINES] [--global]");
                }
                Ok(ScheduleCommand::Logs {
                    name: positional.pop(),
                    lines,
                    global,
                })
            }
            other => bail!("Unknown schedule subcommand: '{other}'. Run `archon schedule --help`."),
        }
    }

    pub fn parse_skill_args<I>(mut args: I) -> Result<SkillCommand>
    where
        I: Iterator<Item = String>,
    {
        let Some(sub) = args.next() else {
            Self::print_skill_help();
            std::process::exit(0);
        };

        if matches!(sub.as_str(), "-h" | "--help") {
            Self::print_skill_help();
            std::process::exit(0);
        }

        let mut dir: Option<PathBuf> = None;
        let mut global = false;
        let mut strict = false;
        let mut positional = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    Self::print_skill_help();
                    std::process::exit(0);
                }
                "-g" | "--global" => global = true,
                "-d" | "--dir" => {
                    let val = Self::require_value(&mut args, arg)?;
                    dir = Some(PathBuf::from(val));
                }
                "--strict" => strict = true,
                _ if arg.starts_with('-') => bail!("Unknown skill flag: {arg}"),
                _ => positional.push(arg),
            }
        }

        if dir.is_some() && global {
            bail!("`--dir` and `--global` cannot be used together.");
        }

        match sub.as_str() {
            "check" => {
                if positional.len() > 1 {
                    bail!("Usage: archon skill check [NAME] [--dir PATH] [--global] [--strict]");
                }
                Ok(SkillCommand::Check {
                    name: positional.pop(),
                    dir,
                    global,
                    strict,
                })
            }
            "list" => {
                if strict {
                    bail!("`--strict` is only valid with `archon skill check`.");
                }
                if let Some(extra) = positional.first() {
                    bail!("Unexpected argument '{extra}' for `archon skill list`.");
                }
                Ok(SkillCommand::List { dir, global })
            }
            other => bail!("Unknown skill subcommand: '{other}'. Run `archon skill --help`."),
        }
    }

    pub fn print_skill_help() {
        println!(
            "Usage: archon skill <COMMAND> [OPTIONS]\n\n\
             Commands:\n  \
               check [NAME]            Validate SKILL.md frontmatter, extends, tools, hook schemas, and args\n  \
               list                    List loaded skills with inheritance, typed args, tools, and hooks\n\n\
             Options:\n  \
               -d, --dir <PATH>        Validate or list skills from a specific directory\n  \
               -g, --global            Operate only on global ~/.config/archon/skills\n  \
               --strict                Treat warnings as errors during `check`\n  \
               -h, --help              Print help"
        );
    }

    pub fn print_schedule_help() {
        println!(
            "Usage: archon schedule <COMMAND> [OPTIONS]\n\n\
             Commands:\n  \
               sync                    Generate and sync systemd user units from config.toml\n  \
               list                    List configured schedules and active systemd timers\n  \
               run <NAME>              Trigger a scheduled service immediately\n  \
               logs [NAME]             Show journalctl logs for a schedule (or all in scope)\n  \
               clean                   Disable and remove all managed units for this scope\n\n\
             Options:\n  \
               -g, --global            Operate on global ~/.config/archon/config.toml schedules\n  \
               -n, --lines <N>         Number of log lines for `logs` (default: 50)\n  \
               -h, --help              Print help"
        );
    }

    pub fn print_help() {
        println!(
            "Usage: archon [OPTIONS] [MODEL]\n       archon schedule <sync|list|run|logs|clean> [OPTIONS]\n       archon skill <check|list> [OPTIONS]\n\n\
             Options:\n  \
               -p, --prompt <TEXT>     Run in headless CLI mode with the given prompt\n  \
               -m, --model <MODEL>     Model name to use\n  \
               -s, --system <PROMPT>   Custom base system prompt\n  \
               --skill <NAME>          Activate a skill for the turn\n  \
               --arg <KEY=VALUE>       Pass a typed skill argument (repeatable)\n  \
               --session <ID>          Resume an existing session ID\n  \
               -i, --image <PATH>      Attach an image file (repeatable)\n  \
               -y, --yes               Auto-approve all tool executions in headless mode\n  \
               --allowed-tools <LIST>  Restrict enabled tools (names or @groups, comma-separated)\n  \
               --no-tools              Disable tool calls\n  \
               --no-save               Do not persist the session to archon.db\n  \
               --thoughts              Stream model thinking to stderr\n  \
               -h, --help              Print help"
        );
    }
}

pub struct CliApp {
    args: CliArgs,
    model: String,
    config: Config,
    cmd_tx: UnboundedSender<EngineCommand>,
}

impl CliApp {
    pub fn new(
        args: CliArgs,
        model: String,
        config: Config,
        cmd_tx: UnboundedSender<EngineCommand>,
    ) -> Self {
        Self {
            args,
            model,
            config,
            cmd_tx,
        }
    }

    pub async fn run(mut self, mut ev_rx: UnboundedReceiver<EngineEvent>) -> Result<()> {
        let raw_prompt = self
            .args
            .prompt
            .take()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("No prompt provided for headless CLI mode"))?;

        let enable_tools = !self.args.no_tools;
        let mut turn = TurnConfig {
            system_prompt: self.args.system_prompt.clone().unwrap_or_default(),
            num_ctx: self.config.context_for_model(&self.model),
            enable_tools,
            active_skill: None,
            skill_args: None,
            raw_skill_args: None,
            skill_inputs: BTreeMap::new(),
        };

        // Resolve explicit `--skill` or leading `@skill` in the prompt
        let prompt = if let Some(ref explicit_skill) = self.args.skill {
            skills::activate_skill_for_turn(
                explicit_skill,
                &raw_prompt,
                &self.args.skill_args,
                &mut turn,
                &self.config,
            )
            .map_err(|e| anyhow::anyhow!(e))?
        } else if let Some((cmd_token, args_str)) = skills::parse_at_command(&raw_prompt) {
            let p = skills::activate_skill_for_turn(
                cmd_token,
                args_str,
                &self.args.skill_args,
                &mut turn,
                &self.config,
            )
            .map_err(|e| anyhow::anyhow!(e))?;
            if args_str.is_empty() && self.args.skill_args.is_empty() {
                raw_prompt.trim().to_string()
            } else {
                p
            }
        } else {
            if !self.args.skill_args.is_empty() {
                bail!("`--arg` requires `--skill <NAME>` or a leading `@skill` in the prompt.");
            }
            raw_prompt
        };

        // 1. Load existing session if requested
        if let Some(ref session_id) = self.args.session_id {
            self.cmd_tx
                .send(EngineCommand::LoadSession(session_id.clone()))?;
            while let Some(ev) = ev_rx.recv().await {
                match ev {
                    EngineEvent::ChatUpdated { .. } => break,
                    EngineEvent::Error(e) => bail!("{e}"),
                    _ => {}
                }
            }
        }

        // 2. Stage any attached images before sending the prompt
        for img_path in &self.args.images {
            self.cmd_tx
                .send(EngineCommand::ImportFile(img_path.clone()))?;
            while let Some(ev) = ev_rx.recv().await {
                if matches!(ev, EngineEvent::AttachmentsUpdated(_)) {
                    break;
                }
            }
        }

        // 3. Dispatch the user prompt
        self.cmd_tx.send(EngineCommand::SendPrompt {
            prompt,
            turn: turn.clone(),
        })?;

        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        let mut printed_thoughts = false;
        let mut printed_text = false;

        let close_thoughts = |err: &mut io::Stderr, flag: &mut bool| {
            if *flag {
                let _ = writeln!(err, "\x1b[0m");
                *flag = false;
            }
        };

        // 4. Process engine events until the final StreamDone (with no pending tool calls)
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                EngineEvent::StreamThoughtChunk(chunk) if self.args.show_thoughts => {
                    if !printed_thoughts {
                        let _ = write!(stderr, "\x1b[2m[thinking] ");
                        printed_thoughts = true;
                    }
                    let _ = write!(stderr, "{chunk}");
                    let _ = stderr.flush();
                }
                EngineEvent::StreamChunk(chunk) => {
                    close_thoughts(&mut stderr, &mut printed_thoughts);
                    printed_text = true;
                    write!(stdout, "{chunk}")?;
                    stdout.flush()?;
                }
                EngineEvent::ToolStarted {
                    summary,
                    index,
                    total,
                } => {
                    close_thoughts(&mut stderr, &mut printed_thoughts);
                    if printed_text {
                        writeln!(stdout)?;
                        stdout.flush()?;
                        printed_text = false;
                    }
                    let progress = if total > 1 {
                        format!(" ({index}/{total})")
                    } else {
                        String::new()
                    };
                    let _ = writeln!(stderr, "\x1b[33m⚙ {summary}{progress}\x1b[0m");
                }
                EngineEvent::ToolApprovalRequested {
                    formatted_preview,
                    index,
                    total,
                } => {
                    close_thoughts(&mut stderr, &mut printed_thoughts);

                    if self.args.auto_approve_all {
                        let _ = writeln!(
                            stderr,
                            "\x1b[32m✓ Auto-approved tool ({index}/{total})\x1b[0m"
                        );
                        self.cmd_tx.send(EngineCommand::ResolveTool {
                            approved: true,
                            always_approve: true,
                            feedback: String::new(),
                            turn: turn.clone(),
                        })?;
                    } else {
                        match prompt_tool_approval_tty(formatted_preview, index, total).await {
                            TtyApprovalDecision::CancelAll => {
                                let _ =
                                    writeln!(stderr, "\x1b[31m✗ Cancelled tool execution.\x1b[0m");
                                self.cmd_tx.send(EngineCommand::CancelStream)?;
                                break;
                            }
                            TtyApprovalDecision::Resolve {
                                approved,
                                always_approve,
                                feedback,
                            } => {
                                self.cmd_tx.send(EngineCommand::ResolveTool {
                                    approved,
                                    always_approve,
                                    feedback,
                                    turn: turn.clone(),
                                })?;
                            }
                        }
                    }
                }
                EngineEvent::TruncatedToolResolutionRequested {
                    summary,
                    raw_bytes,
                    max_size,
                    ..
                } => {
                    let _ = writeln!(
                        stderr,
                        "\x1b[33m⚠ Output from '{summary}' ({raw_bytes} B) exceeded {max_size} B; proceeding with truncated output.\x1b[0m"
                    );
                    self.cmd_tx.send(EngineCommand::ResolveTruncatedTool {
                        action: TruncatedToolAction::ProceedTruncated,
                        guide_prompt: String::new(),
                        turn: turn.clone(),
                    })?;
                }
                EngineEvent::StreamDone { tool_calls, .. } => {
                    close_thoughts(&mut stderr, &mut printed_thoughts);
                    if printed_text {
                        writeln!(stdout)?;
                        stdout.flush()?;
                        printed_text = false;
                    }
                    // If the model requested tool calls, CoreEngine will automatically
                    // process them and start the next model turn. Only exit when done.
                    if tool_calls.is_empty() {
                        break;
                    }
                }
                EngineEvent::StreamError(err) | EngineEvent::Error(err) => {
                    close_thoughts(&mut stderr, &mut printed_thoughts);
                    bail!("{err}");
                }
                _ => {}
            }
        }

        // Dropping `self` drops `cmd_tx`, signaling `CoreEngine::run` to flush SQLite saves and exit.
        Ok(())
    }
}

enum TtyApprovalDecision {
    Resolve {
        approved: bool,
        always_approve: bool,
        feedback: String,
    },
    CancelAll,
}

async fn prompt_tool_approval_tty(
    formatted_preview: String,
    index: usize,
    total: usize,
) -> TtyApprovalDecision {
    tokio::task::spawn_blocking(move || {
        let mut stderr = io::stderr();
        let _ = writeln!(
            stderr,
            "\n\x1b[33;1m⚠ Tool Execution Approval Required ({index}/{total}):\x1b[0m\n{formatted_preview}\n"
        );

        let Ok(tty) = File::open("/dev/tty") else {
            let _ = writeln!(
                stderr,
                "\x1b[31mNo controlling terminal (/dev/tty) available; declining tool call.\x1b[0m"
            );
            return TtyApprovalDecision::Resolve {
                approved: false,
                always_approve: false,
                feedback: String::new(),
            };
        };

        let _ = write!(
            stderr,
            "\x1b[33mApprove? [Y]es / [a]lways / [n]o / [c]ancel (optional `: <feedback>`): \x1b[0m"
        );
        let _ = stderr.flush();

        let mut reader = BufReader::new(tty);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return TtyApprovalDecision::Resolve {
                approved: false,
                always_approve: false,
                feedback: String::new(),
            };
        }

        let trimmed = line.trim();
        let (choice, feedback) = match trimmed.split_once(':') {
            Some((c, fb)) => (c.trim().to_ascii_lowercase(), fb.trim().to_string()),
            None => (trimmed.to_ascii_lowercase(), String::new()),
        };

        match choice.as_str() {
            "" | "y" | "yes" => TtyApprovalDecision::Resolve {
                approved: true,
                always_approve: false,
                feedback,
            },
            "a" | "always" => TtyApprovalDecision::Resolve {
                approved: true,
                always_approve: true,
                feedback,
            },
            "c" | "cancel" => TtyApprovalDecision::CancelAll,
            _ => TtyApprovalDecision::Resolve {
                approved: false,
                always_approve: false,
                feedback,
            },
        }
    })
    .await
    .unwrap_or(TtyApprovalDecision::Resolve {
        approved: false,
        always_approve: false,
        feedback: String::new(),
    })
}
