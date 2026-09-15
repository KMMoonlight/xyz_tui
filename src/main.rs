mod account;
mod app;
mod auth;
mod content;
mod help;
mod home;
mod markdown;
mod player;
mod playlist;
mod recommendations;
mod session;
mod settings;
mod subscriptions;
mod transcript;
mod ui;

#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal};

#[tokio::main]
async fn main() -> io::Result<()> {
    if let Some(argument) = std::env::args().nth(1) {
        match argument.as_str() {
            "--version" | "-V" => {
                println!("xyz-tui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "xyz-tui {}\n\n小宇宙终端客户端\n\n用法：xyz-tui [--version | --help]\n\n在交互式终端中运行，使用小宇宙 App 扫码登录。播放需要 mpv。\n按 ? 查看快捷键，按 q 或 Ctrl+C 退出。\nXYZ_TUI_STATE_DIR 可指定独立数据目录。",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "未知参数，请运行 xyz-tui --help",
                ));
            }
        }
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("请在交互式终端中运行"));
    }

    let mut terminal = ratatui::try_init()?;
    let result = app::run(&mut terminal).await;
    ratatui::restore();
    result
}
