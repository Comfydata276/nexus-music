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
    // The host key is never printed, here or anywhere: whoever reads this program's output
    // could then start jams and take places in rooms. So it has to be given.
    let key = match setting("JAM_HOST_KEY") {
        Some(key) if key.len() >= 16 => key,
        Some(_) => {
            eprintln!("JAM_HOST_KEY is too short: use at least 16 characters, e.g. the output of `openssl rand -hex 24`.");
            std::process::exit(2);
        }
        None => {
            eprintln!("Set JAM_HOST_KEY: at least 16 characters, e.g. the output of `openssl rand -hex 24`. The host link is this relay's public address, then /# and that key.");
            std::process::exit(2);
        }
    };
    let mut config = Config::new(key);
    if let Some(most) = setting("JAM_MAX_JAMS").and_then(|text| text.parse().ok()) {
        config.max_jams = most;
    }
    if let Some(proxy) = setting("JAM_TRUSTED_PROXY") {
        match proxy.parse() {
            Ok(address) => config.trusted_proxy = Some(address),
            Err(_) => {
                eprintln!("JAM_TRUSTED_PROXY isn't an address: give the one address the proxy in front of this relay connects from.");
                std::process::exit(2);
            }
        }
    }
    config.old_link = !setting("JAM_OLD_LINK").is_some_and(|value| matches!(value.to_lowercase().as_str(), "off" | "no" | "false" | "0"));
    let server = match relay::listen(&listen, relay::WRITE_DEADLINE) {
        Ok(server) => Arc::new(server),
        Err(why) => {
            eprintln!("Couldn't listen on {listen}: {why}");
            std::process::exit(1);
        }
    };
    println!("Nexus Music jam relay {} listening on {listen}.", env!("CARGO_PKG_VERSION"));
    match &public {
        Some(address) => println!("Host link, for Nexus Music's settings (keep it private): {address}/# and the host key."),
        None => println!("Host link, for Nexus Music's settings: this relay's public address, then /# and the host key."),
    }
    if !config.old_link {
        println!("Devices that link the old way are turned away.");
    }
    let relay = Arc::new(Relay::new(config));
    // The route a person's devices link through is served apart from everything else, at
    // an address of its own, so that nothing done to it can slow a jam. The proxy in front
    // sends `/api/link/` there.
    if let Some(address) = setting("JAM_LINK_LISTEN") {
        match relay::link::listen(relay.clone(), &address) {
            Ok(at) => println!("Devices link through {at}: have the proxy in front send /api/link/ there."),
            Err(why) => {
                eprintln!("Couldn't listen on {address} for devices to link through: {why}");
                std::process::exit(1);
            }
        }
    }
    relay::serve(relay, server, 8);
}
