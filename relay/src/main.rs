//! The jam relay: a small web service that sits between a copy of Nexus Music that is
//! hosting a jam and the friends who have joined it.
//!
//! The app can't be reached from the internet, so it connects out to this service and
//! waits. A friend opens the jam's link, gets a page from here, and what they do on it
//! (search, add a song, skip) is handed to the app, which answers from its own library.
//! Nothing is stored: a jam lives in memory and is gone when it ends or this stops.

mod relay;

use std::sync::Arc;

use relay::{Config, Relay};

fn main() {
    let setting = |name: &str| std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
    let listen = setting("JAM_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".to_string());
    let public = setting("JAM_PUBLIC_URL").map(|address| address.trim_end_matches('/').to_string());
    let (key, temporary) = match setting("JAM_HOST_KEY") {
        Some(key) if key.len() >= 16 => (key, false),
        Some(_) => {
            eprintln!("JAM_HOST_KEY is too short: use at least 16 characters, e.g. the output of `openssl rand -hex 24`.");
            std::process::exit(2);
        }
        None => (relay::random_text(40), true),
    };
    let mut config = Config::new(key.clone());
    if let Some(most) = setting("JAM_MAX_JAMS").and_then(|text| text.parse().ok()) {
        config.max_jams = most;
    }
    let server = match tiny_http::Server::http(&listen) {
        Ok(server) => Arc::new(server),
        Err(why) => {
            eprintln!("Couldn't listen on {listen}: {why}");
            std::process::exit(1);
        }
    };
    println!("Nexus Music jam relay {} listening on {listen}.", env!("CARGO_PKG_VERSION"));
    if temporary {
        println!("No JAM_HOST_KEY is set, so a key was made up for this run and will change on the next. Set JAM_HOST_KEY to keep one.");
    }
    match &public {
        Some(address) => println!("Host link, for Nexus Music's settings (keep it private): {address}/#{key}"),
        None if temporary => println!("Host link, for Nexus Music's settings: this relay's public address, then /#{key}"),
        None => println!("Host link, for Nexus Music's settings: this relay's public address, then /# and the host key."),
    }
    relay::serve(Arc::new(Relay::new(config)), server, 8);
}
