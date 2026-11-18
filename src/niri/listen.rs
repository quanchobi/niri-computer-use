//! An event stream of its own for a background desktop's start, which waits for niri to
//! load its config and for a window to open. Its task reads every line as it comes, since
//! niri drops a reader that falls 64 events behind, and passes on only the two kinds the
//! start waits for, read by their type, so events of kinds this build doesn't know never
//! end it.

use std::path::Path;
use std::time::Duration;

use niri_ipc::{Event, Window};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::events::{DEADLINE, start};
use crate::error::{ErrorName, ToolError};

/// What the task passes on.
#[derive(Debug)]
enum Heard {
    ConfigLoaded {
        failed: bool,
    },
    Window(Box<Window>),
    /// The stream ended, or a line couldn't be read.
    Ended(String),
}

/// The event stream, from the moment niri sent its initial state on.
#[derive(Debug)]
pub(crate) struct Listener {
    heard: mpsc::UnboundedReceiver<Heard>,
    task: JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Listener {
    /// Opens an event stream on the niri at `socket` and reads its initial state, up to the
    /// `ConfigLoaded` that ends it, within `DEADLINE` each.
    pub(crate) async fn open(socket: &Path) -> Result<Self, ToolError> {
        let connect = tokio::time::timeout(DEADLINE, UnixStream::connect(socket)).await;
        let stream = match connect {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return Err(unavailable(format!(
                    "connect to {}: {error}",
                    socket.display()
                )));
            }
            Err(_) => {
                return Err(deadline(format!(
                    "connect to {} within {DEADLINE:?}",
                    socket.display()
                )));
            }
        };
        let mut stream = BufReader::new(stream);
        start(&mut stream).await?;
        let (sender, heard) = mpsc::unbounded_channel();
        let mut listener = Self {
            heard,
            task: tokio::spawn(pass_on(stream, sender)),
        };
        listener.config_loaded(DEADLINE).await?;
        Ok(listener)
    }

    /// Whether niri's next config load, within `limit`, failed.
    pub(crate) async fn config_loaded(&mut self, limit: Duration) -> Result<bool, ToolError> {
        tokio::time::timeout(limit, self.next_config_load())
            .await
            .unwrap_or_else(|_| {
                Err(deadline(format!(
                    "niri reported no config load within {limit:?}"
                )))
            })
    }

    /// The first window of process `pid` that opens or changes within `limit`.
    pub(crate) async fn window_of(
        &mut self,
        pid: u32,
        limit: Duration,
    ) -> Result<Window, ToolError> {
        tokio::time::timeout(limit, self.next_window(pid))
            .await
            .unwrap_or_else(|_| {
                Err(deadline(format!(
                    "no window of process {pid} opened within {limit:?}"
                )))
            })
    }

    async fn next_config_load(&mut self) -> Result<bool, ToolError> {
        loop {
            if let Heard::ConfigLoaded { failed } = self.next().await? {
                return Ok(failed);
            }
        }
    }

    async fn next_window(&mut self, pid: u32) -> Result<Window, ToolError> {
        loop {
            if let Heard::Window(window) = self.next().await?
                && window.pid.and_then(|owner| u32::try_from(owner).ok()) == Some(pid)
            {
                return Ok(*window);
            }
        }
    }

    /// The next event passed on, or why there are no more.
    async fn next(&mut self) -> Result<Heard, ToolError> {
        match self.heard.recv().await {
            Some(Heard::Ended(why)) => Err(unavailable(why)),
            Some(heard) => Ok(heard),
            None => Err(unavailable("the event stream's reader ended")),
        }
    }
}

/// Reads `stream` until it ends, passing on what `heard` makes of each line.
async fn pass_on(mut stream: BufReader<UnixStream>, sender: mpsc::UnboundedSender<Heard>) {
    let mut line = String::new();
    loop {
        line.clear();
        let ended = match stream.read_line(&mut line).await {
            Ok(0) => Some("niri closed the event stream".to_owned()),
            Ok(_) => None,
            Err(error) => Some(format!("read event: {error}")),
        };
        if let Some(why) = ended {
            sender.send(Heard::Ended(why)).ok();
            return;
        }
        if let Some(heard) = heard(&line)
            && sender.send(heard).is_err()
        {
            return;
        }
    }
}

