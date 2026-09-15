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
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("请在交互式终端中运行"));
    }

    let mut terminal = ratatui::try_init()?;
    let result = app::run(&mut terminal).await;
    ratatui::restore();
    result
}
