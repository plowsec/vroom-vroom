//! One call to a vroom-lab agent endpoint, for agents with a shell:
//! `vroom-rpc --socket S METHOD [PARAMS]` or `vroom-rpc --socket S run COMMAND [ARGS]`;
//! `vroom-rpc --socket S traces [follow]` drains tracepoint and hwatch records as JSON lines.
use clap::Parser;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Call a vroom-lab agent endpoint (JSON-RPC 2.0 over a Unix socket)")]
struct Args {
    /// The lab's --agent-socket.
    #[arg(long)]
    socket: PathBuf,
    /// Agent identity (owns results and jobs).
    #[arg(long, default_value = "vroom-rpc")]
    actor: String,
    /// A method (describe, commands.list, events.poll, jobs.status...), or
    /// `run` to run a command: `run memory.read_virtual '{"address":"0x..."}'`,
    /// or `traces` to drain tracepoint and hwatch records, one JSON line each
    /// (`traces follow`: until interrupted).
    method: String,
    /// JSON params (or, after `run`, the command name then its JSON args).
    rest: Vec<String>,
}

fn call(read: &mut BufReader<UnixStream>, write: &mut UnixStream, id: u64, method: &str, params: Value) -> Result<Value, String> {
    let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    writeln!(write, "{request}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    read.read_line(&mut line).map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_str(&line).map_err(|e| format!("bad reply: {e}"))?;
    match reply.get("error") {
        Some(error) => Err(error.to_string()),
        None => Ok(reply["result"].clone()),
    }
}

fn json_arg(s: Option<&String>) -> Result<Value, String> {
    s.map_or(Ok(json!({})), |s| serde_json::from_str(s).map_err(|e| format!("params: {e}")))
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    let stream = UnixStream::connect(&args.socket).map_err(|e| {
        let hint = match e.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                "\n  nothing listens there: the lab that made it has exited (killed with the shell that started it?).\n  Start it with: vroom-lab CONFIG [--restore SNAPSHOT] --agent-socket PATH --daemon"
            }
            std::io::ErrorKind::NotFound => "\n  no lab socket there yet: start vroom-lab with --agent-socket PATH (--daemon to background it)",
            _ => "",
        };
        format!("{}: {e}{hint}", args.socket.display())
    })?;
    let mut write = stream.try_clone().map_err(|e| e.to_string())?;
    let mut read = BufReader::new(stream);
    call(&mut read, &mut write, 1, "initialize", json!({"actor": args.actor}))?;
    if args.method == "traces" {
        let follow = match args.rest.first().map(String::as_str) {
            None => false,
            Some("follow") => true,
            Some(other) => return Err(format!("traces: expected `follow`, not {other}")),
        };
        let mut out = std::io::stdout().lock();
        for id in 2.. {
            let result = call(&mut read, &mut write, id, "debug.traces", json!({"clear": true, "limit": 1024}))?;
            let records = result["records"].as_array().cloned().unwrap_or_default();
            for record in &records {
                writeln!(out, "{record}").map_err(|e| e.to_string())?;
            }
            if result["dropped"].as_u64().is_some_and(|d| d > 0) && id == 2 {
                eprintln!("vroom-rpc: {} records were dropped before this drain (ring buffer full)", result["dropped"]);
            }
            if result["remaining"].as_u64().is_some_and(|r| r > 0) {
                continue;
            }
            if !follow {
                break;
            }
            out.flush().map_err(|e| e.to_string())?;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        return Ok(());
    }
    let (method, params) = if args.method == "run" {
        let command = args.rest.first().ok_or("run: name a command (see commands.list)")?;
        ("commands.run".to_string(), json!({"command": command, "args": json_arg(args.rest.get(1))?}))
    } else {
        (args.method.clone(), json_arg(args.rest.first())?)
    };
    let result = call(&mut read, &mut write, 2, &method, params)?;
    println!("{}", serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("vroom-rpc: {e}");
        std::process::exit(1);
    }
}
