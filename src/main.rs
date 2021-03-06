// TODO: somehow better handle rate-limits (https://core.telegram.org/bots/faq#broadcasting-to-users)
//       maybe concat many messages into one (in channel) + queues to properly handle limits

use crate::{bot::setup, db::Database, krate::Crate, util::tryn};
use arraylib::Slice;
use fntools::{self, value::ValueExt};
use git2::{Delta, Diff, DiffOptions, Repository, Sort};
use log::info;
use std::str;
use teloxide::prelude::{OnError, Request};
use teloxide::types::ParseMode;
use teloxide::{Bot, BotBuilder};
use tokio_postgres::NoTls;

mod bot;
mod cfg;
mod db;
mod krate;
mod util;

#[tokio::main]
async fn main() {
    let config = cfg::Config::read().expect("couldn't read config");

    simple_logger::init_with_level(config.loglevel).unwrap();
    info!("starting");

    let db = {
        let (d, conn) = Database::connect(&config.db.cfg(), NoTls)
            .await
            .expect("couldn't connect to the database");

        // docs says to do so
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("Database connection error: {}", e);
            }
        });

        info!("connected to db");
        d
    };

    let index_url = &config.index_url; // Closures still borrow full struct :|
    let index_path = &config.index_path;
    let repo = Repository::open(index_path).unwrap_or_else(move |_| {
        info!("start cloning");
        Repository::clone(&index_url, index_path)
            .unwrap()
            .also(|_| info!("cloning finished"))
    });

    let bot = BotBuilder::new().parse_mode(ParseMode::HTML).build();

    tokio::spawn(setup(bot.clone(), db.clone()));

    loop {
        log::info!("start pulling updates");
        pull(&repo, &bot, &db, &config).await.expect("pull failed");
        log::info!("pulling updates finished");

        tokio::time::delay_for(config.pull_delay).await; // delay for 5 min
    }
}

// from https://stackoverflow.com/a/58778350
fn fast_forward(repo: &Repository, commit: &git2::Commit) -> Result<(), git2::Error> {
    let fetch_commit = repo.find_annotated_commit(commit.id())?;
    let analysis = repo.merge_analysis(&[&fetch_commit])?;
    if analysis.0.is_up_to_date() {
        Ok(())
    } else if analysis.0.is_fast_forward() {
        let mut reference = repo.find_reference("refs/heads/master")?;
        reference.set_target(fetch_commit.id(), "Fast-Forward")?;
        repo.set_head(reference.name().unwrap())?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
    } else {
        Err(git2::Error::from_str("Fast-forward only!"))
    }
}

async fn pull(
    repo: &Repository,
    bot: &Bot,
    db: &Database,
    cfg: &cfg::Config,
) -> Result<(), git2::Error> {
    // fetch changes from remote index
    repo.find_remote("origin")
        .expect("couldn't find 'origin' remote")
        .fetch(&["master"], None, None)
        .expect("couldn't fetch new version of the index");

    let mut walk = repo.revwalk()?;
    walk.push_range("HEAD~1..FETCH_HEAD")?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let commits: Result<Vec<_>, _> = walk.map(|oid| repo.find_commit(oid?)).collect();
    let mut opts = DiffOptions::default();
    let opts = opts.context_lines(0).minimal(true);
    for [prev, next] in commits?.array_windows::<[_; 2]>() {
        let diff: Diff =
            repo.diff_tree_to_tree(Some(&prev.tree()?), Some(&next.tree()?), Some(opts))?;
        let (krate, action) = diff_one(diff)?;
        notify(krate, action, bot, db, cfg).await;
        fast_forward(repo, next)?;
        // Try to prevent "too many requests" error from telegram
        tokio::time::delay_for(cfg.update_delay_millis.into()).await;
    }

    Ok(())
}

enum ActionKind {
    NewVersion,
    Yanked,
    Unyanked,
}

fn diff_one(diff: Diff) -> Result<(Crate, ActionKind), git2::Error> {
    let mut prev = None;
    let mut next = None;

    diff.foreach(
        &mut |_, _| true,
        None,
        None,
        Some(&mut |delta, _hunk, line| {
            match delta.status() {
                // New version of a crate or (un)yanked old version
                Delta::Modified | Delta::Added => {
                    assert!(delta.nfiles() == 2 || delta.nfiles() == 1);
                    match line.origin() {
                        '-' => {
                            assert!(
                                prev.is_none(),
                                "Expected number of deletions <= 1 per commit"
                            );
                            let krate = str::from_utf8(line.content()).expect("non-utf8 diff");
                            let krate = serde_json::from_str::<Crate>(krate)
                                .expect("cound't deserialize crate");

                            prev = Some(krate);
                        }
                        '+' => {
                            assert!(
                                next.is_none(),
                                "Expected number of additions = 1 per commit"
                            );
                            let krate = str::from_utf8(line.content()).expect("non-utf8 diff");
                            let krate = serde_json::from_str::<Crate>(krate)
                                .expect("cound't deserialize crate");

                            next = Some(krate);
                        }
                        _ => { /* don't care */ }
                    }
                }
                delta => {
                    log::warn!("Unexpected delta: {:?}", delta);
                }
            }

            true
        }),
    )?;

    let next = next.expect("Expected number of additions = 1 per commit");
    match (prev.as_ref().map(|c| c.yanked), next.yanked) {
        /* was yanked, is yanked */
        (None, false) => {
            // There were no deleted line & crate is not yanked.
            // New version.
            Ok((next, ActionKind::NewVersion))
        }
        (Some(false), true) => {
            // The crate was not yanked and now is yanked.
            // Crate yanked.
            Ok((next, ActionKind::Yanked))
        }
        (Some(true), false) => {
            // The crate was yanked and now is not yanked.
            // Crate unyanked.
            Ok((next, ActionKind::Unyanked))
        }
        _unexpected => {
            // Something unexpected happened
            log::warn!("Unexpected diff_one input: {:?}, {:?}", next, prev);
            Err(git2::Error::from_str("Unexpected diff"))
        }
    }
}

async fn notify(krate: Crate, action: ActionKind, bot: &Bot, db: &Database, cfg: &cfg::Config) {
    let message = match action {
        ActionKind::NewVersion => format!(
            "Crate was updated: <code>{krate}#{version}</code> {links}",
            krate = krate.id.name,
            version = krate.id.vers,
            links = krate.html_links(),
        ),
        ActionKind::Yanked => format!(
            "Crate was yanked: <code>{krate}#{version}</code> {links}",
            krate = krate.id.name,
            version = krate.id.vers,
            links = krate.html_links(),
        ),
        ActionKind::Unyanked => format!(
            "Crate was unyanked: <code>{krate}#{version}</code> {links}",
            krate = krate.id.name,
            version = krate.id.vers,
            links = krate.html_links(),
        ),
    };

    let users = db
        .list_subscribers(&krate.id.name)
        .await
        .map_err(|err| log::error!("db error while getting subscribers: {}", err))
        .unwrap_or_default();

    if let Some(ch) = cfg.channel {
        notify_inner(bot, ch, &message, cfg).await;
    }

    for chat_id in users {
        notify_inner(bot, chat_id, &message, cfg).await;
    }
}

async fn notify_inner(bot: &Bot, chat_id: i64, msg: &str, cfg: &cfg::Config) {
    bot.send_message(chat_id, msg)
        .disable_web_page_preview(true)
        .disable_notification(true)
        .send()
        .await
        .log_on_error()
        .await;
    tokio::time::delay_for(cfg.broadcast_delay_millis.into()).await;
}
