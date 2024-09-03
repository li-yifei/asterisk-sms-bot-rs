use notify::{event::CreateKind, RecursiveMode, Watcher};
use serde::Deserialize;
use std::path::Path;
use teloxide::prelude::*;
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
}

#[derive(Deserialize)]
struct BotConfig {
    token: String,
    admin_id: i64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config_content = String::new();
    File::open("config.toml")
        .await?
        .read_to_string(&mut config_content)
        .await?;
    let config: Config = toml::from_str(&config_content)?;

    let bot = Bot::new(config.bot.token.clone());

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

    println!("Watching for new SMS files in {}", config.sms.path);

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
                    } else if let Err(e) = bot.send_message(ChatId(admin_id), contents).await {
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
