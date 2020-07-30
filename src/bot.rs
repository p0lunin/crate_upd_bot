use crate::cfg::RetryDelay;
use crate::krate::Crate;
use crate::{
    db::Database,
    util::{crate_path, tryn},
};
use fntools::value::ValueExt;
use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};
use teloxide::prelude::*;
use teloxide::utils::command::BotCommand;

#[derive(Debug, BotCommand)]
#[command(rename = "lowercase")]
enum Command {
    Start,
    Subscribe(String),
    Unsubscribe(String),
    List,
    Help,
}

const START_MESSAGE: &'static str = "
Hi! I will notify you about updates of crates. Use /subscribe to subscribe for updates of crates you want to be notified about.

In case you want to see <b>all</b> updates go to @crates_updates

Author: @wafflelapkin
His channel [ru]: @ihatereality
My source: <a href='https://github.com/WaffleLapkin/crate_upd_bot'>[github]</a>";

pub async fn setup(bot: Bot, db: Database) {
    teloxide::commands_repl(bot, todo!(), |cx, cmd: Command| dispatch(cx, cmd, &db)).await;
}

async fn dispatch(cx: UpdateWithCx<Message>, cmd: Command, db: &Database) -> Result<(), HErr> {
    match cmd {
        Command::Start => {
            cx.answer_str(START_MESSAGE).await?;
        }
        Command::Subscribe(crate_name) => {
            let krate = crate_name.as_str();
            if PathBuf::from("./index")
                .also(|p| p.push(crate_path(krate)))
                .exists()
            {
                db.subscribe(cx.chat_id(), krate).await?;
                let v = match Crate::read_last(krate).await {
                    Ok(krate) => format!(
                        " (current version <code>{}</code> {})",
                        krate.id.vers,
                        krate.html_links()
                    ),
                    Err(_) => String::new(),
                };
                let text = format!("You've successfully subscribed for updates on <code>{}</code>{} crate. Use /unsubscribe to unsubscribe.", krate, v);
                cx.answer(text)
                    .disable_web_page_preview(true)
                    .send()
                    .await?;
            } else {
                let text = format!("Error: there is no such crate <code>{}</code>.", krate);
                cx.answer_str(text).await?;
            }
        }
        Command::Unsubscribe(crate_name) => {
            let krate = crate_name.as_str();
            db.unsubscribe(cx.chat_id(), krate).await?;
            let text = format!("You've successfully unsubscribed for updates on <code>{}</code> crate. Use /subscribe to subscribe back.", krate);
            cx.answer_str(text).await?;
        }
        Command::List => {
            let mut subscriptions = db.list_subscriptions(cx.chat_id()).await?;
            for sub in &mut subscriptions {
                match Crate::read_last(sub).await {
                    Ok(krate) => {
                        sub.push('#');
                        sub.push_str(&krate.id.vers);
                        sub.push_str("</code> ");
                        sub.push_str(&krate.html_links());
                    }
                    Err(_) => {
                        sub.push_str(" </code>");
                        /* silently ignore error & just don't add links */
                    }
                }
            }

            if subscriptions.is_empty() {
                let text = "Currently you aren't subscribed to anything. Use /subscribe to subscribe to some crate.";
                cx.answer_str(text).await?;
            } else {
                let text = format!(
                    "You are currently subscribed to:\n— <code>{}",
                    subscriptions.join("\n— <code>")
                );
                cx.answer(text)
                    .disable_web_page_preview(true)
                    .send()
                    .await?;
            }
        }
        Command::Help => {
            cx.answer_str(Command::descriptions()).await?;
        }
    };
    Ok(())
}

#[derive(Debug, derive_more::Display, derive_more::From, derive_more::Error)]
enum HErr {
    Tg(teloxide::RequestError),
    Bd(tokio_postgres::Error),
    GetUser,
}
