//! Single instance: a second `markdone` launch hands its files to the running instance
//! (which opens them as tabs and raises its window) and exits.
//!
//! The running instance listens on a local socket: a socket file in the user's
//! runtime directory (`$XDG_RUNTIME_DIR/markdone.sock`, private to the user) on Unix, a
//! per-user named pipe on Windows. A launch first tries to connect: if an instance
//! answers, the files are sent and the launch exits; otherwise it becomes the listener
//! (replacing a socket file left behind by a crash).
//!
//! Protocol: the client sends each path followed by a NUL byte, then an empty record (a
//! lone NUL) as the end; the server answers `ok\n` once it has queued the files. No
//! files means "just bring the window up".

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{ListenerOptions, Name, Stream};

/// How long a launch waits for the running instance to acknowledge its files.
const ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// Outcome of [`claim`].
pub enum Instance {
    /// No other instance: this process serves future launches.
    Primary(LocalSocketListener),
    /// The files were handed to the running instance: this process should exit.
    Forwarded,
    /// Neither (no socket support, or the other instance did not answer): run as an
    /// independent instance.
    Standalone,
}

#[cfg(unix)]
fn socket_name() -> io::Result<Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    let dir = dirs::runtime_dir().unwrap_or_else(|| {
        // No XDG runtime dir: a per-user name in the temporary directory.
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        std::env::temp_dir().join(format!("markdone-{user}"))
    });
    std::fs::create_dir_all(&dir)?;
    dir.join("markdone.sock")
        .to_fs_name::<GenericFilePath>()
        .map(|n| n.into_owned())
}

#[cfg(windows)]
fn socket_name() -> io::Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!("markdone-{user}.sock")
        .to_ns_name::<GenericNamespaced>()
        .map(|n| n.into_owned())
}

/// Hand `paths` (absolute) to a running instance, or become the instance.
pub fn claim(paths: &[PathBuf]) -> Instance {
    let name = match socket_name() {
        Ok(name) => name,
        Err(e) => {
            eprintln!("[markdone] single instance unavailable: {e}");
            return Instance::Standalone;
        }
    };
    if let Ok(stream) = Stream::connect(name.clone()) {
        return match forward(stream, paths) {
            Ok(()) => Instance::Forwarded,
            Err(e) => {
                eprintln!(
                    "[markdone] the running instance did not answer ({e}); starting a new one"
                );
                Instance::Standalone
            }
        };
    }
    // Nobody listening: take the name over (a stale socket file is replaced).
    match ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()
    {
        Ok(listener) => Instance::Primary(listener),
        Err(e) => {
            eprintln!("[markdone] cannot listen for other launches: {e}");
            Instance::Standalone
        }
    }
}

fn forward(mut stream: Stream, paths: &[PathBuf]) -> io::Result<()> {
    stream.write_all(&encode(paths))?;
    stream.flush()?;
    // Wait for the acknowledgement on a helper thread (sync streams have no timeout).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut answer = String::new();
        let result = BufReader::new(stream)
            .read_line(&mut answer)
            .map(|_| answer);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(ACK_TIMEOUT) {
        Ok(Ok(answer)) if answer.trim_end() == "ok" => Ok(()),
        Ok(Ok(answer)) => Err(io::Error::other(format!("unexpected answer {answer:?}"))),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "no answer")),
    }
}

/// Serve launches forever on a background thread: each connection's files go to
/// `on_open` (an empty list = raise the window).
pub fn serve(listener: LocalSocketListener, on_open: impl Fn(Vec<PathBuf>) + Send + 'static) {
    std::thread::Builder::new()
        .name("markdone-instance".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let mut reader = BufReader::new(conn);
                match read_paths(&mut reader) {
                    Ok(paths) => {
                        on_open(paths);
                        let _ = reader.get_mut().write_all(b"ok\n");
                    }
                    Err(e) => eprintln!("[markdone] bad request from another launch: {e}"),
                }
            }
        })
        .expect("spawn the instance listener");
}

fn encode(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for path in paths {
        out.extend_from_slice(path_bytes(path).as_ref());
        out.push(0);
    }
    out.push(0); // end
    out
}

/// Read NUL-terminated paths up to the empty end record.
fn read_paths(reader: &mut impl BufRead) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    loop {
        let mut record = Vec::new();
        let n = reader.read_until(0, &mut record)?;
        if n == 0 || record.last() != Some(&0) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated request",
            ));
        }
        record.pop();
        if record.is_empty() {
            return Ok(paths);
        }
        paths.push(path_from_bytes(record));
        if paths.len() > 10_000 {
            return Err(io::Error::other("too many paths"));
        }
    }
}

#[cfg(unix)]
fn path_bytes(path: &std::path::Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_bytes(path: &std::path::Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(path.to_string_lossy().into_owned().into_bytes())
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip_including_spaces_and_newlines() {
        let paths = vec![
            PathBuf::from("/a/b c.md"),
            PathBuf::from("/x/strano\nnome.md"),
            PathBuf::from("/è/ü.md"),
        ];
        let bytes = encode(&paths);
        assert_eq!(read_paths(&mut &bytes[..]).unwrap(), paths);
        // No files: just the end record.
        assert_eq!(
            read_paths(&mut &encode(&[])[..]).unwrap(),
            Vec::<PathBuf>::new()
        );
        // A truncated request is an error, not a partial open.
        assert!(read_paths(&mut &bytes[..bytes.len() - 1]).is_err());
    }

    /// A real socket: a second launch's files reach the first instance, in order.
    #[cfg(unix)]
    #[test]
    fn a_second_launch_forwards_its_files() {
        use interprocess::local_socket::GenericFilePath;
        let path = std::env::temp_dir().join(format!("markdone-test-{}.sock", std::process::id()));
        let name = path
            .clone()
            .to_fs_name::<GenericFilePath>()
            .unwrap()
            .into_owned();
        let listener = ListenerOptions::new()
            .name(name.clone())
            .try_overwrite(true)
            .create_sync()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        serve(listener, move |paths| {
            let _ = tx.send(paths);
        });
        let sent = vec![PathBuf::from("/tmp/one.md"), PathBuf::from("/tmp/two.md")];
        forward(Stream::connect(name).unwrap(), &sent).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), sent);
        let _ = std::fs::remove_file(path);
    }
}
