//! niri's IPC protocol on newm-river's own socket, exported as `NIRI_SOCKET` to everything
//! newm-river spawns, so `newm-river msg`, `niri msg`, the bar and scripts talk to it unchanged.
//!
//! One JSON request per line, one JSON reply per request; `EventStream` turns the connection into
//! a stream of events. Connections run on their own threads and hand requests to the main loop,
//! which owns the state: [`Server::rx`] carries them, [`Server::wake`] wakes the loop's poll.

use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use niri_ipc::{Event, FloatingFollow, Reply, Request, Response};
use serde::Serialize;

/// What a connection hands the main loop.
pub enum Incoming {
    Request(Request, Sender<Reply>),
    Subscribe(SyncSender<Event>),
}

/// Events queued for one event-stream client at most: a client that stops reading (a stuck bar)
/// is dropped once its queue is full, instead of the queue growing for as long as the session
/// runs (as niri drops it).
pub const EVENT_QUEUE: usize = 4096;

/// How long a write to an event-stream client may block before the connection is given up, so a
/// dropped client's thread ends too.
const EVENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Server {
    pub path: PathBuf,
    pub rx: Receiver<Incoming>,
    pub wake: UnixStream,
    /// The floating-follow mode last written beside the socket.
    floating_follow: Cell<Option<FloatingFollow>>,
}

impl Server {
    pub fn start() -> anyhow::Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
        let display = display.replace('/', "-");
        let own = dir.join(format!("newm-river.{display}.{}.sock", std::process::id()));
        let path = match std::env::var_os("NIRI_SOCKET").map(PathBuf::from) {
            Some(p) if !p.as_os_str().is_empty() && UnixStream::connect(&p).is_err() => p,
            Some(p) if !p.as_os_str().is_empty() => {
                tracing::warn!(
                    "NIRI_SOCKET {} answers: another compositor owns it, using {}",
                    p.display(),
                    own.display()
                );
                own
            }
            _ => own,
        };
        let _ = std::fs::remove_file(&path);
        let listener =
            UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        let (wake, waker) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name("ipc".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                let Ok(waker) = waker.try_clone() else {
                    continue;
                };
                let _ = thread::Builder::new()
                    .name("ipc-client".into())
                    .spawn(move || serve(stream, &tx, waker));
            }
        })?;
        Ok(Self {
            path,
            rx,
            wake,
            floating_follow: Cell::new(None),
        })
    }

    /// The file beside the socket that holds how floating windows follow the camera, for a bar
    /// to show (`$NIRI_SOCKET.floating-follow`: `screen`, `parent` or `canvas`).
    fn floating_follow_path(&self) -> PathBuf {
        let mut path = self.path.clone().into_os_string();
        path.push(".floating-follow");
        path.into()
    }

    /// Writes `mode` to the floating-follow file when it differs from the last one written.
    pub fn write_floating_follow(&self, mode: FloatingFollow) {
        if self.floating_follow.replace(Some(mode)) == Some(mode) {
            return;
        }
        let path = self.floating_follow_path();
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let res = std::fs::write(&tmp, format!("{}\n", mode.name()))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            tracing::warn!("writing {}: {e}", path.display());
        }
    }

    /// Empties the wake-up pipe.
    pub fn drain_wake(&self) {
        let mut buf = [0u8; 64];
        while let Ok(n) = (&self.wake).read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.floating_follow_path());
    }
}

fn write_line<T: Serialize>(out: &mut UnixStream, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(io::Error::other)?;
    line.push('\n');
    out.write_all(line.as_bytes())
}

