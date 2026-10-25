use discord_client_rest::rest::{RequestProperties, RestClient};
use discord_client_structs::structs::application::detectable::DetectableApplication;
use rand::Rng;
use rand::seq::SliceRandom;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const MS_HOUR: f64 = 3_600_000.0;

fn uuid() -> String {
    let mut rng = rand::rng();
    let hex: String = (0..32)
        .map(|_| format!("{:x}", rng.random_range(0..16u8)))
        .collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn win32_exe(app: &DetectableApplication) -> Option<&str> {
    app.executables
        .iter()
        .find(|e| e.os == "win32" && !e.is_launcher && !e.name.is_empty())
        .map(|e| e.name.as_str())
}

struct Ids {
    hb: String,
    sig: String,
    seq: u64,
}

struct Session {
    game: usize,
    start: i64,
    duration: i64,
}

impl Ids {
    fn new() -> Self {
        Self {
            hb: uuid(),
            sig: uuid(),
            seq: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn heartbeat(
        &mut self,
        game_id: &str,
        name: &str,
        exe: &str,
        session_id: &str,
        ts: i64,
        tracked: i64,
        initial: bool,
        last: bool,
    ) -> Value {
        json!({
            "type": "running_game_heartbeat",
            "properties": {
                "client_track_timestamp": ts,
                "client_heartbeat_session_id": self.hb,
                "event_sequence_number": self.next_seq(),
                "game_id": game_id,
                "game_name": name,
                "game_metadata": null,
                "game_executable": exe,
                "game_detection_enabled": true,
                "initial_heartbeat": initial,
                "final_heartbeat": last,
                "game_session_id": session_id,
                "duration_tracked_ms": tracked,
                "rtc_connection_id": null,
                "media_session_id": null,
                "launch_signature": self.sig,
                "client_app_state": "focused",
                "client_send_timestamp": ts,
            }
        })
    }

    fn launch(&mut self, game_id: &str, name: &str, exe: &str, ts: i64) -> Value {
        json!({
            "type": "launch_game",
            "properties": {
                "client_track_timestamp": ts,
                "client_heartbeat_session_id": self.hb,
                "event_sequence_number": self.next_seq(),
                "game": name,
                "game_id": game_id,
                "verified": true,
                "elevated": false,
                "is_launcher": false,
                "game_platform": "desktop",
                "detection_method": "verified_game",
                "is_overlay_enabled": false,
                "is_overlay_game_enabled": true,
                "is_overlay_game_source": "OOP_DEFAULT_DATABASE",
                "fullscreen_type": "UNKNOWN",
                "hardware_display_count": 1,
                "overlay_method": "Disabled",
                "activity_status_enabled": true,
                "activity_status_shared_guilds": [],
                "current_user_status": "online",
                "game_detection_enabled": true,
                "executable_path": exe,
                "voice_channel_id": null,
                "voice_channel_type": null,
                "voice_channel_bitrate": null,
                "voice_channel_guild_id": null,
                "hidden_by_distributor": false,
                "game_metadata": null,
                "client_performance_cpu": null,
                "client_performance_memory": null,
                "cpu_core_count": null,
                "accessibility_features": 0,
                "rendered_locale": "en-GB",
                "launch_signature": self.sig,
                "client_rtc_state": null,
                "client_app_state": "focused",
                "client_send_timestamp": ts,
            }
        })
    }

    fn session_events(
        &mut self,
        game_id: &str,
        name: &str,
        exe: &str,
        session: &Session,
        hb_minutes: f64,
    ) -> Vec<Value> {
        let session_id = uuid();
        let step = ((hb_minutes * 60_000.0) as i64).max(1);
        let mut events = Vec::new();
        events.push(self.heartbeat(
            game_id,
            name,
            exe,
            &session_id,
            session.start,
            0,
            true,
            false,
        ));
        events.push(self.launch(game_id, name, exe, session.start));
        let mut tracked = 0i64;
        while tracked + step < session.duration {
            tracked += step;
            events.push(self.heartbeat(
                game_id,
                name,
                exe,
                &session_id,
                session.start + tracked,
                tracked,
                false,
                false,
            ));
        }
        events.push(self.heartbeat(
            game_id,
            name,
            exe,
            &session_id,
            session.start + session.duration,
            session.duration,
            false,
            true,
        ));
        events
    }
}

async fn analytics_token(
    rest: &RestClient,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut query = HashMap::new();
    query.insert("with_analytics_token".to_string(), "true".to_string());
    let me: Value = rest
        .get("users/@me", Some(query), Some(RequestProperties::home()))
        .await?;
    me.get("analytics_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "no analytics_token in users/@me response".into())
}

fn game_pool(apps: &[DetectableApplication]) -> Vec<(&DetectableApplication, &str)> {
    apps.iter()
        .filter_map(|a| win32_exe(a).map(|exe| (a, exe)))
        .collect()
}

fn library(games: usize, total_hours: f64) -> Vec<f64> {
    let heavy = ((games as f64 * 0.10).round() as usize).max(1).min(games);
    let mid = ((games as f64 * 0.25).round() as usize)
        .max(1)
        .min(games.saturating_sub(heavy));
    let rest = games.saturating_sub(heavy + mid);
    let mut weights = Vec::with_capacity(games + 1);
    weights.extend(std::iter::repeat_n(4.0, heavy));
    weights.extend(std::iter::repeat_n(1.5, mid));
    weights.extend(std::iter::repeat_n(1.0, rest));
    let sum: f64 = weights.iter().sum();
    weights
        .iter()
        .map(|w| total_hours * w / sum.max(1e-9))
        .collect()
}

fn schedule(
    hours: &[f64],
    session_max: f64,
    spread_days: usize,
    day_max: f64,
) -> Result<(Vec<Session>, usize), String> {
    let mut rng = rand::rng();
    let mut chunks: Vec<(usize, f64)> = Vec::new();
    for (game, total) in hours.iter().enumerate() {
        let mut left = *total;
        while left > 0.05 {
            let take = left.min(rng.random_range(1.5..=session_max));
            chunks.push((game, take));
            left -= take;
        }
    }
    chunks.shuffle(&mut rng);
    let active_days = (spread_days as f64 * 0.85).max(1.0);
    let per_day = ((chunks.len() as f64 / active_days).ceil() as usize).max(1);

    let mut queue = chunks.into_iter().peekable();
    let mut plan = Vec::new();
    let mut day_end = now_ms();
    let mut days_used = 0usize;

    for _ in 0..spread_days {
        if queue.peek().is_none() {
            break;
        }
        days_used += 1;
        if rng.random_bool(0.15) {
            day_end -= 86_400_000;
            continue;
        }
        let target = per_day + rng.random_range(0usize..=1);
        let mut placed = 0usize;
        let mut play = 0.0f64;
        let mut window = 0.0f64;
        let mut session_end = day_end - (rng.random_range(0.5..=4.0) * MS_HOUR) as i64;
        while placed < target {
            let Some(&(game, hours)) = queue.peek() else {
                break;
            };
            let gap = rng.random_range(0.1..=0.5);
            if play + hours > day_max || window + hours + gap > 21.0 {
                break;
            }
            queue.next();
            let duration = (hours * MS_HOUR) as i64;
            let start = session_end - duration;
            plan.push(Session {
                game,
                start,
                duration,
            });
            placed += 1;
            play += hours;
            window += hours + gap;
            session_end = start - (gap * MS_HOUR) as i64;
        }
        day_end -= 86_400_000;
    }

    if queue.peek().is_some() {
        return Err(format!(
            "{} sessions placed, games still queued - raise --claim-spread-days or --claim-day-max-h",
            plan.len()
        ));
    }
    Ok((plan, days_used))
}

async fn send(
    rest: &RestClient,
    analytics: &str,
    events: Vec<Value>,
    batch: usize,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let batch = batch.max(1);
    let total = events.len();
    let requests = (total + batch - 1) / batch;
    let mut sent = 0usize;
    for (index, chunk) in events.chunks(batch).enumerate() {
        let body = json!({"token": analytics, "events": chunk});
        let _: Value = rest
            .post("science", Some(body), Some(RequestProperties::home()))
            .await?;
        sent += chunk.len();
        if (index + 1) % 25 == 0 || index + 1 == requests {
            println!("[science] {sent}/{total} events in {}/{} requests", index + 1, requests);
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    println!("[science] done: {sent} events in {requests} requests");
    Ok(sent)
}

pub async fn claim_plan(
    rest: &RestClient,
    total_hours: f64,
    games: usize,
    session_max: f64,
    hb_minutes: f64,
    spread_days: usize,
    day_max: f64,
    batch: usize,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let analytics = analytics_token(rest).await?;
    let apps = rest.application().get_detectable_games().await?;
    let mut pool = game_pool(&apps);
    if pool.len() < games {
        return Err(format!("only {} detectable games with a win32 exe", pool.len()).into());
    }
    pool.shuffle(&mut rand::rng());
    pool.truncate(games);

    let hours = library(games, total_hours);
    let (mut plan, days_used) = schedule(&hours, session_max, spread_days, day_max)?;
    plan.sort_by_key(|s| s.start);

    let mut ids = Ids::new();
    let mut events = Vec::new();
    for session in &plan {
        let (app, exe) = pool[session.game];
        events.extend(ids.session_events(&app.id.to_string(), &app.name, exe, session, hb_minutes));
    }

    let planned: f64 = plan.iter().map(|s| s.duration as f64 / MS_HOUR).sum();
    let span_days = match (plan.first(), plan.last()) {
        (Some(first), Some(last)) => {
            (last.start + last.duration - first.start) as f64 / 86_400_000.0
        }
        _ => 0.0,
    };
    let requests = (events.len() + batch.max(1) - 1) / batch.max(1);
    println!(
        "[science] plan: {} games, {:.0}h, {} sessions, {:.1}h/day over {} days, {} events, {} requests",
        games,
        planned,
        plan.len(),
        planned / days_used.max(1) as f64,
        days_used,
        events.len(),
        requests
    );
    println!("[science] history spans {span_days:.0} days (dry_run={dry_run})");

    if dry_run {
        println!(
            "[science] first event: {}",
            serde_json::to_string_pretty(&events[0]).unwrap_or_default()
        );
        return Ok(());
    }
    send(rest, &analytics, events, batch).await?;
    Ok(())
}

pub async fn claim(
    rest: &RestClient,
    count: usize,
    hours: f64,
    batch: usize,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let analytics = analytics_token(rest).await?;
    let apps = rest.application().get_detectable_games().await?;
    let mut pool = game_pool(&apps);
    if pool.is_empty() {
        return Err("no detectable games with a win32 executable".into());
    }
    pool.shuffle(&mut rand::rng());
    let picked: Vec<(&DetectableApplication, &str)> = pool.into_iter().take(count).collect();

    let duration = (hours * MS_HOUR) as i64;
    let start = now_ms() - duration;
    let mut ids = Ids::new();
    let mut events = Vec::new();
    for (app, exe) in &picked {
        let session = Session {
            game: 0,
            start,
            duration,
        };
        events.extend(ids.session_events(
            &app.id.to_string(),
            &app.name,
            exe,
            &session,
            hours * 60.0,
        ));
    }

    println!(
        "[science] {} games x {}h = {:.0}h claimed ({} events, dry_run={})",
        picked.len(),
        hours,
        picked.len() as f64 * hours,
        events.len(),
        dry_run
    );
    if dry_run {
        println!(
            "[science] first event: {}",
            serde_json::to_string_pretty(&events[0]).unwrap_or_default()
        );
        return Ok(());
    }
    send(rest, &analytics, events, batch).await?;
    Ok(())
}
