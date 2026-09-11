//! A local HTTP fixture server for browser acceptance.
//!
//! Binds an ephemeral port and serves a fixed route map from a thread. No
//! fixed port, no shared directory, no global state: two of these can run in
//! parallel in the same workspace test run (§41).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct LocalServer {
    pub base: String,
    stop: Arc<AtomicBool>,
}

impl LocalServer {
    pub fn start(routes: HashMap<String, String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let routes = routes.clone();
                        std::thread::spawn(move || serve(stream, &routes));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            stop,
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(mut stream: TcpStream, routes: &HashMap<String, String>) {
    // Windows hands back an accepted socket in the LISTENER's blocking mode,
    // and this listener is non-blocking so the accept loop can poll its stop
    // flag. Unix does not do that. Left alone, the first read here returns
    // WouldBlock, this function gives up, and the connection closes without a
    // byte of response — which the browser reports as
    // ERR_SOCKET_NOT_CONNECTED, on whichever test happened to race.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    // And never let a silent peer hold a fixture thread forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    // Read the rest of the request head. Closing a socket that still has
    // unread data resets the connection instead of finishing it, and the
    // response we just wrote goes with it.
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = path.split('?').next().unwrap_or("/").to_string();
    let (status, body) = match routes.get(&path) {
        Some(b) => ("200 OK", b.clone()),
        None => (
            "404 Not Found",
            "<html><body>not found</body></html>".into(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    // `Connection: close` means the peer closes once it has the body. Send the
    // FIN, then wait for theirs, so the close is graceful on both sides.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = reader.read_to_end(&mut Vec::new());
}

/// Pull the ref out of the snapshot line containing `needle`.
pub fn ref_for(snapshot: &str, needle: &str) -> Option<String> {
    snapshot.lines().find(|l| l.contains(needle)).and_then(|l| {
        let start = l.find('[')? + 1;
        let end = l[start..].find(']')? + start;
        Some(l[start..end].to_string())
    })
}
