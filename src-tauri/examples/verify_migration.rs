//! Inspect persisted migration evidence without starting Garden or writing its profile.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 2 && args[0] == "--archive-evidence" {
        match garden_lib::migration_archive_evidence(std::path::Path::new(&args[1])) {
            Ok(evidence) => println!("{}", serde_json::to_string_pretty(&evidence).unwrap()),
            Err(error) => { eprintln!("observation incomplete: {error}"); std::process::exit(2); }
        }
        return;
    }
    if args.len() != 3 {
        eprintln!("usage: verify_migration GRAPH_DIR GRAPH_ID ARCHIVE.tar.gz");
        std::process::exit(2);
    }
    match garden_lib::verify_migration_files(std::path::Path::new(&args[0]), &args[1], std::path::Path::new(&args[2])) {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if report["summary"]["import_verified"] != true { std::process::exit(1); }
        }
        Err(error) => { eprintln!("verification incomplete: {error}"); std::process::exit(2); }
    }
}
