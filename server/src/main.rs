//! The Nexus server's program. `nexus-music-server` serves; `nexus-music-server health` is
//! the container's own check, and exits 0 if the backend answers as it should.

use std::process::ExitCode;

use nexus_music_server::{health, identity, serve, settings::Settings};

fn main() -> ExitCode {
    // A fault's own words are not written out, since they may hold what a request held;
    // where it happened is (design 13.5). A fault in what answers a request is that request's
    // 500 and nothing else's (see `serve`).
    std::panic::set_hook(Box::new(|fault| match fault.location() {
        Some(at) => eprintln!("A fault at {}:{}.", at.file(), at.line()),
        None => eprintln!("A fault."),
    }));
    let settings = match Settings::read(|name| std::env::var(name).ok()) {
        Ok(settings) => settings,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::from(2);
        }
    };
    match std::env::args().nth(1).as_deref() {
        None => run(settings),
        Some("health") => match health::check(&settings) {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("{why}");
                ExitCode::FAILURE
            }
        },
        Some(_) => {
            eprintln!("Run it with nothing after it to serve, or with `health` to check that it is serving.");
            ExitCode::from(2)
        }
    }
}

fn run(settings: Settings) -> ExitCode {
    if let Err(why) = settings.check_folders() {
        eprintln!("{why}");
        return ExitCode::from(2);
    }
    let random = || {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|why| format!("No randomness to make the backend's id with ({why})."))?;
        Ok(bytes)
    };
    let (id, made) = match identity::read_or_make(&settings.state, random) {
        Ok(found) => found,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(why) => {
            eprintln!("The runtime couldn't be started ({}).", why.kind());
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let listener = match tokio::net::TcpListener::bind(settings.listen).await {
            Ok(listener) => listener,
            Err(why) => {
                eprintln!("NEXUS_LISTEN is wrong: nothing can listen there ({}).", why.kind());
                return ExitCode::from(2);
            }
        };
        // The settings, none of them secret, and the id, which is not. What the owner typed is
        // written quoted and escaped, so that nothing in it can pass for another line.
        println!(
            "Nexus server {} started, with id {} ({}). Listening on {}; answering the proxy at {}; music server {:?}; data {:?}; store kept apart {:?}.",
            nexus_music_server::version(),
            id.to_hex(),
            if made { "made now" } else { "kept" },
            settings.listen,
            settings.proxy,
            settings.music_server,
            settings.data,
            settings.state,
        );
        let shell = serve::Shell::new(settings.proxy, &nexus_music_server::hello(id));
        serve::serve(listener, shell, true).await;
        ExitCode::SUCCESS
    })
}