fn serve(stream: UnixStream, tx: &Sender<Incoming>, mut waker: UnixStream) {
    let Ok(read) = stream.try_clone() else {
        return;
    };
    let mut out = stream;
    for line in BufReader::new(read).lines() {
        let Ok(line) = line else {
            break;
        };
        let reply: Reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => Err(format!("error parsing request: {e}")),
            Ok(Request::EventStream) => {
                let (etx, erx) = mpsc::sync_channel(EVENT_QUEUE);
                let _ = out.set_write_timeout(Some(EVENT_WRITE_TIMEOUT));
                if tx.send(Incoming::Subscribe(etx)).is_err() {
                    return;
                }
                let _ = waker.write_all(&[1]);
                let handled: Reply = Ok(Response::Handled);
                if write_line(&mut out, &handled).is_err() {
                    return;
                }
                for event in erx {
                    if write_line(&mut out, &event).is_err() {
                        return;
                    }
                }
                return;
            }
            Ok(request) => {
                let (rtx, rrx) = mpsc::channel();
                if tx.send(Incoming::Request(request, rtx)).is_err() {
                    return;
                }
                let _ = waker.write_all(&[1]);
                rrx.recv()
                    .unwrap_or_else(|_| Err("newm-river stopped".into()))
            }
        };
        if write_line(&mut out, &reply).is_err() {
            return;
        }
    }
}

/// `newm-river msg`: the requests newm-river answers.
#[derive(clap::Subcommand, Debug)]
pub enum MsgCmd {
    /// newm-river's version.
    Version,
    /// The outputs.
    Outputs,
    /// The workspaces: one canvas per output.
    Workspaces,
    /// The open windows, with their tiles.
    Windows,
    /// The output with the keyboard focus.
    FocusedOutput,
    /// The window with the keyboard focus.
    FocusedWindow,
    /// Do an action.
    Action {
        #[command(subcommand)]
        action: niri_ipc::Action,
    },
    /// Stream events.
    EventStream,
    /// Whether the overview is open.
    OverviewState,
    /// Pick a window with the pointer: the next click answers it, Escape answers nothing.
    PickWindow,
    /// The live settings, with their values and bounds.
    NewmOptions,
    /// Override a live setting until newm-river exits; nothing is written to disk.
    NewmSetOption {
        /// The setting's key, such as `layout.gaps`.
        key: String,
        /// The value as JSON (`20`, `true`, `"canvas"`); text that is not JSON is a string.
        value: String,
    },
    /// Drop the override of a live setting.
    NewmResetOption {
        /// The setting's key.
        key: String,
    },
    /// Drop every live override.
    NewmResetAll,
    /// The live overrides as config file nodes.
    NewmExport,
    /// Launch windows and place them on the active canvas as one block.
    NewmPlaceBlock {
        /// The block as JSON: `{"windows": [{"command", "cwd", "env", "app_id", "rect"}],
        /// "anchor", "focus", "fly"}`.
        block: String,
    },
    /// Which window took each slot of a block.
    NewmBlockStatus {
        /// The block's id.
        id: u64,
    },
    /// Move the active canvas's tiled windows together to just left of the view.
    NewmParkWindows,
    /// Move a tiled window to given tiles of its canvas; windows on them make room.
    NewmPlaceWindow {
        /// The window's id, as `windows` lists it.
        id: u64,
        /// The column of the tile its top-left corner goes to.
        #[arg(allow_negative_numbers = true)]
        column: i32,
        /// The row of that tile.
        #[arg(allow_negative_numbers = true)]
        row: i32,
        /// The span to give it, in tiles: WIDTH HEIGHT. Without it the window keeps its span.
        #[arg(num_args = 2, value_names = ["WIDTH", "HEIGHT"])]
        span: Vec<i32>,
    },
    /// Keep a window out of the saved session (or, with `false`, put it back).
    NewmRestoreExclude {
        /// The window's id, as `windows` lists it.
        id: u64,
        /// Whether the window stays out of the saved session.
        #[arg(default_value_t = true, action = clap::ArgAction::Set)]
        exclude: bool,
    },
}

