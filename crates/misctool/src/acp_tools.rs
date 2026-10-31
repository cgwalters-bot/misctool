use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

const HELP: &str = "List the commands an ACP adapter offers.

Usage: cgwalters-forge-misctool acp-tools [OPTIONS] -- <ADAPTER> [ARGS...]

Spawns the adapter over stdio, sends ACP initialize and session/new, and waits
for its available_commands_update. Prints the agentCapabilities from initialize
and the offered command names. The adapter's stderr is passed through; it gets
no credentials beyond the environment it inherits.

Options:
  --json             Print {\"agentCapabilities\": ..., \"commands\": [...]}
  --timeout SECONDS  Time allowed for the whole exchange (default 30, max 600)
  -h, --help         Print help

Exits 1 on timeout, adapter exit or a JSON-RPC error; 2 on invalid usage.";

const PROTOCOL_VERSION: u64 = 1;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Bounds the wait so a stuck adapter cannot hold a caller indefinitely.
const MAX_TIMEOUT_SECS: u64 = 600;

const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

const INITIALIZE_ID: u64 = 1;

const SESSION_NEW_ID: u64 = 2;

/// JSON-RPC "method not found", the reply to any request the adapter makes.
const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug, PartialEq)]
struct Options {
    json: bool,
    timeout: Duration,
    adapter: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Run(Options),
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut json = false;
    let mut timeout = DEFAULT_TIMEOUT_SECS;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--json" => json = true,
            "--timeout" => {
                let value = iter.next().ok_or("--timeout requires SECONDS")?;
                timeout = value
                    .parse()
                    .ok()
                    .filter(|secs| (1..=MAX_TIMEOUT_SECS).contains(secs))
                    .ok_or_else(|| {
                        format!("--timeout must be 1 to {MAX_TIMEOUT_SECS} seconds, got {value:?}")
                    })?;
            }
            "--" => {
                let adapter: Vec<String> = iter.cloned().collect();
                if adapter.is_empty() {
                    break;
                }
                return Ok(Parsed::Run(Options {
                    json,
                    timeout: Duration::from_secs(timeout),
                    adapter,
                }));
            }
            other => return Err(format!("unexpected argument {other:?}\n\n{HELP}")),
        }
    }
    Err(format!("expected -- <ADAPTER> [ARGS...]\n\n{HELP}"))
}

/// Bounds the memory one line of adapter output can take.
const MAX_LINE_BYTES: u64 = 1 << 20;

/// Bounds the lines read ahead of the main thread; the reader thread then
/// waits, so a fast adapter cannot grow memory without limit.
const MAX_PENDING_LINES: usize = 16;

/// Bounds the replies queued for an adapter that is not reading its stdin.
const MAX_PENDING_WRITES: usize = 16;

/// What the reader and writer threads report to the main thread.
enum Event {
    Line(String),
    Eof,
    ReadFailed(String),
    WriteFailed(std::io::Error),
}

/// Reads newline-delimited lines of at most MAX_LINE_BYTES.
fn read_lines(stdout: ChildStdout, events: &SyncSender<Event>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        let event = match (&mut reader)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => Event::Eof,
            Ok(_) => {
                // Without a newline this is either the final line, as
                // BufRead::lines() allows, or one over the limit.
                let terminated = line.last() == Some(&b'\n');
                if !terminated && line.len() as u64 > MAX_LINE_BYTES {
                    Event::ReadFailed(format!(
                        "adapter wrote a line longer than {MAX_LINE_BYTES} bytes"
                    ))
                } else {
                    if terminated {
                        line.pop();
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                    }
                    String::from_utf8(line).map_or_else(
                        |_| Event::ReadFailed("adapter wrote a line that is not UTF-8".into()),
                        Event::Line,
                    )
                }
            }
            Err(error) => Event::ReadFailed(format!("could not read from adapter: {error}")),
        };
        let last = !matches!(event, Event::Line(_));
        if events.send(event).is_err() || last {
            break;
        }
    }
}

/// An adapter process speaking newline-delimited JSON-RPC on stdio.
struct Adapter {
    child: Child,
    writes: SyncSender<String>,
    events: Receiver<Event>,
    deadline: Instant,
    timeout: Duration,
}

