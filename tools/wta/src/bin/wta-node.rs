use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser, Debug)]
#[command(
    name = "wta-node",
    version,
    about = "Intelligent Terminal remote compute/session runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<NodeCommand>,
}

#[derive(Subcommand, Debug)]
enum NodeCommand {
    /// Proxy line-delimited JSON-RPC over stdin/stdout
    Bridge,
    /// Print the versioned node handshake
    Status,
    /// Check state and local tool availability
    Doctor,
    /// Compute a SHA-256 digest without invoking a shell
    Hash { path: PathBuf },
    /// Run the persistent per-user node daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Start, attach, detach, inspect or stop persistent ACP adapter sessions.
    Acp {
        #[command(subcommand)]
        action: AcpAction,
    },
    /// Start, attach, resize, inspect or stop persistent terminal PTYs.
    Pty {
        #[command(subcommand)]
        action: PtyAction,
    },
    /// Prepare, verify and atomically activate remote file uploads.
    File {
        #[command(subcommand)]
        action: FileAction,
    },
    /// Publish a capability-scoped event to the local Intelligent Terminal.
    Relay {
        #[command(subcommand)]
        action: RelayAction,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Serve the private local IPC socket in the foreground.
    Serve,
}

#[derive(Subcommand, Debug)]
enum AcpAction {
    /// Start the adapter when absent, then attach inherited stdio.
    Start {
        #[arg(long)]
        session: String,
        #[arg(required = true, trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Attach inherited stdio to an already-running adapter.
    Attach {
        #[arg(long)]
        session: String,
    },
    /// Disconnect the current attachment without stopping the adapter.
    Detach {
        #[arg(long)]
        session: String,
    },
    /// Stop the adapter process group and detach its client.
    Stop {
        #[arg(long)]
        session: String,
    },
    /// Print every persistent remote session known to the daemon.
    List,
}

#[derive(Subcommand, Debug)]
enum PtyAction {
    Start {
        #[arg(long)]
        session: String,
        #[arg(long, default_value = "120")]
        cols: u16,
        #[arg(long, default_value = "30")]
        rows: u16,
        #[arg(required = true, trailing_var_arg = true)]
        argv: Vec<String>,
    },
    Attach {
        #[arg(long)]
        session: String,
        #[arg(long, default_value = "120")]
        cols: u16,
        #[arg(long, default_value = "30")]
        rows: u16,
    },
    Resize {
        #[arg(long)]
        session: String,
        #[arg(long)]
        attachment: String,
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
    },
    Status {
        #[arg(long)]
        session: String,
    },
    Stop {
        #[arg(long)]
        session: String,
    },
    List,
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        session: String,
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
        #[arg(required = true, trailing_var_arg = true)]
        argv: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum FileAction {
    // Remote File Explorer operations are intentionally available only through
    // the scoped JSON-RPC bridge. Keeping raw path/root CLI variants here would
    // bypass workspace policy and make revocation ineffective.
    PrepareUpload {
        #[arg(long)]
        transfer: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        size: u64,
        #[arg(long)]
        sha256: String,
    },
    /// Receive the prepared payload from stdin without exposing a remote path.
    ReceiveUpload {
        #[arg(long)]
        transfer: String,
    },
    CommitUpload {
        #[arg(long)]
        transfer: String,
    },
    AbortUpload {
        #[arg(long)]
        transfer: String,
    },
    /// Stream the immutable prepared download snapshot to stdout.
    StreamDownload {
        #[arg(long)]
        transfer: String,
    },
    List,
}

#[derive(Subcommand, Debug)]
enum RelayAction {
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        #[arg(long, default_value = "info")]
        level: String,
    },
    Status {
        #[arg(long)]
        state: String,
        #[arg(long)]
        detail: Option<String>,
    },
    Progress {
        #[arg(long)]
        fraction: f64,
        #[arg(long)]
        label: Option<String>,
    },
    Focus {
        #[arg(long)]
        reason: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command.unwrap_or(NodeCommand::Bridge) {
        NodeCommand::Bridge => wta::compute::node::bridge_stdio().await,
        NodeCommand::Status => {
            println!(
                "{}",
                serde_json::to_string_pretty(&wta::compute::node::handshake()?)?
            );
            Ok(())
        }
        NodeCommand::Doctor => {
            println!(
                "{}",
                serde_json::to_string_pretty(&wta::compute::node::doctor()?)?
            );
            Ok(())
        }
        NodeCommand::Hash { path } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "path": path,
                    "sha256": wta::compute::snapshot::sha256_file(&path)?,
                }))?
            );
            Ok(())
        }
        NodeCommand::Daemon {
            action: DaemonAction::Serve,
        } => wta::compute::session::daemon_serve().await,
        NodeCommand::Acp { action } => match action {
            AcpAction::Start { session, argv } => {
                wta::compute::session::acp_start(&session, &argv).await
            }
            AcpAction::Attach { session } => wta::compute::session::acp_attach(&session).await,
            AcpAction::Detach { session } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &wta::compute::session::acp_detach(&session).await?
                    )?
                );
                Ok(())
            }
            AcpAction::Stop { session } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &wta::compute::session::acp_stop(&session).await?
                    )?
                );
                Ok(())
            }
            AcpAction::List => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wta::compute::session::acp_list().await?)?
                );
                Ok(())
            }
        },
        NodeCommand::Pty { action } => match action {
            PtyAction::Start {
                session,
                cols,
                rows,
                argv,
            } => wta::compute::pty::start_and_attach(&session, &argv, cols, rows).await,
            PtyAction::Attach {
                session,
                cols,
                rows,
            } => wta::compute::pty::attach_client(&session, cols, rows).await,
            PtyAction::Resize {
                session,
                attachment,
                cols,
                rows,
            } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &wta::compute::pty::resize_client(&session, &attachment, cols, rows)
                            .await?
                    )?
                );
                Ok(())
            }
            PtyAction::Status { session } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wta::compute::pty::status(&session).await?)?
                );
                Ok(())
            }
            PtyAction::Stop { session } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wta::compute::pty::stop(&session).await?)?
                );
                Ok(())
            }
            PtyAction::List => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wta::compute::pty::list()?)?
                );
                Ok(())
            }
            PtyAction::Serve {
                session,
                cols,
                rows,
                argv,
            } => wta::compute::pty::serve(&session, &argv, cols, rows).await,
        },
        NodeCommand::File { action } => {
            if let FileAction::StreamDownload { transfer } = &action {
                return wta::compute::transfer::stream_download(transfer);
            }
            let value = match action {
                FileAction::PrepareUpload {
                    transfer,
                    name,
                    size,
                    sha256,
                } => serde_json::to_value(wta::compute::transfer::prepare_upload(
                    &transfer, &name, size, &sha256,
                )?)?,
                FileAction::ReceiveUpload { transfer } => {
                    wta::compute::transfer::receive_upload(&transfer)?
                }
                FileAction::CommitUpload { transfer } => {
                    serde_json::to_value(wta::compute::transfer::commit_upload(&transfer)?)?
                }
                FileAction::AbortUpload { transfer } => {
                    wta::compute::transfer::abort_upload(&transfer)?
                }
                FileAction::StreamDownload { .. } => unreachable!(),
                FileAction::List => {
                    serde_json::to_value(wta::compute::transfer::list_node_uploads()?)?
                }
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        NodeCommand::Relay { action } => {
            let token = std::env::var("WTA_RELAY_TOKEN")
                .map_err(|_| anyhow::anyhow!("WTA_RELAY_TOKEN is required"))?;
            let workspace_id = std::env::var("WTA_WORKSPACE_ID")
                .map_err(|_| anyhow::anyhow!("WTA_WORKSPACE_ID is required"))?;
            let surface_id = std::env::var("WTA_SURFACE_ID").ok();
            let scope = json!({
                "workspace_id": workspace_id,
                "surface_id": surface_id,
            });
            let authorization = json!({
                "token": token,
                "nonce": uuid::Uuid::new_v4().to_string(),
            });
            let (method, mut params) = match action {
                RelayAction::Notify { title, body, level } => (
                    "relay.notify",
                    json!({
                        "scope": scope,
                        "authorization": authorization,
                        "title": title,
                        "body": body,
                        "level": level,
                        "metadata": {},
                    }),
                ),
                RelayAction::Status { state, detail } => (
                    "relay.status",
                    json!({
                        "scope": scope,
                        "authorization": authorization,
                        "state": state,
                        "detail": detail,
                        "metadata": {},
                    }),
                ),
                RelayAction::Progress { fraction, label } => (
                    "relay.progress",
                    json!({
                        "scope": scope,
                        "authorization": authorization,
                        "fraction": fraction,
                        "label": label,
                        "metadata": {},
                    }),
                ),
                RelayAction::Focus { reason } => (
                    "relay.focus",
                    json!({
                        "scope": scope,
                        "authorization": authorization,
                        "reason": reason,
                    }),
                ),
            };
            let value = wta::compute::session::relay_dispatch(method, &params).await?;
            // Drop the capability-bearing request before formatting output.
            params = serde_json::Value::Null;
            drop(params);
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}
