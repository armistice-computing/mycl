fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 1 {
        eprintln!(
            "Usage: inspect_maintenance PROFILE_DIRECTORY (read-only; JSON receipt on stdout)"
        );
        std::process::exit(2);
    }
    match garden_lib::maintenance_inspection::inspect_profile(std::path::Path::new(&args[0])) {
        Ok(receipt) => println!("{}", serde_json::to_string_pretty(&receipt).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
