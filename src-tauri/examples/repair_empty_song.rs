#[cfg(feature = "desktop")]
fn main() {
    eprintln!("repair_empty_song requires --no-default-features --features headless");
    std::process::exit(2);
}

#[cfg(not(feature = "desktop"))]
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!("GARDEN_PROFILE_DIR=... repair_empty_song GRAPH_ID EXPECTED_SONG_JSON VERSE_TEXT_FILE");
        std::process::exit(2);
    }
    match garden_lib::song_repair::repair(&args[0], std::path::Path::new(&args[1]), std::path::Path::new(&args[2])) {
        Ok(()) => println!("Repaired only empty Verse 0 text; authority verified, projection publisher reported success."),
        Err(error) => { eprintln!("{error}"); std::process::exit(1); }
    }
}