impl Adapter {
    fn spawn(argv: &[String], timeout: Duration) -> Result<Self, String> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not run adapter {:?}: {error}", argv[0]))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        // Reads and writes happen on their own threads so that the main
        // thread only ever blocks on the deadline, never on the adapter.
        let (event_sender, events) = mpsc::sync_channel(MAX_PENDING_LINES);
        let write_events = event_sender.clone();
        std::thread::spawn(move || read_lines(stdout, &event_sender));
        let (writes, pending) = mpsc::sync_channel::<String>(MAX_PENDING_WRITES);
        std::thread::spawn(move || {
            for message in pending {
                if let Err(error) = stdin
                    .write_all(message.as_bytes())
                    .and_then(|()| stdin.flush())
                {
                    let _ = write_events.send(Event::WriteFailed(error));
                    break;
                }
            }
        });
        Ok(Self {
            child,
            writes,
            events,
            deadline: Instant::now() + timeout,
            timeout,
        })
    }

    /// Describes why the adapter's stdio closed, waiting for its exit status
    /// only until the deadline: closing stdout does not mean it exited.
    fn closed(&mut self, context: &str) -> String {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return format!("adapter exited ({status}) {context}"),
                Ok(None) if Instant::now() < self.deadline => {
                    std::thread::sleep(EXIT_POLL_INTERVAL);
                }
                Ok(None) => return format!("adapter closed its stdio {context}"),
                Err(error) => return format!("could not wait for adapter {context}: {error}"),
            }
        }
    }

    /// Queues `message` for the writer thread; a failed write is reported
    /// as an event, so this never blocks.
    fn send(&mut self, message: &Value) -> Result<(), String> {
        match self.writes.try_send(format!("{message}\n")) {
            // The writer thread stopped after a failed write, which
            // next_message() reports.
            Ok(()) | Err(TrySendError::Disconnected(_)) => Ok(()),
            Err(TrySendError::Full(_)) => Err("adapter is not reading its stdin".to_string()),
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    }

    /// Returns the adapter's next message that is not a request to us;
    /// requests are refused so the adapter does not wait on them.
    fn next_message(&mut self, waiting_for: &str) -> Result<Value, String> {
        loop {
            let timed_out = || {
                format!(
                    "timed out after {}s waiting for {waiting_for}",
                    self.timeout.as_secs()
                )
            };
            // Checked before every message, so a stream of ignored output
            // cannot keep the loop going past the deadline.
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timed_out());
            }
            let line = match self.events.recv_timeout(remaining) {
                Ok(Event::Line(line)) => line,
                Ok(Event::ReadFailed(error)) => {
                    return Err(format!("{error} while waiting for {waiting_for}"));
                }
                Ok(Event::WriteFailed(error)) => {
                    let context = format!("while waiting for {waiting_for}");
                    return Err(format!("{} ({error})", self.closed(&context)));
                }
                Ok(Event::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.closed(&format!("while waiting for {waiting_for}")));
                }
                Err(RecvTimeoutError::Timeout) => return Err(timed_out()),
            };
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line)
                .map_err(|error| format!("adapter wrote invalid JSON-RPC ({error}): {line}"))?;
            match (message.get("id"), message.get("method")) {
                (Some(id), Some(method)) => {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": METHOD_NOT_FOUND, "message": format!("acp-tools does not handle {method}")},
                    });
                    self.send(&reply)?;
                }
                _ => return Ok(message),
            }
        }
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        // The adapter is only probed; stop it rather than wait for it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extracts the result of response `id`, or the JSON-RPC error it carries.
fn response_result(message: &Value, id: u64, method: &str) -> Option<Result<Value, String>> {
    if message.get("id")?.as_u64()? != id {
        return None;
    }
    Some(match message.get("error") {
        Some(error) => Err(format!("{method} failed: {error}")),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    })
}

/// Returns the command names if `message` is an available_commands_update.
fn available_commands(message: &Value) -> Option<Result<Vec<String>, String>> {
    if message.get("method")?.as_str()? != "session/update" {
        return None;
    }
    let update = message.pointer("/params/update")?;
    if update.get("sessionUpdate")?.as_str()? != "available_commands_update" {
        return None;
    }
    let invalid = || format!("malformed available_commands_update: {update}");
    let names = update
        .get("availableCommands")
        .and_then(Value::as_array)
        .ok_or_else(invalid)
        .and_then(|commands| {
            commands
                .iter()
                .map(|command| {
                    command
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(invalid)
                })
                .collect()
        });
    Some(names)
}

