use core::ptr::addr_of;

unsafe extern "C" {
    fn host_http_call(
        method_ptr: i32,
        method_len: i32,
        url_ptr: i32,
        url_len: i32,
        headers_ptr: i32,
        headers_len: i32,
        body_ptr: i32,
        body_len: i32,
        result_buf_ptr: i32,
        result_buf_len: i32,
    ) -> i32;
    fn host_set_result(ptr: i32, len: i32);
}

const BUFFER_LEN: usize = 4096;
static mut BUFFER: [u8; BUFFER_LEN] = [0; BUFFER_LEN];

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let method = "GET";
    let url = "http://127.0.0.1:8787/tdata/EchoTests";
    let length = unsafe {
        host_http_call(
            method.as_ptr() as i32,
            method.len() as i32,
            url.as_ptr() as i32,
            url.len() as i32,
            0,
            0,
            0,
            0,
            addr_of!(BUFFER) as *const u8 as i32,
            BUFFER_LEN as i32,
        )
    };
    let allowed = if length > 0 && (length as usize) <= BUFFER_LEN {
        let bytes = unsafe {
            core::slice::from_raw_parts(addr_of!(BUFFER) as *const u8, length as usize)
        };
        bytes.starts_with(b"200\n")
    } else {
        false
    };
    let result = if allowed {
        r#"{"action":"EchoSucceeded","params":{},"success":true}"#
    } else {
        r#"{"action":"EchoFailed","params":{},"success":false,"error":"local TData denied"}"#
    };
    unsafe { host_set_result(result.as_ptr() as i32, result.len() as i32) };
    (!allowed) as i32
}