impl MsgCmd {
    fn request(self) -> anyhow::Result<Request> {
        Ok(match self {
            Self::Version => Request::Version,
            Self::Outputs => Request::Outputs,
            Self::Workspaces => Request::Workspaces,
            Self::Windows => Request::Windows,
            Self::FocusedOutput => Request::FocusedOutput,
            Self::FocusedWindow => Request::FocusedWindow,
            Self::Action { action } => Request::Action(action),
            Self::EventStream => Request::EventStream,
            Self::OverviewState => Request::OverviewState,
            Self::PickWindow => Request::PickWindow,
            Self::NewmOptions => Request::NewmOptions,
            Self::NewmSetOption { key, value } => Request::NewmSetOption {
                key,
                value: serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value)),
            },
            Self::NewmResetOption { key } => Request::NewmResetOption { key },
            Self::NewmResetAll => Request::NewmResetAll,
            Self::NewmExport => Request::NewmExport,
            Self::NewmPlaceBlock { block } => Request::NewmPlaceBlock(
                serde_json::from_str(&block).context("the block is not a block's JSON")?,
            ),
            Self::NewmBlockStatus { id } => Request::NewmBlockStatus { id },
            Self::NewmParkWindows => Request::NewmParkWindows,
            Self::NewmPlaceWindow {
                id,
                column,
                row,
                span,
            } => Request::NewmPlaceWindow {
                id,
                tile: (column, row),
                span: match span[..] {
                    [w, h] => Some((w, h)),
                    _ => None,
                },
            },
            Self::NewmRestoreExclude { id, exclude } => Request::NewmRestoreExclude { id, exclude },
        })
    }
}

/// The payload of a reply: `{"Windows": [...]}` prints as `[...]`.
fn payload(response: &Response) -> anyhow::Result<serde_json::Value> {
    Ok(match serde_json::to_value(response)? {
        serde_json::Value::Object(map) if map.len() == 1 => map.into_iter().next().unwrap().1,
        other => other,
    })
}