fn probe(options: &Options) -> Result<(Value, Vec<String>), String> {
    let cwd = std::env::current_dir()
        .and_then(std::path::absolute)
        .map_err(|error| format!("could not determine the current directory: {error}"))?;
    let mut adapter = Adapter::spawn(&options.adapter, options.timeout)?;
    adapter.request(
        INITIALIZE_ID,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
            },
        }),
    )?;
    let capabilities = loop {
        let message = adapter.next_message("the initialize response")?;
        if let Some(result) = response_result(&message, INITIALIZE_ID, "initialize") {
            break result?
                .get("agentCapabilities")
                .cloned()
                .unwrap_or_else(|| json!({}));
        }
    };
    adapter.request(
        SESSION_NEW_ID,
        "session/new",
        json!({"cwd": cwd, "mcpServers": []}),
    )?;
    // Adapters may send the update before or after the session/new response;
    // an early one is kept until the session is known to exist.
    let mut session_created = false;
    let mut early_commands = None;
    loop {
        let message = adapter.next_message(if session_created {
            "available_commands_update"
        } else {
            "the session/new response"
        })?;
        if let Some(result) = response_result(&message, SESSION_NEW_ID, "session/new") {
            result?;
            session_created = true;
            if let Some(commands) = early_commands.take() {
                return Ok((capabilities, commands));
            }
        } else if let Some(commands) = available_commands(&message) {
            let commands = commands?;
            if session_created {
                return Ok((capabilities, commands));
            }
            early_commands = Some(commands);
        }
    }
}

fn render(json: bool, capabilities: &Value, commands: &[String]) -> String {
    if json {
        return json!({"agentCapabilities": capabilities, "commands": commands}).to_string();
    }
    let mut out = format!("agentCapabilities: {capabilities}\ncommands:");
    for command in commands {
        out.push_str("\n  ");
        out.push_str(command);
    }
    out
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let options = match parse_args(args)? {
        Parsed::Help => {
            println!("{HELP}");
            return Ok(0);
        }
        Parsed::Run(options) => options,
    };
    match probe(&options) {
        Ok((capabilities, commands)) => {
            println!("{}", render(options.json, &capabilities, &commands));
            Ok(0)
        }
        Err(error) => {
            eprintln!("error: {error}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse_args(&args(&[
                "--json",
                "--timeout",
                "5",
                "--",
                "adapter",
                "--json"
            ])),
            Ok(Parsed::Run(Options {
                json: true,
                timeout: Duration::from_secs(5),
                adapter: args(&["adapter", "--json"]),
            }))
        );
        assert_eq!(parse_args(&args(&["--help", "--", "x"])), Ok(Parsed::Help));
        for invalid in [
            &[][..],
            &["--"],
            &["adapter"],
            &["--timeout"],
            &["--timeout", "0", "--", "x"],
            &["--timeout", "601", "--", "x"],
            &["--timeout", "-1", "--", "x"],
        ] {
            assert!(parse_args(&args(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn messages() {
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": {"agentCapabilities": {}}});
        assert_eq!(
            response_result(&ok, 1, "initialize"),
            Some(Ok(json!({"agentCapabilities": {}})))
        );
        assert_eq!(response_result(&ok, 2, "initialize"), None);
        let error = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "no"}});
        assert!(
            matches!(response_result(&error, 1, "initialize"), Some(Err(e)) if e.contains("no"))
        );
        let update =
            |update: Value| json!({"method": "session/update", "params": {"update": update}});
        assert_eq!(
            available_commands(&update(json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{"name": "a"}, {"name": "b", "description": "x"}],
            }))),
            Some(Ok(args(&["a", "b"])))
        );
        assert_eq!(
            available_commands(&update(json!({"sessionUpdate": "agent_message_chunk"}))),
            None
        );
        assert!(matches!(
            available_commands(&update(
                json!({"sessionUpdate": "available_commands_update"})
            )),
            Some(Err(_))
        ));
    }
}
