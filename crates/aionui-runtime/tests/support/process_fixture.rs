//! Standalone native process used only by subprocess tests. No external tools.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().unwrap();
    if matches!(
        exe.file_stem().and_then(|name| name.to_str()),
        Some("node" | "npm" | "npx")
    ) {
        match args.as_slice() {
            [version] if version == "--version" => println!("v24.11.0"),
            [script, version]
                if version == "--version"
                    && Path::new(script).is_file()
                    && matches!(
                        Path::new(script).file_name().and_then(|name| name.to_str()),
                        Some("npm-cli.js" | "npx-cli.js")
                    ) =>
            {
                println!("11.6.2");
            }
            _ => panic!("unexpected Node fixture arguments: {args:?}"),
        }
        return;
    }

    match args.first().map(String::as_str) {
        Some("env") => {
            for key in &args[1..] {
                println!("{key}:{}", std::env::var(key).unwrap_or_else(|_| "unset".into()));
            }
        }
        Some("echo") => println!("{}", args[1..].join(" ")),
        Some("print") => print!("{}", args[1..].join("")),
        Some("sleep") => std::thread::sleep(Duration::from_millis(args[1].parse().unwrap())),
        Some("non-mcp") => {
            // Read the request first so the test reaches initialize_response,
            // rather than racing a broken pipe while sending initialize.
            let mut request = String::new();
            std::io::stdin().lock().read_line(&mut request).unwrap();
            assert!(request.contains("\"initialize\""));
            std::fs::write(&args[2], &args[1]).unwrap();
            println!("{}", args[1]);
        }
        Some("background") => {
            let mut command = std::process::Command::new(exe);
            command.args(["hold-pipe", &args[1], &args[2], &args[3]]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            // Intentionally let this child outlive its parent, inheriting both
            // output handles. The test releases it; a deadline bounds failures.
            let _child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !Path::new(&args[1]).exists() {
                assert!(Instant::now() < deadline, "background child did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            println!("parent_done");
        }
        Some("hold-pipe") => {
            println!("child_ready");
            std::io::stdout().flush().unwrap();
            std::fs::write(&args[1], b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !Path::new(&args[2]).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let outcome = if Path::new(&args[2]).exists() {
                "released"
            } else {
                "deadline"
            };
            let pending = Path::new(&args[3]).with_extension("pending");
            std::fs::write(&pending, outcome).unwrap();
            // Publish only complete contents. This is the last filesystem
            // operation, so the test may now remove the directory.
            std::fs::rename(pending, &args[3]).unwrap();
        }
        _ => panic!("unexpected fixture arguments: {args:?}"),
    }
}
