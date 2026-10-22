use clap::{Parser, ValueEnum};
use discord_client_gateway::events::Event;
use discord_client_gateway::gateway::GatewayClient;
use discord_client_rest::rest::{RequestProperties, RestClient};
use discord_client_structs::structs::application::detectable::DetectableApplication;
use discord_client_structs::structs::channel::Channel;
use discord_client_structs::structs::user::status::StatusType;
use rand::seq::SliceRandom;
use std::time::Duration;

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
    stream_channel: Option<u64>,

    /// Status shown while farming
    #[arg(short, long, value_enum, default_value_t = Status::Idle)]
    status: Status,

    /// Leave any voice channel and exit
    #[arg(long)]
    leave: bool,

    /// Guild of the voice channel (looked up from the channel when omitted)
    #[arg(long, requires = "stream_channel")]
    stream_guild: Option<u64>,
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
                            p.current,
                            p.threshold,
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
    let mut stream = match cli.stream_channel {
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

    let mut gateway = GatewayClient::builder(token)
        .desktop()
        .connect()
        .await
        .expect("gateway connect");
    gateway.status = cli.status.into();

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
    gateway.clear_game_activity().await.ok();
    print_progress(&rest).await;
    gateway.graceful_shutdown().await.ok();
}
