//! The Nexus server's program. `nexus-music-server` serves; `nexus-music-server health` is
//! the container's own check, and exits 0 if the backend answers as it should. The owner's
//! two commands (design 11.2, 13.2), each refused while the backend serves:
//! `nexus-music-server put-back [<copy>]` puts a copy of the data back (with no copy named,
//! it lists the nightly copies); `nexus-music-server take-as-is` takes the data as it is when
//! the store kept apart is lost.

use std::process::ExitCode;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use nexus_music_server::store::keeper::{self, Keeper};
use nexus_music_server::store::copies;
use nexus_music_server::{health, serve, settings::Settings};

/// Milliseconds on this machine's clock. (Before 1970 is 0: every decision is made at the later
/// of this and the latest time the backend has acted at.)
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

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
        Some("put-back") => command(&settings, |state, data| match std::env::args().nth(2) {
            None => Ok(list(data)),
            Some(copy) => keeper::put_back(state, data, Path::new(&copy), now()),
        }),
        Some("take-as-is") => command(&settings, |state, data| keeper::take_as_is(state, data, now())),
        Some(_) => {
            eprintln!("Run it with nothing after it to serve; with `health` to check that it is serving; with `put-back <copy>` to put a copy of the data back (`put-back` alone lists the copies); or with `take-as-is` to take the data as it is when the store kept apart is lost.");
            ExitCode::from(2)
        }
    }
}

/// One of the owner's commands: what it did, or why it did nothing, in plain words.
fn command(settings: &Settings, run: impl FnOnce(&Path, &Path) -> Result<Vec<String>, keeper::Refused>) -> ExitCode {
    if let Err(why) = settings.check_folders() {
        eprintln!("{why}");
        return ExitCode::from(2);
    }
    match run(&settings.state, &settings.data) {
        Ok(said) => {
            for line in said {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    }
}

fn list(data: &Path) -> Vec<String> {
    let found = copies::list(data);
    if found.is_empty() {
        return vec![format!("There are no nightly copies in {} yet. Name a copy to put back: put-back <copy>.", copies::folder(data).display())];
    }
    let mut said = vec!["The nightly copies, newest last. To put one back: put-back <copy>.".to_string()];
    said.extend(found.iter().map(|copy| format!("  {} (made at {} ms)", copy.path.display(), copy.made_at)));
    said
}

fn run(settings: Settings) -> ExitCode {
    if let Err(why) = settings.check_folders() {
        eprintln!("{why}");
        return ExitCode::from(2);
    }
    // The data and the store kept apart, compared before anything is served (design 9.2, 11.2).
    let (keeper, started) = match Keeper::open(&settings.state, &settings.data, now()) {
        Ok(opened) => opened,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::FAILURE;
        }
    };
    println!("{}", started.said());
    let id = keeper.backend();
    nightly(settings.data.clone());
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(why) => {
            eprintln!("The runtime couldn't be started ({}).", why.kind());
            return ExitCode::FAILURE;
        }
    };
    let served = runtime.block_on(async {
        let listener = match serve::listen(settings.listen) {
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
            if started == keeper::Started::Made { "made now" } else { "kept" },
            settings.listen,
            settings.proxy,
            settings.music_server,
            settings.data,
            settings.state,
        );
        let shell = serve::Shell::new(settings.proxy, &nexus_music_server::hello(id));
        serve::serve(listener, shell, true).await;
        ExitCode::SUCCESS
    });
    // The stores are held, and no other program may use them, for as long as it serves.
    keeper.close();
    served
}

/// The nightly copy (design 13.4): looked for at the start and every ten minutes, made when
/// a night has passed since the last, on a thread of its own and from a read of its own.
fn nightly(data: std::path::PathBuf) {
    let spawned = std::thread::Builder::new().name("nightly copy".into()).spawn(move || {
        loop {
            if copies::due(&data, now()) {
                match copies::nightly(&data, now()) {
                    Ok(copy) => println!("The night's copy of the data was made: {}.", copy.path.display()),
                    Err(why) => eprintln!("{why}"),
                }
            }
            std::thread::sleep(copies::LOOK_EVERY);
        }
    });
    if let Err(why) = spawned {
        eprintln!("The nightly copy couldn't be started ({}); there will be no copies until the backend is started again.", why.kind());
    }
}
