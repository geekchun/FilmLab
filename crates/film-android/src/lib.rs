//! 安卓 JNI 桥。
//!
//! 桌面版两端都在 Rust：`window.ipc.postMessage` 收命令，主线程把
//! `window.__fm(...)` 注入回 WebView。安卓上这两端都在 Kotlin 侧，所以这层
//! 只做两件事：
//!
//! 1. 把 UI 的 JSON 命令喂给同一个 `Session`，原样返回一段可执行的 JS；
//! 2. 把"选好的图片字节"直接送进会话，绕开 WebView 里 data URL 的搬运。
//!
//! `Session` 持有 `Rc` 和全幅浮点缓存，只能在单线程里跑：Kotlin 侧必须用
//! 单线程 Executor 串行调用，否则 thread_local 会各自持有一份会话（内存翻倍）。

#[path = "../../filmlab/src/session.rs"]
mod session;

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};

use base64::Engine as _;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use serde_json::{json, Value};

thread_local! {
    static SESSION: RefCell<Option<session::Session>> = const { RefCell::new(None) };
}

fn with_session<R>(f: impl FnOnce(&mut session::Session) -> R) -> R {
    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        let sess = slot.get_or_insert_with(session::Session::new);
        f(sess)
    })
}

fn err_js(e: impl std::fmt::Display) -> String {
    format!("window.__fm({});", json!({ "type": "error", "error": format!("{e}") }))
}

fn to_js(env: &mut JNIEnv, msg: String) -> jstring {
    match env.new_string(msg) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 一条 UI 命令 → `window.__fm(...)`。Kotlin 拿到字符串直接 evaluateJavascript。
///
/// `export` 单独分出来：桌面写 16-bit TIFF 到磁盘，安卓没有可写目录可用，
/// 改成回一段 JPEG data URL，由 Kotlin 存进相册。
fn handle_body(body: &str) -> String {
    let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let is_export = req.get("cmd").and_then(|v| v.as_str()) == Some("export");

    if !is_export {
        return with_session(|s| s.handle(body).0);
    }

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let r = with_session(|s| s.export_jpeg_url());
    let mut v = match r {
        Ok(v) => v,
        Err(e) => json!({ "type": "error", "error": format!("{e:#}") }),
    };
    v["id"] = id;
    format!("window.__fm({});", v)
}

#[no_mangle]
pub extern "system" fn Java_ai_film_Native_send<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    input: JString<'local>,
) -> jstring {
    let body = env
        .get_string(&input)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let out = catch_unwind(AssertUnwindSafe(|| handle_body(&body)))
        .unwrap_or_else(|_| err_js("内部异常"));

    to_js(&mut env, out)
}

/// Kotlin 已经按长边缩好的 JPEG 字节直接进会话。
///
/// 走 WebView 里的 `<input type="file">` 会把原图（24MP，几十 MB）读成 base64
/// 再穿一遍 JS，内存和耗时都不可接受，所以选图和缩放全在 Java 侧完成。
#[no_mangle]
pub extern "system" fn Java_ai_film_Native_openBytes<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    bytes: JByteArray<'local>,
    name: JString<'local>,
) -> jstring {
    let data = env.convert_byte_array(&bytes).unwrap_or_default();
    let name = env
        .get_string(&name)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "image".to_string());

    let url = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&data)
    );
    let body = json!({
        "id": 0,
        "cmd": "openData",
        "data": url,
        "name": name,
    })
    .to_string();

    let out = catch_unwind(AssertUnwindSafe(|| handle_body(&body)))
        .unwrap_or_else(|_| err_js("内部异常"));

    to_js(&mut env, out)
}

/// 不经 JVM 的冒烟测试：把"命令 → Session → JS"整条链路在桌面上跑一遍，
/// 这样 JNI 侧只剩字符串搬运，风险面被压到最小。
#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[test]
    fn demo_render_export_round_trip() {
        let js = handle_body(r#"{"id":1,"cmd":"demo"}"#);
        assert!(js.starts_with("window.__fm("), "{js}");

        // 新语义：先框定有效图像区域，之后整幅出片（不裁图）
        let js = handle_body(r#"{"id":2,"cmd":"crop","x":0.1,"y":0.1,"w":0.8,"h":0.8}"#);
        assert!(js.contains("\"type\":\"geom\""), "{js}");
        assert!(js.contains("\"cropped\":true"), "{js}");

        let js = handle_body(r#"{"id":3,"cmd":"render","params":{"exposure":0.2}}"#);
        assert!(js.starts_with("window.__fm("), "{js}");
        assert!(js.contains("\"type\":\"render\""), "{js}");

        let js = handle_body(r#"{"id":4,"cmd":"export"}"#);
        assert!(js.contains("data:image/jpeg;base64,"), "{js}");
        assert!(js.contains("\"id\":4"), "{js}");
    }
}
