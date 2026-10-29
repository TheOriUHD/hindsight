//! UI process <-> recorder process over a local named pipe, one JSON request
//! per connection. Keeps the recorder process free of any webview.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hindsight_recorder::Settings;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use windows::core::w;
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::*;

const PIPE: &str = r"\\.\pipe\hindsight-v1";

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Settings,
    SetSettings { settings: Settings },
    SaveClip,
    SetEnabled { enabled: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Response {
    Ok(Value),
    Err(String),
}

/// Serve requests on a background thread; each connection gets its own thread
/// so a slow request (saving a clip) never blocks status polling.
pub fn serve(handler: impl Fn(Request) -> Result<Value> + Send + Sync + 'static) {
    let handler = std::sync::Arc::new(handler);
    std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || loop {
            let h = unsafe {
                CreateNamedPipeW(
                    w!(r"\\.\pipe\hindsight-v1"),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    1 << 16,
                    1 << 16,
                    0,
                    None,
                )
            };
            if h.is_invalid() {
                log::error!("CreateNamedPipe failed");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
                if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                    let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
                    continue;
                }
            }
            let handler = handler.clone();
            let raw = h.0 as usize;
            std::thread::spawn(move || {
                let file = unsafe { File::from_raw_handle(raw as _) };
                let mut line = String::new();
                if BufReader::new(&file).read_line(&mut line).is_err() {
                    return;
                }
                let resp = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => match handler(req) {
                        Ok(v) => Response::Ok(v),
                        Err(e) => Response::Err(format!("{e:#}")),
                    },
                    Err(e) => Response::Err(format!("bad request: {e}")),
                };
                let mut out = serde_json::to_vec(&resp).unwrap_or_default();
                out.push(b'\n');
                let _ = (&file).write_all(&out);
                unsafe {
                    let h = windows::Win32::Foundation::HANDLE(raw as _);
                    let _ = FlushFileBuffers(h);
                    let _ = DisconnectNamedPipe(h);
                }
            });
        })
        .expect("spawn ipc thread");
}

fn connect() -> Result<File> {
    for _ in 0..20 {
        match OpenOptions::new().read(true).write(true).open(PIPE) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                unsafe {
                    let _ = WaitNamedPipeW(w!(r"\\.\pipe\hindsight-v1"), 500);
                }
            }
            Err(e) => return Err(e).context("Hindsight recorder isn't running"),
        }
    }
    bail!("recorder busy")
}

pub fn call<T: for<'de> Deserialize<'de>>(req: &Request) -> Result<T> {
    let mut f = connect()?;
    let mut msg = serde_json::to_vec(req)?;
    msg.push(b'\n');
    f.write_all(&msg)?;
    let mut line = String::new();
    BufReader::new(&f).read_line(&mut line)?;
    match serde_json::from_str::<Response>(&line)? {
        Response::Ok(v) => Ok(serde_json::from_value(v)?),
        Response::Err(e) => bail!(e),
    }
}
