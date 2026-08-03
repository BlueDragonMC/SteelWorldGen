use std::collections::HashMap;
use std::env;
use std::io;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use steel_provider::{WorldgenContext, initialize, serialize_chunk_sections};

mod transport;

use transport::{
    Connection, Endpoint, Listener, SocketFileGuard, parse_endpoint, read_exact, write_response,
};

/// Request size (seed + chunk coords, 16 bytes).
const REQUEST_SIZE: usize = 16;

/// How long a [`WorldgenContext`] may sit unused before the sweeper evicts it.
const CONTEXT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How often the sweeper thread scans for idle contexts.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Map of (world seed) to (context and the time the context was last used)
type ContextCache = Mutex<HashMap<u64, (Arc<WorldgenContext>, Instant)>>;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <endpoint>", args[0]);
        eprintln!("  endpoint: a Unix socket path (e.g. /tmp/steel-provider.sock)");
        eprintln!("            or an IP:port pair (e.g. 0.0.0.0:4096)");
        std::process::exit(1);
    }

    initialize();

    match parse_endpoint(&args[1]) {
        Endpoint::Tcp(address) => {
            let listener = TcpListener::bind(&address).expect("failed to bind TCP listener");
            serve(Listener::Tcp(listener), address, None);
        }
        Endpoint::Unix(path) => {
            // Remove any stale socket file left by a previous run.
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("failed to bind Unix socket");

            // Restrict access to the socket to the owning user. The socket is
            // created under a random temporary directory and only read by the
            // parent JVM, but locking down the mode is good hygiene regardless.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }

            let description = path.display().to_string();
            // Owned so the socket file is unlinked on normal exit.
            let guard = SocketFileGuard(Some(path));
            serve(Listener::Unix(listener), description, Some(guard));
        }
    }
}

/// Accept connections until the listener fails, generating chunks for each
/// request. The `_socket_file` guard (if any) is kept alive for the whole
/// lifetime of the server so the Unix socket file is removed on exit.
fn serve(listener: Listener, description: String, _socket_file: Option<SocketFileGuard>) {
    let contexts: Arc<ContextCache> = Arc::new(Mutex::new(HashMap::new()));
    start_context_sweeper(Arc::clone(&contexts));

    eprintln!("steel-provider listening on {description}");

    loop {
        match listener.accept() {
            Ok(connection) => {
                let contexts = Arc::clone(&contexts);
                thread::Builder::new()
                    .name("steelgen-conn".into())
                    // SteelMC's generated code can be stack hungry; give the
                    // handler threads a generous stack.
                    .stack_size(8 * 1024 * 1024)
                    .spawn(move || {
                        if let Err(error) = handle_connection(connection, &contexts) {
                            eprintln!("connection error: {error}");
                        }
                    })
                    .expect("failed to spawn connection thread");
            }
            Err(error) => {
                eprintln!("accept error: {error}");
                break;
            }
        }
    }
}

/// Get (or create) the shared [`WorldgenContext`] for the given seed, updating
/// its last-used time.
///
/// Creation happens outside the lock so a slow first-time world setup doesn't
/// block lookups of already-initialized seeds. If two connections race to
/// create the same seed, the loser is discarded.
fn get_or_create_context(cache: &Arc<ContextCache>, seed: u64) -> Arc<WorldgenContext> {
    {
        let mut cache = cache.lock().unwrap();
        if let Some((ctx, last_used)) = cache.get_mut(&seed) {
            *last_used = Instant::now();
            return Arc::clone(ctx);
        }
    }

    let ctx = Arc::new(WorldgenContext::new(seed));
    let mut cache = cache.lock().unwrap();
    cache
        .entry(seed)
        .or_insert_with(|| (Arc::clone(&ctx), Instant::now()))
        .0
        .clone()
}

/// Periodically evict contexts that have not been used recently.
fn start_context_sweeper(cache: Arc<ContextCache>) {
    thread::Builder::new()
        .name("steelgen-sweep".into())
        .spawn(move || {
            loop {
                thread::sleep(SWEEP_INTERVAL);
                let mut cache = cache.lock().unwrap();
                let now = Instant::now();
                cache.retain(|_, (_, last_used)| {
                    now.saturating_duration_since(*last_used) < CONTEXT_IDLE_TIMEOUT
                });
            }
        })
        .expect("failed to spawn context sweeper thread");
}

/// Handle one client connection until it disconnects. See PROTOCOL.md for
/// a detailed description of the wire format.
fn handle_connection(mut connection: Connection, contexts: &Arc<ContextCache>) -> io::Result<()> {
    let mut request = [0u8; REQUEST_SIZE];

    loop {
        if read_exact(&mut connection, &mut request)? {
            return Ok(()); // peer closed the connection cleanly
        }

        let seed = u64::from_be_bytes(request[0..8].try_into().unwrap());
        let chunk_x = i32::from_be_bytes(request[8..12].try_into().unwrap());
        let chunk_z = i32::from_be_bytes(request[12..16].try_into().unwrap());

        let result = std::panic::catch_unwind(|| {
            let ctx = get_or_create_context(contexts, seed);
            let chunk = ctx.generate_with_structures(chunk_x, chunk_z);
            serialize_chunk_sections(&chunk)
        });

        match result {
            Ok(data) => {
                write_response(&mut connection, 0, &data)?;
            }
            Err(payload) => {
                let message = format!("worldgen panic: {payload:?}");
                eprintln!("{message}");
                write_response(&mut connection, 1, message.as_bytes())?;
            }
        }
    }
}
