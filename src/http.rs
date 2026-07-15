use std::ffi::CString;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use crate::c;
use bch_bindgen::printbuf::Printbuf;

extern crate tiny_http;

fn http_thread(server: tiny_http::Server) {
    use tiny_http::{Response};

    for request in server.incoming_requests() {
        let (_, path) = request.url().split_once('/').unwrap();

        let c_path = CString::new(path).unwrap();

        match request.method() {
            tiny_http::Method::Get => {
                let mut buf = Printbuf::new();

                let ret = unsafe { c::sysfs_read_or_html_dirlist(c_path.as_ptr(), buf.as_raw()) };

                if ret < 0 {
                    let response = Response::from_string(format!("Error {}", ret))
                        .with_status_code(403);
                    request.respond(response).expect("Responded");
                } else {
                    let response = Response::from_string(buf.as_str());
                    request.respond(response).expect("Responded");
                }
            }

            _ => {
                let response = Response::from_string("Unsupported HTTP method")
                    .with_status_code(405);
                request.respond(response).expect("Responded");
            }
        };
    }
}

/*
 * Pick a per-process unix socket path: /run/bcachefs/<pid>.sock for root,
 * $XDG_RUNTIME_DIR/bcachefs/<pid>.sock (typically /run/user/<uid>/...)
 * for unprivileged callers. Caller is responsible for ensuring the
 * parent dir exists (see ensure_socket_dir).
 */
fn http_socket_path() -> String {
    let pid = std::process::id();
    let uid = unsafe { libc::geteuid() };
    let parent = if uid == 0 {
        "/run/bcachefs".to_string()
    } else if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        format!("{}/bcachefs", dir.to_string_lossy())
    } else {
        format!("/run/user/{}/bcachefs", uid)
    };
    format!("{}/{}.sock", parent, pid)
}

/*
 * cleanup_socket runs at process exit (atexit handler). It must work
 * across fork(): the child inherits this registration but binds its
 * own socket at a child-pid-derived path, so we must compute the path
 * from the current pid each time rather than caching it.
 */
extern "C" fn cleanup_socket() {
    let path = http_socket_path();
    let _ = std::fs::remove_file(path);
}

static STARTED_FOR_PID: AtomicU32 = AtomicU32::new(0);
static ATEXIT_REGISTERED: AtomicBool = AtomicBool::new(false);
static ATFORK_REGISTERED: AtomicBool = AtomicBool::new(false);
static INIT_LOCK: Mutex<()> = Mutex::new(());

/*
 * pthread_atfork child handler: bind a fresh socket in the new child
 * process. The parent's http thread doesn't survive fork (only the
 * calling thread does), so the child needs its own server bound at
 * /run/.../<child-pid>.sock.
 */
extern "C" fn child_after_fork() {
    bch2_start_http_lazy();
}

/*
 * Bind a unix socket and spawn a thread serving sysfs/debugfs over HTTP.
 * Idempotent within a process: only the first call per pid actually
 * starts the server.
 *
 * Called from linux/kobject.c's debugfs_create_file shim, so userspace
 * fses (mount, fsck, format, migrate, ...) all expose their debugfs
 * tree without needing to opt in.
 *
 * Fork handling: process-local Once doesn't work — a child of a process
 * that already started the server would skip startup and have no http
 * thread (the parent's thread doesn't survive fork). We track the pid
 * the server started for, and a pthread_atfork child handler re-runs
 * init in any forked child. Each child binds its own socket at
 * /run/.../<child-pid>.sock.
 *
 * Cleanup is via an atexit handler that unlinks the current pid's
 * socket on normal exit. Sockets from killed-by-signal / panicked
 * processes are left behind; user can rm them. Bind fails loudly if a
 * stale file exists rather than overwriting (so we never hijack an
 * in-use path).
 */
#[no_mangle]
pub extern "C" fn bch2_start_http_lazy() {
    let my_pid = std::process::id();
    if STARTED_FOR_PID.load(Ordering::Acquire) == my_pid {
        return;
    }

    let _guard = INIT_LOCK.lock().unwrap();
    if STARTED_FOR_PID.load(Ordering::Relaxed) == my_pid {
        return;
    }

    let path = http_socket_path();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match tiny_http::Server::http_unix(std::path::Path::new(&path)) {
        Ok(server) => {
            // atexit / pthread_atfork registrations are inherited across
            // fork; only register once per process to avoid duplicates.
            if !ATEXIT_REGISTERED.swap(true, Ordering::AcqRel) {
                unsafe { libc::atexit(cleanup_socket); }
            }
            if !ATFORK_REGISTERED.swap(true, Ordering::AcqRel) {
                unsafe {
                    libc::pthread_atfork(None, None, Some(child_after_fork));
                }
            }
            STARTED_FOR_PID.store(my_pid, Ordering::Release);
            std::thread::spawn(move || http_thread(server));
        }
        Err(e) => {
            eprintln!("bcachefs: failed to bind {}: {}", path, e);
        }
    }
}
