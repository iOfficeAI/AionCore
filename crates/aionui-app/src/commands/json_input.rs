//! Shared bounded JSON input for the agent-facing command families.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use clap::Args;
use serde_json::Value;

const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const DEFAULT_STDIN_TIMEOUT_MS: u64 = 1_000;
const MAX_STDIN_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(DEFAULT_STDIN_TIMEOUT_MS);
const OVERALL_READ_TIMEOUT: Duration = Duration::from_secs(30);
const ERROR_PREFIX_BYTES: usize = 16;
const ENV_STDIN_TIMEOUT_MS: &str = "AIONUI_CLI_STDIN_TIMEOUT_MS";

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct JsonInputArgs {
    /// Supply a JSON payload directly. This takes precedence over --input and stdin.
    #[arg(long, global = true, value_name = "JSON")]
    pub json: Option<String>,

    /// Read a JSON payload from PATH, or use - to read stdin.
    #[arg(long, global = true, value_name = "PATH")]
    pub input: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonInputSource {
    Json,
    File,
    Stdin,
}

impl JsonInputSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::File => "file",
            Self::Stdin => "stdin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonInputFailure {
    ReadTimeout,
    ReadFailed,
    TooLarge,
    InvalidEncoding,
    InvalidJson,
}

impl JsonInputFailure {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::ReadTimeout => "timed out while reading JSON input",
            Self::ReadFailed => "failed to read JSON input",
            Self::TooLarge => "JSON input exceeds the 4194304 byte limit",
            Self::InvalidEncoding => "JSON input must be UTF-8 or BOM-marked UTF-16",
            Self::InvalidJson => "JSON input is invalid",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsonInputMetadata {
    pub(crate) source: JsonInputSource,
    pub(crate) bytes_read: usize,
    pub(crate) prefix_hex: String,
}

impl JsonInputMetadata {
    fn from_bytes(source: JsonInputSource, bytes: &[u8]) -> Self {
        let prefix_hex = bytes
            .iter()
            .take(ERROR_PREFIX_BYTES)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            source,
            bytes_read: bytes.len(),
            prefix_hex,
        }
    }

    pub(crate) fn details(&self) -> Value {
        serde_json::json!({
            "source": self.source.as_str(),
            "bytes_read": self.bytes_read,
            "prefix_hex": self.prefix_hex,
        })
    }

