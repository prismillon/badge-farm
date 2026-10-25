use discord_client_rest::rest::{RequestProperties, RestClient};
use discord_client_structs::structs::application::detectable::DetectableApplication;
use discord_client_structs::structs::channel::Channel;
use rand::seq::SliceRandom;

const GAME_TIME_BADGE: u64 = 20;
const GAME_VARIETY_BADGE: u64 = 21;
const STREAMING_BADGE: u64 = 19;

pub fn pick_games(all: Vec<DetectableApplication>, count: usize) -> Vec<DetectableApplication> {
    let mut candidates: Vec<_> = all
        .into_iter()
        .filter(|g| {
            g.executables
                .iter()
                .any(|e| e.os == "win32" && !e.is_launcher)
        })
        .collect();
    candidates.shuffle(&mut rand::rng());
    if count > 0 {
        candidates.truncate(count);
    }
    candidates
}

pub async fn print_progress(rest: &RestClient) {
    match rest.user().get_badges().await {
        Ok(badges) => {
            for badge in badges.badges.iter().filter(|b| {
                [GAME_TIME_BADGE, GAME_VARIETY_BADGE, STREAMING_BADGE].contains(&b.badge_id)
            }) {
                let progress = badge
                    .progress
                    .as_ref()
                    .and_then(|p| p.first())
                    .map(|p| {
                        format!(
                            "{}/{} {} ({})",
                            p.current.unwrap_or(0),
                            p.threshold.unwrap_or(0),
                            p.unit.as_deref().unwrap_or(""),
                            p.progress_helper_text.as_deref().unwrap_or("")
                        )
                    })
                    .unwrap_or_else(|| "no progress".to_string());
                println!(
                    "[badge] {} tier={:?} next={:?} {}",
                    badge.name, badge.current_tier, badge.next_tier, progress
                );
            }
        }
        Err(e) => eprintln!("[badge] fetch failed: {e}"),
    }
}

pub async fn resolve_guild(rest: &RestClient, guild: Option<u64>, channel: u64) -> Option<u64> {
    if guild.is_some() {
        return guild;
    }
    rest.get::<Channel>(
        &format!("channels/{channel}"),
        None,
        Some(RequestProperties::home()),
    )
    .await
    .expect("stream channel lookup")
    .guild_id
}
