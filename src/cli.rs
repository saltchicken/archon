use crate::core::{
    EngineCommand, EngineEvent, TruncatedToolAction, TurnConfig, config::Config, skills,
};
use anyhow::{Result, bail};
use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    path::PathBuf,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug, Default)]
pub struct CliArgs {
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub system_prompt: Option<String>,
    pub skill: Option<String>,
    pub session_id: Option<String>,
    pub images: Vec<PathBuf>,
    pub no_tools: bool,
    pub auto_approve_all: bool,
    pub show_thoughts: bool,
    pub headless: bool,
}

impl CliArgs {
    pub fn parse() -> Result<Self> {
        let mut cli = Self::default();
        let mut args = env::args().skip(1).peekable();
        let mut positional = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    Self::print_help();
                    std::process::exit(0);
                }
                "-m" | "--model" => {
                    cli.model = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for {arg}"))?,
                    );
                }
                "-p" | "--prompt" => {
                    cli.prompt = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for {arg}"))?,
                    );
                    cli.headless = true;
                }
                "-s" | "--system" => {
                    cli.system_prompt = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for {arg}"))?,
                    );
                }
                "--skill" => {
                    cli.skill = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for {arg}"))?,
                    );
                }
                "--session" => {
                    cli.session_id = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for {arg}"))?,
                    );
                }
                "-i" | "--image" => {
                    let p = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("Missing value for {arg}"))?;
                    cli.images.push(PathBuf::from(p));
                }
                "--no-tools" => cli.no_tools = true,
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

    pub fn print_help() {
        println!(
            "Usage: archon [OPTIONS] [MODEL]\n\n\
             Options:\n  \
               -p, --prompt <TEXT>     Run in headless CLI mode with the given prompt\n  \
               -m, --model <MODEL>     Model name to use\n  \
               -s, --system <PROMPT>   Custom base system prompt\n  \
               --skill <NAME>          Activate a skill for the turn\n  \
               --session <ID>          Resume an existing session ID\n  \
               -i, --image <PATH>      Attach an image file (repeatable)\n  \
               -y, --yes               Auto-approve all tool executions in headless mode\n  \
               --no-tools              Disable tool calls\n  \
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
        };

        // Resolve explicit `--skill` or leading `@skill` in the prompt
        let prompt = if let Some(ref explicit_skill) = self.args.skill {
            let name = skills::check_skill_activation(explicit_skill, enable_tools, &self.config)
                .map_err(|e| anyhow::anyhow!(e))?;
            turn.active_skill = Some(name);
            turn.skill_args = Some(raw_prompt.clone());
            raw_prompt
        } else if let Some((cmd_token, args_str)) = skills::parse_at_command(&raw_prompt) {
            let name = skills::check_skill_activation(cmd_token, enable_tools, &self.config)
                .map_err(|e| anyhow::anyhow!(e))?;
            turn.active_skill = Some(name);
            turn.skill_args = (!args_str.is_empty()).then(|| args_str.to_string());
            if args_str.is_empty() {
                raw_prompt
            } else {
                args_str.to_string()
            }
        } else {
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
                    if printed_thoughts {
                        let _ = writeln!(stderr, "\x1b[0m");
                        printed_thoughts = false;
                    }
                    printed_text = true;
                    write!(stdout, "{chunk}")?;
                    stdout.flush()?;
                }
                EngineEvent::ToolStarted {
                    summary,
                    index,
                    total,
                } => {
                    if printed_thoughts {
                        let _ = writeln!(stderr, "\x1b[0m");
                        printed_thoughts = false;
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
                    if printed_thoughts {
                        let _ = writeln!(stderr, "\x1b[0m");
                        printed_thoughts = false;
                    }

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
                    if printed_thoughts {
                        let _ = writeln!(stderr, "\x1b[0m");
                        printed_thoughts = false;
                    }
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
                    if printed_thoughts {
                        let _ = writeln!(stderr, "\x1b[0m");
                    }
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
            "\x1b[33mApprove? [↵]/y]es / [a]lways / [n]o / [c]ancel (optional `: <feedback>`): \x1b[0m"
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
