use anyhow::{Result, anyhow};
use brainrot_tg_llm_rewrite::app::{init_tracing, run_rewrite_mode};
use brainrot_tg_llm_rewrite::config::{Config, ConfigMode, load_config_for_mode};
use brainrot_tg_llm_rewrite::telegram::TelegramBot;
use clap::{ArgAction, Parser};
use std::ffi::OsString;
use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppMode {
    Rewrite,
    ListChats { query: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppArgs {
    config_path: PathBuf,
    mode: AppMode,
}

#[derive(Debug, Parser)]
#[command(name = "brainrot_tg_llm_rewrite")]
#[command(about = "Telegram userbot rewriter with optional chat listing mode")]
struct Cli {
    #[arg(long, value_name = "path", default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    #[arg(long, action = ArgAction::SetTrue)]
    list_chats: bool,
    #[arg(value_name = "query", requires = "list_chats")]
    query: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args = parse_args()?;
    let config_mode = match args.mode {
        AppMode::Rewrite => ConfigMode::Rewrite,
        AppMode::ListChats { .. } => ConfigMode::ListChats,
    };
    let config = load_config_for_mode(&args.config_path, config_mode)?;

    match args.mode {
        AppMode::ListChats { query } => run_list_mode(&config, query.as_deref()).await,
        AppMode::Rewrite => run_rewrite_mode(&config, &args.config_path).await,
    }
}

async fn run_list_mode(config: &Config, query: Option<&str>) -> Result<()> {
    let mut bot = TelegramBot::connect_for_listing(&config.telegram).await?;
    let chats = bot.list_chats(query).await?;

    if chats.is_empty() {
        if let Some(query) = query {
            println!("No chats matched filter: {query}");
        } else {
            println!("No chats found.");
        }
    } else {
        for chat in chats {
            println!("{}\t{}", chat.id, chat.name);
        }
    }

    bot.shutdown().await?;
    Ok(())
}

fn parse_args() -> Result<AppArgs> {
    parse_args_from(std::env::args_os())
}

fn parse_args_from<I, S>(args: I) -> Result<AppArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).map_err(|error| anyhow!(error.to_string()))?;
    let mode = if cli.list_chats {
        AppMode::ListChats { query: cli.query }
    } else {
        AppMode::Rewrite
    };

    Ok(AppArgs {
        config_path: cli.config,
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::{AppMode, parse_args_from};
    use std::path::PathBuf;

    #[test]
    fn parse_list_chats_without_query() {
        let parsed = parse_args_from(["brainrot_tg_llm_rewrite", "--list-chats"])
            .expect("parsing should succeed");
        assert_eq!(parsed.mode, AppMode::ListChats { query: None });
    }

    #[test]
    fn parse_list_chats_with_query() {
        let parsed = parse_args_from(["brainrot_tg_llm_rewrite", "--list-chats", "work"])
            .expect("parsing should succeed");
        assert_eq!(
            parsed.mode,
            AppMode::ListChats {
                query: Some("work".to_string()),
            }
        );
    }

    #[test]
    fn parse_config_path() {
        let parsed = parse_args_from(["brainrot_tg_llm_rewrite", "--config", "custom.toml"])
            .expect("parsing should succeed");
        assert_eq!(parsed.config_path, PathBuf::from("custom.toml"));
        assert_eq!(parsed.mode, AppMode::Rewrite);
    }

    #[test]
    fn parse_missing_config_path_fails() {
        let err = parse_args_from(["brainrot_tg_llm_rewrite", "--config"])
            .expect_err("parsing should fail");
        assert!(err.to_string().contains("--config"));
    }

    #[test]
    fn parse_unknown_flag_fails() {
        let err =
            parse_args_from(["brainrot_tg_llm_rewrite", "--wat"]).expect_err("parsing should fail");
        assert!(err.to_string().contains("--wat"));
    }

    #[test]
    fn parse_config_then_list_mode_with_query() {
        let parsed = parse_args_from([
            "brainrot_tg_llm_rewrite",
            "--config",
            "x.toml",
            "--list-chats",
            "team",
        ])
        .expect("parsing should succeed");
        assert_eq!(parsed.config_path, PathBuf::from("x.toml"));
        assert_eq!(
            parsed.mode,
            AppMode::ListChats {
                query: Some("team".to_string()),
            }
        );
    }

    #[test]
    fn parse_list_mode_then_config_without_query() {
        let parsed = parse_args_from([
            "brainrot_tg_llm_rewrite",
            "--list-chats",
            "--config",
            "x.toml",
        ])
        .expect("parsing should succeed");
        assert_eq!(parsed.config_path, PathBuf::from("x.toml"));
        assert_eq!(parsed.mode, AppMode::ListChats { query: None });
    }

    #[test]
    fn parse_query_without_list_mode_fails() {
        let err =
            parse_args_from(["brainrot_tg_llm_rewrite", "work"]).expect_err("parsing should fail");
        assert!(err.to_string().contains("--list-chats"));
    }
}
