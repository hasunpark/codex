use std::io::IsTerminal;
use std::path::Path;

use anyhow::anyhow;
use clap::Parser;
use codex_common::CliConfigOverrides;
use codex_core::AuthManager;
use codex_core::ConversationManager;
use codex_core::NewConversation;
use codex_core::RolloutRecorder;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::find_conversation_path_by_id_str;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Submission;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tracing::error;
use tracing::info;

#[derive(Debug, Parser)]
pub struct ProtoCli {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,
}

#[derive(Debug)]
pub struct ProtoResumeOpts {
    pub config_overrides: CliConfigOverrides,
    pub session_id: Option<String>,
    pub last: bool,
}

pub async fn run_main(opts: ProtoCli) -> anyhow::Result<()> {
    run_proto(opts.config_overrides, ConversationSource::New).await
}

pub async fn run_resume(opts: ProtoResumeOpts) -> anyhow::Result<()> {
    run_proto(
        opts.config_overrides,
        ConversationSource::Resume {
            session_id: opts.session_id,
            last: opts.last,
        },
    )
    .await
}

#[derive(Debug)]
enum ConversationSource {
    New,
    Resume {
        session_id: Option<String>,
        last: bool,
    },
}

async fn run_proto(
    config_overrides: CliConfigOverrides,
    source: ConversationSource,
) -> anyhow::Result<()> {
    if std::io::stdin().is_terminal() {
        anyhow::bail!("Protocol mode expects stdin to be a pipe, not a terminal");
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let overrides_vec = config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;

    let config = Config::load_with_cli_overrides(overrides_vec, ConfigOverrides::default())?;
    let codex_home = config.codex_home.clone();
    let auth_manager = AuthManager::shared(codex_home.clone());
    let conversation_manager = ConversationManager::new(auth_manager.clone());
    let NewConversation {
        conversation_id: _,
        conversation,
        session_configured,
    } = match source {
        ConversationSource::New => conversation_manager.new_conversation(config).await?,
        ConversationSource::Resume { session_id, last } => {
            if !last && session_id.is_none() {
                anyhow::bail!(
                    "Protocol resume requires either a SESSION_ID argument or the --last flag"
                );
            }

            let resume_path = resolve_resume_path(&codex_home, session_id.as_deref(), last).await?;
            let resume_path = match resume_path {
                Some(path) => path,
                None => {
                    if let Some(id) = session_id {
                        anyhow::bail!("No recorded session found for id {id}");
                    } else {
                        let sessions_dir = codex_home.join("sessions");
                        anyhow::bail!(
                            "No recorded sessions found under {}",
                            sessions_dir.display()
                        );
                    }
                }
            };

            conversation_manager
                .resume_conversation_from_rollout(config, resume_path, auth_manager.clone())
                .await?
        }
    };

    // Simulate streaming the session_configured event.
    let synthetic_event = Event {
        // Fake id value.
        id: "".to_string(),
        msg: EventMsg::SessionConfigured(session_configured),
    };
    let session_configured_event = match serde_json::to_string(&synthetic_event) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to serialize session_configured: {e}");
            return Err(anyhow::Error::from(e));
        }
    };
    println!("{session_configured_event}");

    // Task that reads JSON lines from stdin and forwards to Submission Queue
    let sq_fut = {
        let conversation = conversation.clone();
        async move {
            let stdin = BufReader::new(tokio::io::stdin());
            let mut lines = stdin.lines();
            loop {
                let result = tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        break
                    },
                    res = lines.next_line() => res,
                };

                match result {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Submission>(line) {
                            Ok(sub) => {
                                if let Err(e) = conversation.submit_with_id(sub).await {
                                    error!("{e:#}");
                                    break;
                                }
                            }
                            Err(e) => {
                                error!("invalid submission: {e}");
                            }
                        }
                    }
                    _ => {
                        info!("Submission queue closed");
                        break;
                    }
                }
            }
        }
    };

    // Task that reads events from the agent and prints them as JSON lines to stdout
    let eq_fut = async move {
        loop {
            let event = tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                event = conversation.next_event() => event,
            };
            match event {
                Ok(event) => {
                    let event_str = match serde_json::to_string(&event) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Failed to serialize event: {e}");
                            continue;
                        }
                    };
                    println!("{event_str}");
                }
                Err(e) => {
                    error!("{e:#}");
                    break;
                }
            }
        }
        info!("Event queue closed");
    };

    tokio::join!(sq_fut, eq_fut);
    Ok(())
}

async fn resolve_resume_path(
    codex_home: &Path,
    session_id: Option<&str>,
    last: bool,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    if last {
        let page = RolloutRecorder::list_conversations(codex_home, 1, None)
            .await
            .map_err(|e| anyhow!("failed to list recorded sessions: {e}"))?;
        Ok(page.items.first().map(|it| it.path.clone()))
    } else if let Some(id_str) = session_id {
        let path = find_conversation_path_by_id_str(codex_home, id_str)
            .await
            .map_err(|e| anyhow!("failed to locate recorded session: {e}"))?;
        Ok(path)
    } else {
        Ok(None)
    }
}