/// The event `line` holds, if it is one of the kinds the listener passes on. Only those
/// are parsed: the others may be large, or of kinds this build doesn't know.
fn heard(line: &str) -> Option<Heard> {
    if !line.starts_with("{\"ConfigLoaded\"") && !line.starts_with("{\"WindowOpenedOrChanged\"") {
        return None;
    }
    match serde_json::from_str(line) {
        Ok(Event::ConfigLoaded { failed }) => Some(Heard::ConfigLoaded { failed }),
        Ok(Event::WindowOpenedOrChanged { window }) => Some(Heard::Window(Box::new(window))),
        Ok(_) | Err(_) => None,
    }
}

fn unavailable(detail: impl Into<String>) -> ToolError {
    ToolError::new(ErrorName::NiriUnavailable, detail)
}

fn deadline(detail: String) -> ToolError {
    ToolError::new(ErrorName::DeadlineExceeded, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_on_config_loads_and_windows_only() {
        assert!(matches!(
            heard("{\"ConfigLoaded\":{\"failed\":true}}\n"),
            Some(Heard::ConfigLoaded { failed: true })
        ));
        let Some(Heard::Window(window)) = heard(&opened(3)) else {
            panic!("no window");
        };
        assert_eq!((window.id, window.pid), (11, Some(3)));
        assert!(heard("{\"WorkspaceActivated\":{\"id\":1,\"focused\":true}}\n").is_none());
        assert!(heard("{\"NotARealEvent\":{}}\n").is_none());
        assert!(heard("{\"ConfigLoaded\":{\"failed\":\"no\"}}\n").is_none());
    }

    /// A `WindowOpenedOrChanged` line for window 11 of process `pid`.
    fn opened(pid: u32) -> String {
        format!(
            "{{\"WindowOpenedOrChanged\":{{\"window\":{{\"id\":11,\"title\":\"niri\",\"app_id\":\"niri\",\"pid\":{pid},\"workspace_id\":1,\"is_focused\":false,\"is_floating\":true,\"is_urgent\":false,\"layout\":{{\"pos_in_scrolling_layout\":null,\"tile_size\":[400.0,300.0],\"window_size\":[400,300],\"tile_pos_in_workspace_view\":[0.0,0.0],\"window_offset_in_tile\":[0.0,0.0]}}}}}}}}\n"
        )
    }

    #[tokio::test]
    async fn waits_past_the_initial_state_for_the_next_config_load_and_the_window() {
        use tokio::io::AsyncWriteExt as _;
        let dir = crate::test_support::fresh_dir("listen");
        let socket = dir.join("niri.sock");
        let niri = tokio::net::UnixListener::bind(&socket).unwrap();
        let lines = [
            "{\"Ok\":\"Handled\"}\n",
            "{\"WorkspacesChanged\":{\"workspaces\":[]}}\n",
            "{\"ConfigLoaded\":{\"failed\":false}}\n",
            &opened(9),
            "{\"ConfigLoaded\":{\"failed\":true}}\n",
            &opened(3),
        ]
        .concat();
        let served = tokio::spawn(async move {
            let (mut stream, _) = niri.accept().await.unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut request)
                .await
                .unwrap();
            stream.write_all(lines.as_bytes()).await.unwrap();
            stream
        });
        let mut listener = Listener::open(&socket).await.unwrap();
        assert_eq!(listener.config_loaded(DEADLINE).await, Ok(true));
        assert_eq!(listener.window_of(3, DEADLINE).await.unwrap().id, 11);
        let error = listener
            .window_of(3, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(error.name, ErrorName::DeadlineExceeded);
        drop(served.await.unwrap());
        let ended = listener.config_loaded(DEADLINE).await.unwrap_err();
        assert_eq!(ended.detail, "niri closed the event stream");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
