use notify::{event::CreateKind, RecursiveMode, Watcher};
use once_cell::sync::OnceCell;
use serde::Deserialize;
use std::path::Path;
use teloxide::dispatching::Dispatcher;
use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::*;
use teloxide::types::Message;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::channel;

#[derive(Deserialize)]
struct Config {
    sms: SmsConfig,
    bot: BotConfig,
}

#[derive(Deserialize)]
struct SmsConfig {
    path: String,
    history_path: String,
}

#[derive(Deserialize)]
struct BotConfig {
    token: String,
    admin_id: i64,
}

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "Show the last N lines of the history file.")]
    Tail(String),
    #[command(description = "Display this text.")]
    Help,
}

static CURRENT_CONFIG: OnceCell<Config> = OnceCell::new();

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config_content = String::new();
    File::open("config.toml")
        .await?
        .read_to_string(&mut config_content)
        .await?;
    let config: Config = toml::from_str(&config_content)?;

    let config = CURRENT_CONFIG.get_or_init(|| config);

    let bot = Bot::new(config.bot.token.as_str());

    // Set up command handling
    let cmd_handler = Update::filter_message()
        .filter(|msg: Message| {
            msg.from
                .map(|user| user.id.0 as i64 == config.bot.admin_id)
                .unwrap_or(false)
        })
        .filter_command::<Command>()
        .branch(dptree::case![Command::Tail(n)].endpoint(tail_handler))
        .branch(dptree::case![Command::Help].endpoint(help_handler));

    let (tx, mut rx) = channel(10);
    let mut watcher = notify::recommended_watcher(move |res| match res {
        Ok(event) => {
            if let Err(e) = tx.clone().blocking_send(event) {
                eprintln!("Failed to send event: {:?}", e);
            }
        }
        Err(e) => eprintln!("watch error: {:?}", e),
    })?;
    watcher.watch(Path::new(&config.sms.path), RecursiveMode::NonRecursive)?;

    let app = Box::leak(Box::new(
        Dispatcher::builder(bot.clone(), cmd_handler)
            .enable_ctrlc_handler()
            .build(),
    ));
    // Spawn the dispatcher
    tokio::spawn(app.dispatch());

    println!("Watching for new SMS files in {}", config.sms.path);

    // Handle file events
    while let Some(e) = rx.recv().await {
        if let notify::EventKind::Create(CreateKind::File) = e.kind {
            if let Some(path_buf) = e.paths.first().cloned() {
                let bot = bot.clone();
                let admin_id = config.bot.admin_id;
                tokio::spawn(async move {
                    let mut file = File::open(&path_buf).await.expect("Failed to open file");
                    let mut contents = String::new();
                    if let Err(e) = file.read_to_string(&mut contents).await {
                        eprintln!("Failed to read file: {:?}", e);
                    } else if let Err(e) = bot
                        .send_message(ChatId(admin_id), highlight_sms_codes(&contents))
                        .parse_mode(ParseMode::MarkdownV2)
                        .await
                    {
                        eprintln!("Failed to send message: {:?}", e);
                    }
                    if let Err(e) = tokio::fs::remove_file(&path_buf).await {
                        eprintln!("Failed to delete file: {:?}", e);
                    }
                });
            }
        }
    }

    Ok(())
}

async fn tail_handler(bot: Bot, message: Message, command: Command) -> ResponseResult<()> {
    let config = CURRENT_CONFIG.get().unwrap();
    if let Command::Tail(n_str) = command {
        // Parse N
        let n: usize = n_str.parse().unwrap_or(50);

        // Read the last N lines from history_path
        match read_last_n_lines(&config.sms.history_path, n).await {
            Ok(lines) => {
                let lines_vec: Vec<&str> = lines.split('\n').collect();
                for chunk in lines_vec.chunks(50) {
                    let chunk_str = chunk.join("\n");
                    let sent = bot
                        .send_message(message.chat.id, format!("```\n{}\n```", chunk_str))
                        .parse_mode(ParseMode::MarkdownV2)
                        .await;
                    if let Err(e) = sent {
                        eprintln!("Failed to send message: {:?}", e);
                    }
                }
            }
            Err(e) => {
                bot.send_message(message.chat.id, format!("Error: {}", e))
                    .await?;
            }
        }
    }

    Ok(())
}

async fn help_handler(bot: Bot, message: Message) -> ResponseResult<()> {
    bot.send_message(message.chat.id, Command::descriptions().to_string())
        .await?;
    Ok(())
}

async fn read_last_n_lines<P: AsRef<Path>>(
    filename: P,
    n: usize,
) -> Result<String, std::io::Error> {
    let file = File::open(filename).await?;
    // Start of Selection
    use tokio::io::AsyncReadExt;
    use tokio::io::{AsyncSeekExt, BufReader};

    let mut reader = BufReader::new(file);
    let mut pos = reader.seek(std::io::SeekFrom::End(0)).await?;
    let mut buffer = Vec::new();

    let mut line_count = 0;
    let mut start_pos = pos;

    // Find the (n+1)th line eliminator from the end
    while line_count <= n && pos > 0 {
        let mut buffer = [0; 1024];
        let chunk_pos = pos.saturating_sub(1024);
        reader.seek(std::io::SeekFrom::Start(chunk_pos)).await?;
        let bytes_read = reader.read(&mut buffer).await?;

        for (i, &byte) in buffer[..bytes_read].iter().rev().enumerate() {
            if byte == b'\n' {
                line_count += 1;
                if line_count == n + 1 {
                    start_pos = chunk_pos + bytes_read as u64 - i as u64;
                    break;
                }
            }
        }

        pos = chunk_pos;
        if line_count == n + 1 {
            break;
        }
    }

    // If we haven't found n+1 lines, start from the beginning
    if line_count <= n {
        start_pos = 0;
    }

    // Read from start_pos to the end
    reader.seek(std::io::SeekFrom::Start(start_pos)).await?;
    reader.read_to_end(&mut buffer).await?;

    Ok(escape_markdown(&String::from_utf8_lossy(&buffer)))
}

fn escape_markdown(contents: &str) -> String {
    contents
        .replace("\\", "\\\\")
        .replace("*", "\\*")
        .replace("_", "\\_")
        .replace("[", "\\[")
        .replace("]", "\\]")
        .replace("`", "\\`")
        .replace("~", "\\~")
        .replace(">", "\\>")
        .replace("<", "\\<")
        .replace("(", "\\(")
        .replace(")", "\\)")
        .replace("#", "\\#")
        .replace("+", "\\+")
        .replace("-", "\\-")
        .replace("=", "\\=")
        .replace("|", "\\|")
        .replace("{", "\\{")
        .replace("}", "\\}")
        .replace(".", "\\.")
        .replace("!", "\\!")
}

/// Escape auth codes in the SMS message with monospace font
fn highlight_sms_codes(contents: &str) -> String {
    let contents = escape_markdown(contents);
    // Use regex to find 6-8 digit numbers
    let re = regex::Regex::new(r"\d{4,8}").expect("Invalid regex pattern");

    // Replace each auth code with monospace formatting
    re.replace_all(&contents, |caps: &regex::Captures| {
        format!("`{}`", &caps[0])
    })
    .to_string()
}
