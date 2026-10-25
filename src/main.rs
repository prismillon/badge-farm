use clap::{Parser, ValueEnum};
use discord_client_gateway::events::Event;
use discord_client_gateway::gateway::GatewayClient;
use discord_client_rest::rest::{RequestProperties, RestClient};
use discord_client_structs::structs::application::detectable::DetectableApplication;
use discord_client_structs::structs::channel::Channel;
use discord_client_structs::structs::user::status::StatusType;
use rand::seq::SliceRandom;
use std::time::Duration;

mod science;
mod stream;
use stream::StreamFarm;
use tokio::time::{Instant, interval, sleep, sleep_until};

const GAME_TIME_BADGE: u64 = 20;
const GAME_VARIETY_BADGE: u64 = 21;
const STREAMING_BADGE: u64 = 19;

#[derive(Clone, Copy, ValueEnum)]
enum Status {
    Online,
    Idle,
    Dnd,
}

impl From<Status> for StatusType {
    fn from(status: Status) -> Self {
        match status {
            Status::Online => StatusType::Online,
            Status::Idle => StatusType::AFK,
            Status::Dnd => StatusType::DoNotDisturb,
        }
    }
}

#[derive(Parser)]
#[command(about = "Farms Discord badge progress (Game Time, Game Variety, Streaming)")]
struct Cli {
    /// File containing the account token
    #[arg(short, long, default_value = "token.txt")]
    token_file: String,

    /// Number of random games to rotate through (0 = every detectable game)
    #[arg(short, long, default_value_t = 0)]
    games: usize,

    /// Minutes spent on each game before switching
    #[arg(short, long, default_value_t = 10.0)]
    rotate_minutes: f64,

    /// Minutes between badge progress checks
    #[arg(short, long, default_value_t = 5.0)]
    poll_minutes: f64,

    /// Stop after going through every game once
    #[arg(long)]
    once: bool,

    /// Do not play games, only stream
    #[arg(long, requires = "stream_channel")]
    no_games: bool,

    /// Voice channel to join and stream in
    #[arg(short = 'c', long)]
    stream_channel: Vec<u64>,

    /// Status shown while farming
    #[arg(short, long, value_enum, default_value_t = Status::Idle)]
    status: Status,

    /// Leave any voice channel and exit
    #[arg(long)]
    leave: bool,

    /// Guild of the voice channel (looked up from the channel when omitted)
    #[arg(long, requires = "stream_channel")]
    stream_guild: Option<u64>,

    /// Claim playtime on N detectable games via the science endpoint, then exit
    #[arg(long, default_value_t = 0)]
    claim_games: usize,

    /// Hours to claim per game
    #[arg(long, default_value_t = 1.0)]
    claim_hours: f64,

    /// Events per science request
    #[arg(long, default_value_t = 50)]
    claim_batch: usize,

    /// Build the science payload and print it without posting
    #[arg(long)]
    claim_dry_run: bool,

    /// Plan mode: total hours to claim across a backdated history
    #[arg(long, default_value_t = 0.0)]
    claim_total_hours: f64,

    /// Plan mode: maximum hours per session (sessions above ~15h get rejected)
    #[arg(long, default_value_t = 5.0)]
    claim_session_max: f64,

    /// Plan mode: minutes between heartbeats inside a session
    #[arg(long, default_value_t = 15.0)]
    claim_heartbeat_min: f64,

    /// Plan mode: how many days the claimed history spans
    #[arg(long, default_value_t = 540)]
    claim_spread_days: usize,

    /// Plan mode: maximum hours of play in a single day
    #[arg(long, default_value_t = 14.0)]
    claim_day_max_h: f64,

}