    pub(crate) fn fields(&self) -> [(&'static str, String); 3] {
        [
            ("source", self.source.as_str().to_owned()),
            ("bytes_read", self.bytes_read.to_string()),
            ("prefix_hex", self.prefix_hex.clone()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsonInputError {
    pub(crate) failure: JsonInputFailure,
    pub(crate) metadata: JsonInputMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JsonInputDocument {
    pub(crate) value: Value,
    pub(crate) metadata: JsonInputMetadata,
}

pub(crate) fn read_json_input(args: &JsonInputArgs) -> Result<JsonInputDocument, JsonInputError> {
    let (bytes, source) = read_selected_input(args)?;
    parse_bytes(&bytes, source)
}

fn read_selected_input(args: &JsonInputArgs) -> Result<(Vec<u8>, JsonInputSource), JsonInputError> {
    read_selected_input_with(args, read_stdin)
}

fn read_selected_input_with(
    args: &JsonInputArgs,
    read_stdin: impl FnOnce(bool) -> Result<(Vec<u8>, JsonInputSource), JsonInputError>,
) -> Result<(Vec<u8>, JsonInputSource), JsonInputError> {
    if let Some(json) = args.json.as_deref() {
        let bytes = json.as_bytes();
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(JsonInputError {
                failure: JsonInputFailure::TooLarge,
                metadata: JsonInputMetadata::from_bytes(JsonInputSource::Json, bytes),
            });
        }
        return Ok((bytes.to_vec(), JsonInputSource::Json));
    }

    if let Some(path) = args.input.as_deref() {
        if path == Path::new("-") {
            return read_stdin(false);
        }
        return read_file(path).map(|bytes| (bytes, JsonInputSource::File));
    }

    read_stdin(true)
}

fn read_stdin(implicit: bool) -> Result<(Vec<u8>, JsonInputSource), JsonInputError> {
    let stdin = io::stdin();
    read_reader(stdin, JsonInputSource::Stdin, implicit)
}

fn read_reader<R: Read + Send + IsTerminal + 'static>(
    reader: R,
    source: JsonInputSource,
    implicit: bool,
) -> Result<(Vec<u8>, JsonInputSource), JsonInputError> {
    read_reader_with_limits(
        reader,
        source,
        implicit,
        configured_stdin_timeout(),
        OVERALL_READ_TIMEOUT,
    )
}

fn configured_stdin_timeout() -> Duration {
    let value = std::env::var(ENV_STDIN_TIMEOUT_MS).ok();
    stdin_timeout_from_override(value.as_deref())
}

fn stdin_timeout_from_override(value: Option<&str>) -> Duration {
    let milliseconds = value
        .and_then(parse_stdin_timeout_ms)
        .unwrap_or(DEFAULT_STDIN_TIMEOUT_MS);
    Duration::from_millis(milliseconds)
}

fn parse_stdin_timeout_ms(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .map(|milliseconds| milliseconds.min(MAX_STDIN_TIMEOUT_MS))
}

fn read_reader_with_limits<R: Read + Send + IsTerminal + 'static>(
    reader: R,
    source: JsonInputSource,
    implicit: bool,
    first_byte_timeout: Duration,
    overall_timeout: Duration,
) -> Result<(Vec<u8>, JsonInputSource), JsonInputError> {
    let is_terminal = reader.is_terminal();
    match read_bounded(reader, source, is_terminal, first_byte_timeout, overall_timeout) {
        Err(error)
            if implicit
                && source == JsonInputSource::Stdin
                && error.failure == JsonInputFailure::ReadTimeout
                && error.metadata.bytes_read == 0 =>
        {
            Ok((Vec::new(), source))
        }
        result => result.map(|bytes| (bytes, source)),
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, JsonInputError> {
    let path = path.to_owned();
    read_bounded_with(
        JsonInputSource::File,
        DEFAULT_READ_TIMEOUT,
        OVERALL_READ_TIMEOUT,
        move |sender| {
            let Ok(file) = File::open(path) else {
                let _ = sender.send(ReadEvent::Failed);
                return;
            };
            if file.is_terminal() {
                let _ = sender.send(ReadEvent::Eof);
                return;
            }
            read_stream(file, sender);
        },
    )
}

fn read_bounded<R: Read + Send + 'static>(
    reader: R,
    source: JsonInputSource,
    is_terminal: bool,
    first_byte_timeout: Duration,
    overall_timeout: Duration,
) -> Result<Vec<u8>, JsonInputError> {
    if is_terminal {
        return Ok(Vec::new());
    }

    read_bounded_with(source, first_byte_timeout, overall_timeout, move |sender| {
        read_stream(reader, sender);
    })
}

fn read_bounded_with(
    source: JsonInputSource,
    first_byte_timeout: Duration,
    overall_timeout: Duration,
    producer: impl FnOnce(mpsc::Sender<ReadEvent>) + Send + 'static,
) -> Result<Vec<u8>, JsonInputError> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("aioncore-json-input".to_owned())
        .spawn(move || producer(sender))
        .map_err(|_| JsonInputError {
            failure: JsonInputFailure::ReadFailed,
            metadata: JsonInputMetadata::from_bytes(source, &[]),
        })?;

    let mut bytes = Vec::new();
    let first_wait = first_byte_timeout.min(overall_timeout);
    let first_event = receiver.recv_timeout(first_wait).map_err(|error| JsonInputError {
        failure: match error {
            RecvTimeoutError::Timeout => JsonInputFailure::ReadTimeout,
            RecvTimeoutError::Disconnected => JsonInputFailure::ReadFailed,
        },
        metadata: JsonInputMetadata::from_bytes(source, &bytes),
    })?;

    if !append_event(first_event, &mut bytes, source)? {
        return Ok(bytes);
    }
    let deadline = Instant::now() + overall_timeout;

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(JsonInputError {
                failure: JsonInputFailure::ReadTimeout,
                metadata: JsonInputMetadata::from_bytes(source, &bytes),
            });
        };
        if remaining.is_zero() {
            return Err(JsonInputError {
                failure: JsonInputFailure::ReadTimeout,
                metadata: JsonInputMetadata::from_bytes(source, &bytes),
            });
        }
        let event = receiver.recv_timeout(remaining).map_err(|error| JsonInputError {
            failure: match error {
                RecvTimeoutError::Timeout => JsonInputFailure::ReadTimeout,
                RecvTimeoutError::Disconnected => JsonInputFailure::ReadFailed,
            },
            metadata: JsonInputMetadata::from_bytes(source, &bytes),
        })?;
        if !append_event(event, &mut bytes, source)? {
            return Ok(bytes);
        }
    }
}

enum ReadEvent {
    Bytes(Vec<u8>),
    Eof,
    Failed,
}

fn read_stream<R: Read>(mut reader: R, sender: mpsc::Sender<ReadEvent>) {
    let mut total = 0usize;
    loop {
        let remaining = MAX_INPUT_BYTES + 1 - total;
        let mut buffer = vec![0; remaining.min(READ_CHUNK_BYTES)];
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(ReadEvent::Eof);
                return;
            }
            Ok(read) => {
                buffer.truncate(read);
                total += read;
                if sender.send(ReadEvent::Bytes(buffer)).is_err() {
                    return;
                }
                if total > MAX_INPUT_BYTES {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = sender.send(ReadEvent::Failed);
                return;
            }
        }
    }
}

