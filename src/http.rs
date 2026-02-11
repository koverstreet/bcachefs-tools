use std::ffi::{CString, CStr, c_char};
use crate::c;

extern crate tiny_http;

fn http_thread(listen: String) {
    use tiny_http::{Response, Server};

    let server = Server::http(listen).unwrap();

    for request in server.incoming_requests() {
        let (_, path) = request.url().split_once('/').unwrap();

        let c_path = CString::new(path).unwrap();

        match request.method() {
            tiny_http::Method::Get => {
                let mut buf = c::printbuf::new();

                let ret = unsafe { c::sysfs_read_or_html_dirlist(c_path.as_ptr(), &mut buf) };

                if ret < 0 {
                    let response = Response::from_string(format!("Error {}", ret))
                        .with_status_code(403);
                    request.respond(response).expect("Responded");
                } else {
                    let body = if buf.buf.is_null() {
                        String::new()
                    } else {
                        unsafe { CStr::from_ptr(buf.buf) }.to_string_lossy().into_owned()
                    };

                    let response = Response::from_string(body);
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

#[no_mangle]
pub extern "C" fn start_http(listen: *const c_char) {
    let listen = unsafe { CStr::from_ptr(listen) };
    let listen = listen.to_string_lossy().into_owned();

    std::thread::spawn(|| http_thread(listen));
}
