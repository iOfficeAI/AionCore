//! Test-only native subprocess fixture, shared by runtime consumers.
//!
//! Compile once into Cargo's test output directory. The source-keyed cache and
//! OS file lock also share the executable across nextest's isolated processes.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn executable() -> &'static Path {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE.get_or_init(|| {
        let source = include_str!("process_fixture.rs");
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hash);
        let test_exe = std::env::current_exe().expect("test executable");
        let cache = test_exe
            .parent()
            .expect("test executable directory")
            .join("native-process-fixtures")
            .join(format!("{:016x}", hash.finish()));
        std::fs::create_dir_all(&cache).expect("create native fixture cache");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(cache.join("compile.lock"))
            .expect("open fixture compilation lock");
        lock.lock().expect("lock fixture compilation");
        let executable = cache.join(format!("process_fixture{}", std::env::consts::EXE_SUFFIX));
        if !executable.is_file() {
            let source_path = cache.join("process_fixture.rs");
            std::fs::write(&source_path, source).expect("write native fixture source");
            let pending = cache.join(format!("pending{}", std::env::consts::EXE_SUFFIX));
            // This is test infrastructure compiling a standalone std-only
            // fixture, not an application subprocess or an agent executable.
            let mut compiler = std::process::Command::new(option_env!("RUSTC").unwrap_or("rustc"));
            compiler.arg("--edition=2024").arg(&source_path).arg("-o").arg(&pending);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                compiler.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            let output = compiler.output().expect("compile native process fixture");
            assert!(
                output.status.success(),
                "native fixture compilation failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::rename(pending, &executable).expect("publish native process fixture");
        }
        executable
    })
}