fn append_event(event: ReadEvent, bytes: &mut Vec<u8>, source: JsonInputSource) -> Result<bool, JsonInputError> {
    match event {
        ReadEvent::Bytes(chunk) => {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(JsonInputError {
                    failure: JsonInputFailure::TooLarge,
                    metadata: JsonInputMetadata::from_bytes(source, bytes),
                });
            }
            Ok(true)
        }
        ReadEvent::Eof => Ok(false),
        ReadEvent::Failed => Err(JsonInputError {
            failure: JsonInputFailure::ReadFailed,
            metadata: JsonInputMetadata::from_bytes(source, bytes),
        }),
    }
}

fn parse_bytes(bytes: &[u8], source: JsonInputSource) -> Result<JsonInputDocument, JsonInputError> {
    let metadata = JsonInputMetadata::from_bytes(source, bytes);
    let text = decode_text(bytes).map_err(|()| JsonInputError {
        failure: JsonInputFailure::InvalidEncoding,
        metadata: metadata.clone(),
    })?;
    let value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&text).map_err(|_| JsonInputError {
            failure: JsonInputFailure::InvalidJson,
            metadata: metadata.clone(),
        })?
    };
    Ok(JsonInputDocument { value, metadata })
}

fn decode_text(bytes: &[u8]) -> Result<Cow<'_, str>, ()> {
    if let Some(utf8) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return std::str::from_utf8(utf8).map(Cow::Borrowed).map_err(|_| ());
    }

    if let Some(utf16) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16(utf16, true);
    }
    if let Some(utf16) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16(utf16, false);
    }

    std::str::from_utf8(bytes).map(Cow::Borrowed).map_err(|_| ())
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<Cow<'_, str>, ()> {
    if !bytes.len().is_multiple_of(2) {
        return Err(());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| {
            let pair = [pair[0], pair[1]];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).map(Cow::Owned).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::ffi::{c_char, c_void};
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::fd::FromRawFd;
    use std::sync::mpsc::{self, Receiver};

    use super::*;

    #[test]
    fn inline_json_takes_precedence_over_input_path() {
        let args = JsonInputArgs {
            json: Some(r#"{"source":"json"}"#.to_owned()),
            input: Some(PathBuf::from("does-not-exist.json")),
        };
        let input = read_json_input(&args).unwrap();
        assert_eq!(input.value["source"], "json");
        assert_eq!(input.metadata.source, JsonInputSource::Json);
    }

    #[test]
    fn stdin_timeout_override_uses_default_and_caps_large_values() {
        assert_eq!(stdin_timeout_from_override(None), Duration::from_millis(1_000));
        assert_eq!(
            stdin_timeout_from_override(Some("invalid")),
            Duration::from_millis(1_000)
        );
        assert_eq!(stdin_timeout_from_override(Some("0")), Duration::from_millis(1_000));
        assert_eq!(
            stdin_timeout_from_override(Some("40000")),
            Duration::from_millis(30_000)
        );
    }

    #[test]
    fn input_path_reads_a_json_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aioncore-json-input-{}-{suffix}.json", std::process::id()));
        std::fs::write(&path, br#"{"source":"file"}"#).unwrap();

        let input = read_json_input(&JsonInputArgs {
            json: None,
            input: Some(path.clone()),
        })
        .unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(input.value["source"], "file");
        assert_eq!(input.metadata.source, JsonInputSource::File);
    }

    #[test]
    fn utf8_bom_is_removed_before_json_parsing() {
        let input = parse_bytes(&[0xef, 0xbb, 0xbf, b'{', b'}'], JsonInputSource::Stdin).unwrap();
        assert_eq!(input.value, serde_json::json!({}));
        assert_eq!(input.metadata.bytes_read, 5);
    }

    #[test]
    fn utf16_little_and_big_endian_boms_are_normalized() {
        let little_endian = [0xff, 0xfe, b'{', 0, b'}', 0];
        let big_endian = [0xfe, 0xff, 0, b'{', 0, b'}'];
        assert_eq!(
            parse_bytes(&little_endian, JsonInputSource::File).unwrap().value,
            serde_json::json!({})
        );
        assert_eq!(
            parse_bytes(&big_endian, JsonInputSource::File).unwrap().value,
            serde_json::json!({})
        );
    }

    #[test]
    fn empty_input_becomes_an_empty_object_without_losing_source_metadata() {
        let input = parse_bytes(b" \n\t", JsonInputSource::Stdin).unwrap();
        assert_eq!(input.value, serde_json::json!({}));
        assert_eq!(input.metadata.bytes_read, 3);
        assert_eq!(input.metadata.source, JsonInputSource::Stdin);
    }

    #[test]
    fn an_open_pipe_without_a_first_byte_times_out() {
        let (_sender, receiver) = mpsc::channel();
        let error = read_bounded(
            BlockingReader(receiver),
            JsonInputSource::Stdin,
            false,
            Duration::from_millis(30),
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(error.failure, JsonInputFailure::ReadTimeout);
        assert_eq!(error.metadata.bytes_read, 0);
        assert_eq!(error.metadata.source, JsonInputSource::Stdin);
    }

    #[cfg(unix)]
    #[test]
    fn a_real_open_implicit_stdin_pipe_times_out_as_an_empty_object() {
        let (reader, _writer) = open_pipe();
        let (bytes, source) = read_reader_with_limits(
            reader,
            JsonInputSource::Stdin,
            true,
            Duration::from_millis(30),
            Duration::from_millis(150),
        )
        .unwrap();
        let document = parse_bytes(&bytes, source).unwrap();
        assert_eq!(document.value, serde_json::json!({}));
        assert_eq!(document.metadata.bytes_read, 0);
    }

    #[cfg(unix)]
    #[test]
    fn an_explicit_real_stdin_pipe_timeout_remains_an_error() {
        let (reader, _writer) = open_pipe();
        let error = read_reader_with_limits(
            reader,
            JsonInputSource::Stdin,
            false,
            Duration::from_millis(30),
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(error.failure, JsonInputFailure::ReadTimeout);
        assert_eq!(error.metadata.details()["source"], "stdin");
    }

    #[cfg(unix)]
    #[test]
    fn inline_json_wins_while_a_real_stdin_pipe_remains_open() {
        let (reader, _writer) = open_pipe();
        let args = JsonInputArgs {
            json: Some(r#"{"selected":"json"}"#.to_owned()),
            input: Some(PathBuf::from("-")),
        };
        let mut tried_stdin = false;
        let (bytes, source) = read_selected_input_with(&args, |implicit| {
            tried_stdin = true;
            read_reader_with_limits(
                reader,
                JsonInputSource::Stdin,
                implicit,
                Duration::from_millis(30),
                Duration::from_millis(150),
            )
        })
        .unwrap();
        let document = parse_bytes(&bytes, source).unwrap();
        assert!(!tried_stdin);
        assert_eq!(document.value["selected"], "json");
    }

    #[test]
    fn an_open_pipe_after_its_first_byte_hits_the_overall_deadline() {
        let (sender, receiver) = mpsc::channel();
        sender.send(Some(b'{')).unwrap();
        let error = read_bounded(
            BlockingReader(receiver),
            JsonInputSource::Stdin,
            false,
            Duration::from_millis(100),
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert_eq!(error.failure, JsonInputFailure::ReadTimeout);
        assert_eq!(error.metadata.bytes_read, 1);
        assert_eq!(error.metadata.prefix_hex, "7b");
    }

    #[test]
    fn bounded_reader_rejects_input_larger_than_the_limit() {
        let error = read_bounded(
            Cursor::new(vec![b'x'; MAX_INPUT_BYTES + 1]),
            JsonInputSource::Stdin,
            false,
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(error.failure, JsonInputFailure::TooLarge);
        assert_eq!(error.metadata.bytes_read, MAX_INPUT_BYTES + 1);
        assert_eq!(error.metadata.prefix_hex.split(' ').count(), ERROR_PREFIX_BYTES);
    }

    #[test]
    fn terminal_stdin_is_not_read() {
        let result = read_bounded(
            PanicReader,
            JsonInputSource::Stdin,
            true,
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert!(result.is_empty());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_real_pty_stdin_is_detected_and_returned_without_reading() {
        let (_master, slave) = open_pty();
        assert!(slave.is_terminal());
        let (bytes, source) = read_reader(slave, JsonInputSource::Stdin, true).unwrap();
        let document = parse_bytes(&bytes, source).unwrap();
        assert_eq!(document.value, serde_json::json!({}));
        assert_eq!(document.metadata.bytes_read, 0);
    }

    #[test]
    fn error_metadata_contains_only_source_length_and_a_sixteen_byte_prefix() {
        let error = parse_bytes(b"top secret invalid json", JsonInputSource::Json).unwrap_err();
        assert_eq!(error.failure, JsonInputFailure::InvalidJson);
        let details = error.metadata.details();
        assert_eq!(details.as_object().unwrap().len(), 3);
        assert_eq!(details["source"], "json");
        assert_eq!(details["bytes_read"], 23);
        assert_eq!(details["prefix_hex"].as_str().unwrap().split(' ').count(), 16);
        assert!(!details.to_string().contains("top secret"));
    }

    #[test]
    fn a_regular_reader_reaches_eof_and_returns_complete_input() {
        let input = read_bounded(
            Cursor::new(b"{\"ok\":true}".to_vec()),
            JsonInputSource::File,
            false,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(input, br#"{"ok":true}"#);
    }

    struct BlockingReader(Receiver<Option<u8>>);

    impl Read for BlockingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self.0.recv() {
                Ok(Some(byte)) => {
                    buffer[0] = byte;
                    Ok(1)
                }
                Ok(None) | Err(_) => Ok(0),
            }
        }
    }

    struct PanicReader;

    impl Read for PanicReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            panic!("terminal input must be returned without a read")
        }
    }

    #[cfg(unix)]
    fn open_pipe() -> (File, File) {
        let mut descriptors = [-1; 2];
        let result = unsafe { c_pipe(descriptors.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe creation failed");
        unsafe { (File::from_raw_fd(descriptors[0]), File::from_raw_fd(descriptors[1])) }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn open_pty() -> (File, File) {
        let mut master = -1;
        let mut slave = -1;
        let result = unsafe {
            c_openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(result, 0, "pty creation failed");
        unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
    }

    #[cfg(unix)]
    unsafe extern "C" {
        #[link_name = "pipe"]
        fn c_pipe(descriptors: *mut i32) -> i32;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[link(name = "util")]
    unsafe extern "C" {
        #[link_name = "openpty"]
        fn c_openpty(
            master: *mut i32,
            slave: *mut i32,
            name: *mut c_char,
            termp: *const c_void,
            winp: *const c_void,
        ) -> i32;
    }
}