fn pick_games(all: Vec<DetectableApplication>, count: usize) -> Vec<DetectableApplication> {
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

async fn print_progress(rest: &RestClient) {
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

async fn verify_gateway(
    token: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut gateway = tokio::time::timeout(
        Duration::from_secs(30),
        GatewayClient::builder(token.to_string()).desktop().connect(),
    )
    .await
    .map_err(|_| "gateway connect timed out")??;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            return Err("gateway never reached READY".into());
        }
        match tokio::time::timeout(Duration::from_secs(10), gateway.next_event()).await {
            Ok(Ok(Event::Ready(_))) => break,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => return Err(e),
            Err(_) => continue,
        }
    }

    gateway.graceful_shutdown().await.ok();
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let rotate = Duration::from_secs_f64(cli.rotate_minutes * 60.0);
    let token = std::fs::read_to_string(&cli.token_file)
        .expect("token file")
        .trim()
        .to_string();

    let rest = RestClient::connect(token.clone(), None, None, None, None)
        .await
        .expect("rest connect");

    let claiming = cli.claim_games > 0 || cli.claim_total_hours > 0.0;
    if claiming && !cli.claim_dry_run {
        if let Err(e) = verify_gateway(&token).await {
            eprintln!("[check] gateway verification failed, nothing posted: {e}");
            return;
        }
        println!("[check] gateway reached READY, token valid - posting");
    }

    if cli.claim_total_hours > 0.0 {
        let games = if cli.claim_games > 0 { cli.claim_games } else { 115 };
        if let Err(e) = science::claim_plan(
            &rest,
            cli.claim_total_hours,
            games,
            cli.claim_session_max,
            cli.claim_heartbeat_min,
            cli.claim_spread_days,
            cli.claim_day_max_h,
            cli.claim_batch,
            cli.claim_dry_run,
        )
        .await
        {
            eprintln!("[science] {e}");
        }
        return;
    }

    if cli.claim_games > 0 {
        if let Err(e) = science::claim(
            &rest,
            cli.claim_games,
            cli.claim_hours,
            cli.claim_batch,
            cli.claim_dry_run,
        )
        .await
        {
            eprintln!("[science] {e}");
        }
        return;
    }

    let games = if cli.no_games {
        Vec::new()
    } else {
        let games = pick_games(
            rest.application()
                .get_detectable_games()
                .await
                .expect("detectable games"),
            cli.games,
        );
        assert!(!games.is_empty(), "no detectable game found");
        println!("[farm] {} games in rotation", games.len());
        games
    };

    let mut leave_guild = None;
    let mut extras: Vec<(Option<u64>, u64)> = Vec::new();
    let mut stream = match cli.stream_channel.first().copied() {
        Some(channel) => {
            let guild = match cli.stream_guild {
                Some(guild) => Some(guild),
                None => {
                    rest.get::<Channel>(
                        &format!("channels/{channel}"),
                        None,
                        Some(RequestProperties::home()),
                    )
                    .await
                    .expect("stream channel lookup")
                    .guild_id
                }
            };
            println!("[stream] channel {channel} guild {guild:?}");
            leave_guild = guild;
            Some(StreamFarm::new(guild, channel, rest.user_id))
        }
        None => None,
    };

    for &channel in cli.stream_channel.iter().skip(1) {
        let guild = match cli.stream_guild {
            Some(guild) => Some(guild),
            None => {
                rest.get::<Channel>(
                    &format!("channels/{channel}"),
                    None,
                    Some(RequestProperties::home()),
                )
                .await
                .expect("stream channel lookup")
                .guild_id
            }
        };
        println!("[stream] extra channel {channel} guild {guild:?}");
        extras.push((guild, channel));
    }

    let mut gateway = GatewayClient::builder(token.clone())
        .desktop()
        .connect()
        .await
        .expect("gateway connect");
    gateway.status = cli.status.into();

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut extra_tasks = Vec::new();
    for (guild, channel) in extras {
        let token = token.clone();
        let status = cli.status;
        let user_id = rest.user_id;
        let game = games.first().map(|g| (g.name.clone(), g.id));
        let stop_rx = stop_rx.clone();
        extra_tasks.push(tokio::spawn(async move {
            stream_only(token, status, guild, channel, user_id, game, stop_rx).await;
        }));
        sleep(Duration::from_millis(1200)).await;
    }

    if cli.leave {
        gateway
            .update_voice_state(leave_guild, None, false, false, None)
            .await
            .expect("leave voice");
        sleep(Duration::from_secs(3)).await;
        gateway.graceful_shutdown().await.ok();
        return;
    }

    print_progress(&rest).await;

    let mut poll = interval(Duration::from_secs_f64(cli.poll_minutes * 60.0));
    poll.tick().await;
    let mut tick = interval(Duration::from_secs(1));

    let mut index = 0;
    if !games.is_empty() {
        let game = &games[index];
        gateway
            .set_game_activity(&game.name, game.id, None)
            .await
            .expect("set activity");
        println!("[farm] playing {} ({})", game.name, game.id);
    }
    let mut switch_at = Instant::now() + rotate;

    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    loop {
        tokio::select! {
            event = gateway.next_event() => match event {
                Ok(event) => {
                    match &event {
                        Event::Ready(_) => {
                            println!("[gateway] ready");
                            if !games.is_empty() {
                                let game = &games[index];
                                if let Err(e) = gateway.set_game_activity(&game.name, game.id, None).await {
                                    eprintln!("[farm] set activity failed: {e}");
                                }
                            }
                        }
                        Event::Resumed(_) => println!("[gateway] resumed"),
                        _ => {}
                    }
                    if let Some(stream) = stream.as_mut() {
                        stream.on_event(&event);
                        stream.step(&mut gateway).await;
                    }
                }
                Err(e) => {
                    eprintln!("[gateway] {e}");
                    sleep(Duration::from_secs(5)).await;
                }
            },
            _ = tick.tick() => {
                if let Some(stream) = stream.as_mut() {
                    stream.step(&mut gateway).await;
                }
            }
            _ = poll.tick() => print_progress(&rest).await,
            _ = sleep_until(switch_at), if !games.is_empty() => {
                if cli.once && index + 1 >= games.len() {
                    break;
                }
                print_progress(&rest).await;
                index = (index + 1) % games.len();
                let game = &games[index];
                gateway.clear_game_activity().await.ok();
                sleep(Duration::from_secs(2)).await;
                if let Err(e) = gateway.set_game_activity(&game.name, game.id, None).await {
                    eprintln!("[farm] set activity failed: {e}");
                }
                println!("[farm] playing {} ({})", game.name, game.id);
                switch_at = Instant::now() + rotate;
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }

    if let Some(stream) = stream.as_mut() {
        stream.stop(&mut gateway).await;
    }
    let _ = stop_tx.send(true);
    for task in extra_tasks {
        let _ = task.await;
    }
    gateway.clear_game_activity().await.ok();
    print_progress(&rest).await;
    gateway.graceful_shutdown().await.ok();
}

async fn stream_only(
    token: String,
    status: Status,
    guild: Option<u64>,
    channel: u64,
    user_id: u64,
    game: Option<(String, u64)>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut gateway = loop {
        let attempt = tokio::time::timeout(
            Duration::from_secs(45),
            GatewayClient::builder(token.clone()).desktop().connect(),
        )
        .await;
        match attempt {
            Ok(Ok(gateway)) => break gateway,
            Ok(Err(e)) => eprintln!("[stream {channel}] connect failed: {e}"),
            Err(_) => eprintln!("[stream {channel}] connect timed out"),
        }
        tokio::select! {
            _ = sleep(Duration::from_secs(5)) => {}
            _ = stop.changed() => return,
        }
    };
    gateway.status = status.into();
    if let Some((name, id)) = game {
        if let Err(e) = gateway.set_game_activity(&name, id, None).await {
            eprintln!("[stream {channel}] set activity failed: {e}");
        }
    }
    let mut stream = StreamFarm::new(guild, channel, user_id);
    let mut tick = interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            event = gateway.next_event() => match event {
                Ok(event) => {
                    stream.on_event(&event);
                    stream.step(&mut gateway).await;
                }
                Err(e) => {
                    eprintln!("[stream {channel}] {e}");
                    sleep(Duration::from_secs(5)).await;
                }
            },
            _ = tick.tick() => stream.step(&mut gateway).await,
            _ = stop.changed() => break,
        }
    }

    stream.stop(&mut gateway).await;
    gateway.clear_game_activity().await.ok();
    gateway.graceful_shutdown().await.ok();
}