/// Sends one request to `$NIRI_SOCKET` and prints the reply (and, for `event-stream`, every
/// event after it).
pub fn msg(cmd: MsgCmd, json: bool) -> anyhow::Result<()> {
    let path = std::env::var_os(niri_ipc::socket::SOCKET_PATH_ENV)
        .context("NIRI_SOCKET is not set: run inside newm-river")?;
    let request = cmd.request()?;
    let stream_events = matches!(request, Request::EventStream);
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to {}", PathBuf::from(&path).display()))?;
    write_line(&mut stream, &request)?;
    if !stream_events {
        stream.shutdown(Shutdown::Write)?;
    }
    let mut lines = BufReader::new(stream).lines();
    let line = lines.next().context("newm-river closed the socket")??;
    let reply: Reply = serde_json::from_str(&line)?;
    let response = match reply {
        Ok(r) => r,
        Err(e) => bail!("newm-river: {e}"),
    };
    let value = payload(&response)?;
    if json {
        println!("{}", serde_json::to_string(&value)?);
    } else if value != serde_json::Value::String("Handled".into()) {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    if stream_events {
        for line in lines {
            println!("{}", line?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_unwrap_the_variant() {
        assert_eq!(
            payload(&Response::Version("1".into())).unwrap(),
            serde_json::json!("1")
        );
        assert_eq!(
            payload(&Response::Handled).unwrap(),
            serde_json::json!("Handled")
        );
    }

    #[test]
    fn requests_parse_as_niri_sends_them() {
        let r: Request =
            serde_json::from_str(r#"{"Action":{"Quit":{"skip_confirmation":true}}}"#).unwrap();
        assert!(matches!(
            r,
            Request::Action(niri_ipc::Action::Quit {
                skip_confirmation: true
            })
        ));
        let r: Request = serde_json::from_str(r#""Windows""#).unwrap();
        assert!(matches!(r, Request::Windows));
    }

    #[test]
    fn live_settings_requests_parse_as_a_settings_app_sends_them() {
        let r: Request =
            serde_json::from_str(r#"{"NewmSetOption":{"key":"layout.gaps","value":20}}"#).unwrap();
        assert!(matches!(
            r,
            Request::NewmSetOption { ref key, ref value } if key == "layout.gaps" && *value == 20
        ));
        let set = MsgCmd::NewmSetOption {
            key: "layout.canvas.floating-follow".into(),
            value: "canvas".into(),
        };
        assert!(matches!(
            set.request(),
            Ok(Request::NewmSetOption { value, .. }) if value == "canvas"
        ));
        let r: Request = serde_json::from_str(r#""NewmExport""#).unwrap();
        assert!(matches!(r, Request::NewmExport));
    }

    #[test]
    fn block_requests_parse_as_a_client_sends_them() {
        let place = MsgCmd::NewmPlaceBlock {
            block: r#"{"windows":[{"command":["foot"],"cwd":null,"env":{"A":"1"},
                "app_id":"foot","rect":[0,0,2,1]}],"anchor":{"Tiles":[3,0]},"focus":0,
                "fly":true}"#
                .into(),
        };
        let Ok(Request::NewmPlaceBlock(block)) = place.request() else {
            panic!("not a block");
        };
        assert_eq!(block.anchor, niri_ipc::NewmBlockAnchor::Tiles(3, 0));
        assert_eq!(block.windows[0].rect, (0, 0, 2, 1));
        assert_eq!(block.windows[0].env["A"], "1");
        let bad = MsgCmd::NewmPlaceBlock { block: "{}".into() };
        assert!(bad.request().is_err());
        let r: Request = serde_json::from_str(r#""NewmParkWindows""#).unwrap();
        assert!(matches!(r, Request::NewmParkWindows));
        let r: Request = serde_json::from_str(r#"{"NewmBlockStatus":{"id":4}}"#).unwrap();
        assert!(matches!(r, Request::NewmBlockStatus { id: 4 }));
        let status: Reply = Ok(Response::NewmBlockStatus {
            id: 4,
            windows: vec![Some(9), None],
            done: false,
        });
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({"Ok": {"NewmBlockStatus": {"id": 4, "windows": [9, null], "done": false}}})
        );
    }

    #[test]
    fn window_requests_parse_as_a_client_sends_them() {
        let r: Request =
            serde_json::from_str(r#"{"NewmPlaceWindow":{"id":42,"tile":[12,-3],"span":[8,6]}}"#)
                .unwrap();
        assert!(matches!(
            r,
            Request::NewmPlaceWindow {
                id: 42,
                tile: (12, -3),
                span: Some((8, 6))
            }
        ));
        let r: Request =
            serde_json::from_str(r#"{"NewmPlaceWindow":{"id":42,"tile":[0,0]}}"#).unwrap();
        assert!(matches!(r, Request::NewmPlaceWindow { span: None, .. }));
        let placed: Reply = Ok(Response::NewmWindowTiles {
            id: 42,
            tiles: (12, -3, 8, 6),
        });
        assert_eq!(
            serde_json::to_value(placed).unwrap(),
            serde_json::json!({"Ok": {"NewmWindowTiles": {"id": 42, "tiles": [12, -3, 8, 6]}}})
        );
        let r: Request =
            serde_json::from_str(r#"{"NewmRestoreExclude":{"id":42,"exclude":true}}"#).unwrap();
        assert!(matches!(
            r,
            Request::NewmRestoreExclude {
                id: 42,
                exclude: true
            }
        ));

        let cmd = MsgCmd::NewmPlaceWindow {
            id: 7,
            column: -2,
            row: 5,
            span: vec![3, 2],
        };
        assert!(matches!(
            cmd.request(),
            Ok(Request::NewmPlaceWindow {
                id: 7,
                tile: (-2, 5),
                span: Some((3, 2))
            })
        ));
        let cmd = MsgCmd::NewmPlaceWindow {
            id: 7,
            column: 0,
            row: 0,
            span: Vec::new(),
        };
        assert!(matches!(
            cmd.request(),
            Ok(Request::NewmPlaceWindow { span: None, .. })
        ));
        let cmd = MsgCmd::NewmRestoreExclude {
            id: 7,
            exclude: false,
        };
        assert!(matches!(
            cmd.request(),
            Ok(Request::NewmRestoreExclude {
                id: 7,
                exclude: false
            })
        ));
    }
}
