// Live weather + rain alert (only while Settings > "Weather" is on).
// Location: approximate, from the IP address (ipwho.is). Weather: Open-Meteo (free, no key).
// Refreshed every 15 minutes; location is looked up again every 6 hours.
use crate::util::emit;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const UA: &str = "Atoll-Island/0.1";
static LAST: Mutex<Option<Value>> = Mutex::new(None);

async fn get_json(url: &str) -> Option<Value> {
    let r = reqwest::Client::new().get(url).header("User-Agent", UA)
        .timeout(Duration::from_secs(15)).send().await.ok()?;
    if !r.status().is_success() { return None; }
    r.json::<Value>().await.ok()
}

/// (latitude, longitude, city)
async fn locate() -> Option<(f64, f64, String)> {
    let j = get_json("https://ipwho.is/?fields=success,latitude,longitude,city").await?;
    if j["success"].as_bool() == Some(false) { return None; }
    Some((j["latitude"].as_f64()?, j["longitude"].as_f64()?, j["city"].as_str().unwrap_or("").to_string()))
}

async fn fetch(lat: f64, lon: f64, city: &str) -> Option<Value> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.3}&longitude={lon:.3}\
         &current=temperature_2m,weather_code,is_day,precipitation&minutely_15=precipitation\
         &forecast_minutely_15=6&timezone=auto");
    let j = get_json(&url).await?;
    let cur = &j["current"];
    let now_precip = cur["precipitation"].as_f64().unwrap_or(0.0);
    // If it is not raining now: will rain start within the next 60 min? (15-min slots)
    let mut rain_in: Option<u32> = None;
    if now_precip < 0.1 {
        if let Some(p) = j["minutely_15"]["precipitation"].as_array() {
            for (i, v) in p.iter().enumerate().skip(1).take(4) {
                if v.as_f64().unwrap_or(0.0) >= 0.1 { rain_in = Some(i as u32 * 15); break; }
            }
        }
    }
    Some(json!({
        "temp": cur["temperature_2m"].as_f64().map(|t| t.round() as i64),
        "code": cur["weather_code"].as_i64(),
        "isDay": cur["is_day"].as_i64() == Some(1),
        "raining": now_precip >= 0.1,
        "rainIn": rain_in,
        "city": city,
    }))
}

#[tauri::command]
pub fn get_weather() -> Value { LAST.lock().unwrap().clone().unwrap_or_else(|| json!({})) }

pub fn start() {
    tauri::async_runtime::spawn(async {
        let mut loc: Option<(f64, f64, String)> = None;
        let mut loc_at = Instant::now();
        let mut next = Instant::now();
        let mut was_on = false;
        loop {
            if !crate::settings::get().weather {
                if was_on { *LAST.lock().unwrap() = None; emit("weather", json!({})); }
                was_on = false;
                crate::settings::wait_until_async(|s| s.weather).await;
                continue;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            if !crate::settings::get().weather { continue; }
            if !was_on { next = Instant::now(); was_on = true; }   // just enabled: fetch immediately
            if Instant::now() < next { continue; }
            if loc.is_none() || loc_at.elapsed() > Duration::from_secs(6 * 3600) {
                if let Some(l) = locate().await { loc = Some(l); loc_at = Instant::now(); }
            }
            let ok = match &loc {
                Some((la, lo, c)) => match fetch(*la, *lo, c).await {
                    Some(v) => { *LAST.lock().unwrap() = Some(v.clone()); emit("weather", v); true }
                    None => false,
                },
                None => false,
            };
            next = Instant::now() + if ok { Duration::from_secs(15 * 60) } else { Duration::from_secs(120) };
        }
    });
}
