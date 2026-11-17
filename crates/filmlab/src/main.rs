// 发布版按 GUI 子系统链接：双击 exe 时不再弹出黑色控制台窗口。
// 调试版保留控制台，方便 `cargo run` 直接看日志。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod session;

use std::sync::mpsc::channel;

use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::{DragDropEvent, WebViewBuilder};

/// 后台线程 → 主线程：要执行的 JS 语句
enum UserEvent {
    Js(String),
    SetTitle(String),
}

const UI_HTML: &str = include_str!("../assets/ui.html");
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// 窗口与任务栏图标：内嵌 PNG → RGBA。解码失败不致命，退回系统默认图标。
fn window_icon() -> Option<tao::window::Icon> {
    let img = image::load_from_memory(ICON_PNG).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    tao::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// GUI 子系统进程默认没有标准输出，`--probe` 之类的调试输出会消失。
/// 只有在"确实没有可用 stdout"（典型是资源管理器双击）时才接回父控制台；
/// 从终端或管道启动时句柄本来就有效，不去覆盖它。
#[cfg(all(windows, not(debug_assertions)))]
fn ensure_stdout() {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX; // -1
    const STD_OUTPUT_HANDLE: u32 = u32::MAX - 10; // -11
    const STD_ERROR_HANDLE: u32 = u32::MAX - 11; // -12
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE: isize = -1;

    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn AttachConsole(process_id: u32) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn SetStdHandle(which: u32, handle: Handle) -> i32;
    }

    unsafe {
        let current = GetStdHandle(STD_OUTPUT_HANDLE);
        if !current.is_null() && current as isize != INVALID_HANDLE {
            return; // 已被重定向到管道/终端，保持原样
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // 父进程没有控制台（双击启动），本来也不需要输出
        }
        let wide: Vec<u16> = "CONOUT$".encode_utf16().chain(std::iter::once(0)).collect();
        let con = CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE | GENERIC_READ,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if !con.is_null() && con as isize != INVALID_HANDLE {
            SetStdHandle(STD_OUTPUT_HANDLE, con);
            SetStdHandle(STD_ERROR_HANDLE, con);
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn ensure_stdout() {}

fn main() -> anyhow::Result<()> {
    ensure_stdout();
    // 调试用：--synth 生成"模拟翻拍"的负片 JPEG；--probe <file> 跑完整读取链路并落盘预览
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--synth") {
        return probe_synth();
    }
    if let Some(pos) = args.iter().position(|a| a == "--basediag") {
        if let Some(path) = args.get(pos + 1) {
            return probe_basediag(path);
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--geom") {
        if let Some(path) = args.get(pos + 1) {
            return probe_geom(path);
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--neg") {
        if let Some(path) = args.get(pos + 1) {
            let ev = args
                .get(pos + 2)
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(0.0);
            let base = match (args.get(pos + 3), args.get(pos + 4), args.get(pos + 5)) {
                (Some(r), Some(g), Some(b)) => r
                    .parse::<f32>()
                    .ok()
                    .zip(g.parse::<f32>().ok())
                    .zip(b.parse::<f32>().ok())
                    .map(|((r, g), b)| [r, g, b]),
                _ => None,
            };
            let frame_k = args
                .iter()
                .position(|a| a == "--frame-k")
                .and_then(|p| args.get(p + 1))
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(film_core::calib::FRAME_K);
            return probe_neg(path, ev, base, frame_k);
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--probe") {
        if let Some(path) = args.get(pos + 1) {
            return probe_run(path);
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--rest") {
        if let Some(path) = args.get(pos + 1) {
            let look = args.get(pos + 2).map(|s| s.as_str()).unwrap_or("natural");
            return probe_rest(path, look);
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--diff") {
        if let (Some(r), Some(n)) = (args.get(pos + 1), args.get(pos + 2)) {
            let look = args.get(pos + 3).map(|s| s.as_str()).unwrap_or("natural");
            return probe_diff(r, n, look);
        }
    }

    // 启动时要打开的内容：`--demo`、或第一个非选项参数（双击/拖到 exe 上打开）。
    // 都没给时默认加载演示负片，保证首次启动就能看到成片，而不是空白画布。
    let startup: Option<String> = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .or_else(|| Some("demo".to_string()));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // 按主显示器尺寸收缩，避免在小屏/高缩放下窗口超出屏幕
    let (mw, mh) = event_loop
        .primary_monitor()
        .map(|m| {
            let s = m.size().to_logical::<f32>(m.scale_factor());
            (s.width, s.height)
        })
        .unwrap_or((1600.0, 1000.0));
    let win_w = (mw * 0.94).min(1440.0);
    let win_h = (mh * 0.90).min(900.0);

    let window = WindowBuilder::new()
        .with_title("FilmLab")
        .with_window_icon(window_icon())
        .with_inner_size(LogicalSize::new(win_w, win_h))
        .with_min_inner_size(LogicalSize::new(
            win_w.min(1040.0),
            win_h.min(660.0),
        ))
        .build(&event_loop)?;
    // IPC / 拖放 → 后台线程
    let (job_tx, job_rx) = channel::<String>();
    let worker_proxy = proxy.clone();
    std::thread::spawn(move || {
        let mut session = session::Session::new();
        // 处理过程可能持续一两秒（解码大图 / 估片基 / 全幅渲染），
        // 每到一个阶段就推一条 progress 给前端，免得界面看起来是卡死的。
        let notify = worker_proxy.clone();
        session.set_progress(std::rc::Rc::new(move |pct: u8, msg: &str| {
            let js = format!(
                "window.__fm({});",
                serde_json::json!({ "type": "progress", "pct": pct, "msg": msg })
            );
            let _ = notify.send_event(UserEvent::Js(js));
        }));
        for body in job_rx {
            let (js, title) = session.handle(&body);
            if worker_proxy.send_event(UserEvent::Js(js)).is_err() {
                break;
            }
            if let Some(t) = title {
                let _ = worker_proxy.send_event(UserEvent::SetTitle(t));
            }
        }
    });

    let ipc_tx = job_tx.clone();
    let drop_tx = job_tx.clone();
    let drop_js = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(UI_HTML)
        .with_ipc_handler(move |req| {
            let _ = ipc_tx.send(req.body().clone());
        })
        .with_drag_drop_handler(move |event: DragDropEvent| {
            if let DragDropEvent::Drop { paths, .. } = event {
                if let Some(path) = paths.first() {
                    // 拖放不走 send()，这里替前端先把"开始处理"的遮罩打开
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let _ = drop_js.send_event(UserEvent::Js(format!(
                        "window.__fm({});",
                        serde_json::json!({ "type": "begin", "title": format!("正在打开 {name}") })
                    )));
                    let body = serde_json::json!({
                        "cmd": "openPath",
                        "path": path.to_string_lossy(),
                    })
                    .to_string();
                    let _ = drop_tx.send(body);
                }
            }
            true
        })
        .build(&window)?;

    let mut startup = startup;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Some(s) = startup.take() {
                    let body = if s == "demo" {
                        serde_json::json!({ "cmd": "demo" }).to_string()
                    } else {
                        serde_json::json!({ "cmd": "openPath", "path": s }).to_string()
                    };
                    let _ = job_tx.send(body);
                }
            }
            Event::UserEvent(UserEvent::Js(js)) => {
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::SetTitle(title)) => window.set_title(&title),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

fn probe_synth() -> anyhow::Result<()> {
    use film_core::image::{clamp01, linear_to_srgb, FloatImage};
    use film_core::synth::{synthetic_negative, SynthOptions};

    let opts = SynthOptions {
        width: 900,
        height: 600,
        rebate: 14,
        ..Default::default()
    };
    let (neg, _pos, _truth) = synthetic_negative(&opts);
    // 模拟"相机翻拍负片存成 JPEG"：线性透过率 → sRGB 编码 → 8-bit
    let mut buf = Vec::with_capacity(neg.len() * 3);
    for i in 0..neg.len() {
        let p = neg.pixel(i);
        for c in 0..3 {
            buf.push((clamp01(linear_to_srgb(p[c])) * 255.0).round() as u8);
        }
    }
    let _ = FloatImage::new(1, 1);
    let path = std::path::Path::new("target/synth-neg.jpg");
    std::fs::create_dir_all("target")?;
    let mut f = std::fs::File::create(path)?;
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut f, 94);
    enc.encode(&buf, neg.width as u32, neg.height as u32, image::ExtendedColorType::Rgb8)?;
    println!("wrote {} ({}×{})", path.display(), neg.width, neg.height);
    Ok(())
}

/// 诊断片基估计：列出边框区域的"橙色平坦块"及其位置/亮度，判断自动检测抓到了什么。
fn probe_basediag(path: &str) -> anyhow::Result<()> {
    use film_core::calib::estimate_base;
    use film_core::image::{FloatImage, Linearize};

    let img = FloatImage::load(std::path::Path::new(path), Linearize::Srgb, 2.2)?;
    let (w, h) = (img.width, img.height);
    let est = estimate_base(&img);
    println!(
        "estimate_base -> {:?} (conf {:.2}, source {:?})",
        est.base, est.confidence, est.source
    );
    let block = 8usize;
    let mx = (w as f32 * 0.15) as usize;
    let my = (h as f32 * 0.15) as usize;
    let mut cands: Vec<(f32, f32, [f32; 3])> = Vec::new();
    for y in (0..h.saturating_sub(block)).step_by(block) {
        for x in (0..w.saturating_sub(block)).step_by(block) {
            let border = x < mx || x + block > w - mx || y < my || y + block > h - my;
            if !border {
                continue;
            }
            let mut mean = [0.0f32; 3];
            let mut m2 = [0.0f32; 3];
            for dy in 0..block {
                for dx in 0..block {
                    let p = img.pixel((y + dy) * w + x + dx);
                    for c in 0..3 {
                        mean[c] += p[c];
                        m2[c] += p[c] * p[c];
                    }
                }
            }
            let cnt = (block * block) as f32;
            let mut flat = true;
            for c in 0..3 {
                mean[c] /= cnt;
                m2[c] /= cnt;
                let std = (m2[c] - mean[c] * mean[c]).max(0.0).sqrt();
                if mean[c] <= 0.02 || mean[c] >= 0.995 || std > 0.03 * mean[c] + 0.002 {
                    flat = false;
                    break;
                }
            }
            if flat && mean[0] > mean[1] * 1.30 && mean[1] > mean[2] * 1.25 {
                cands.push((
                    x as f32 / w as f32,
                    y as f32 / h as f32,
                    mean,
                ));
            }
        }
    }
    println!("orange border blocks = {}", cands.len());
    cands.sort_by(|a, b| b.2[0].partial_cmp(&a.2[0]).unwrap_or(std::cmp::Ordering::Equal));
    for (i, (nx, ny, b)) in cands.iter().take(16).enumerate() {
        println!(
            "#{i:2} at ({:.2},{:.2})  rgb=[{:.3},{:.3},{:.3}]",
            nx, ny, b[0], b[1], b[2]
        );
    }
    Ok(())
}

fn probe_geom(path: &str) -> anyhow::Result<()> {
    let mut s = session::Session::new();
    let steps = [
        ("open", serde_json::json!({ "cmd": "openPath", "path": path })),
        ("rot-cw", serde_json::json!({ "cmd": "rotate", "dir": "cw" })),
        ("rot-ccw", serde_json::json!({ "cmd": "rotate", "dir": "ccw" })),
        ("rot-180", serde_json::json!({ "cmd": "rotate", "dir": "180" })),
        ("crop", serde_json::json!({ "cmd": "crop", "x": 0.25, "y": 0.25, "w": 0.5, "h": 0.4 })),
        // 框定区域只改统计口径，不裁图：尺寸应与旋转后一致
        ("clear", serde_json::json!({ "cmd": "restoreCrop" })),
        // 清掉区域标记后回到未处理的全幅，等用户重新框定
        ("render", serde_json::json!({ "cmd": "render", "params": { "fitGain": false } })),
        ("reset", serde_json::json!({ "cmd": "resetGeom" })),
    ];
    for (label, cmd) in steps {
        let (js, _) = s.handle(&cmd.to_string());
        let v: serde_json::Value = serde_json::from_str(
            js.trim_start_matches("window.__fm(").trim_end_matches(");"),
        )?;
        if v["type"] == "error" {
            println!("{label:9} ERROR {}", v["error"]);
            continue;
        }
        println!(
            "{label:9} type={:8} cropped={:5} needCrop={:5} preview {}×{}  full {}×{}  base={}  {} ms",
            v["type"].as_str().unwrap_or("—"),
            v["cropped"].as_bool().unwrap_or(false),
            v["needCrop"].as_bool().unwrap_or(false),
            v["w"], v["h"], v["fullW"], v["fullH"],
            serde_json::to_string(&v["stats"]["base"]).unwrap_or_default(),
            v["ms"],
        );
    }
    Ok(())
}

/// 新内核（`neg`）的直出验证：只测片基与量程，一次解析，不做任何闭环。
fn probe_neg(
    path: &str,
    ev: f32,
    base: Option<[f32; 3]>,
    frame_k: f32,
) -> anyhow::Result<()> {
    use film_core::image::{FloatImage, Linearize};
    use film_core::neg::Tuning;

    let img = FloatImage::load(std::path::Path::new(path), Linearize::Srgb, 2.2)?;
    let t = Tuning {
        exposure_ev: ev,
        ..Default::default()
    };
    let out = film_core::pipeline::process_neg(&img, t, base, frame_k);
    out.image
        .save_display8(std::path::Path::new("target/neg-ours.jpg"))?;
    println!("neg {}×{}", img.width, img.height);
    println!("base    {:?}", out.samples.base);
    println!("densest {:?}", out.samples.densest);
    println!(
        "gain {:.4}  bias {:.4}  black {:.4}",
        out.neg.gain, out.neg.bias, out.neg.black
    );
    // 密度分布：三通道取最厚层；排除片框外与无光区（片夹遮挡的黑底没有画面信息）。
    let stride = (img.len() / 20_000).max(1);
    let base_lum = film_core::neg::luma(out.samples.base);
    let mut ds: Vec<f32> = (0..img.len())
        .step_by(stride)
        .filter(|i| {
            let p = img.pixel(*i);
            out.alpha[*i] == 0 && film_core::neg::luma(p) > 0.10 * base_lum
        })
        .map(|i| {
            let p = img.pixel(i);
            (0..3)
                .map(|c| (out.neg.base[c] / p[c].max(1e-6)).log10())
                .fold(f32::NEG_INFINITY, f32::max)
        })
        .collect();
    ds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = ds.len();
    println!(
        "D  p10={:.2} p50={:.2} p90={:.2} p99={:.2}",
        ds[n / 10],
        ds[n / 2],
        ds[n * 9 / 10],
        ds[n * 99 / 100]
    );
    let mut os: Vec<f32> = (0..img.len())
        .step_by(stride)
        .filter(|i| {
            let p = img.pixel(*i);
            out.alpha[*i] == 0 && film_core::neg::luma(p) > 0.10 * base_lum
        })
        .map(|i| film_core::neg::luma(out.image.pixel(i)))
        .collect();
    os.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = os.len();
    println!(
        "out p10={:.3} p50={:.3} p90={:.3} p99={:.3}",
        os[m / 10],
        os[m / 2],
        os[m * 9 / 10],
        os[m * 99 / 100]
    );
    Ok(())
}

/// 框定有效区域后的整幅导出：画面不裁切，区域外跟着同一套参数出片，
/// 白底与齿孔应保持纸白。
fn probe_rest(path: &str, look: &str) -> anyhow::Result<()> {
    let mut s = session::Session::new();
    let open = serde_json::json!({ "cmd": "openPath", "path": path }).to_string();
    s.handle(&open);
    let crop = serde_json::json!({
        "cmd": "crop", "x": 0.10, "y": 0.12, "w": 0.80, "h": 0.74
    })
    .to_string();
    s.handle(&crop);
    if look != "natural" {
        let p = serde_json::json!({ "cmd": "render", "params": { "look": look } }).to_string();
        s.handle(&p);
    }
    let v = s.export_jpeg_url()?;
    if let Some(url) = v["data"].as_str() {
        dump_data_url(url, std::path::Path::new("target/probe-rest.jpg"))?;
    }
    println!("rest export {}×{}", v["w"], v["h"]);
    Ok(())
}

fn probe_run(path: &str) -> anyhow::Result<()> {
    let mut s = session::Session::new();
    let body = serde_json::json!({ "cmd": "openPath", "path": path }).to_string();
    let (js, _) = s.handle(&body);
    let inner = js
        .trim_start_matches("window.__fm(")
        .trim_end_matches(");");
    let v: serde_json::Value = serde_json::from_str(inner)?;
    if v["type"] == "error" {
        anyhow::bail!("{}", v["error"].as_str().unwrap_or("?"));
    }
    println!("stats = {}", serde_json::to_string_pretty(&v["stats"])?);
    println!(
        "preview {}×{} (full {}×{}), {} ms",
        v["w"], v["h"], v["fullW"], v["fullH"], v["ms"]
    );
    if let Some(url) = v["jpeg"].as_str() {
        dump_data_url(url, std::path::Path::new("target/probe-positive.jpg"))?;
    }
    if let Some(url) = v["neg"].as_str() {
        dump_data_url(url, std::path::Path::new("target/probe-negative.jpg"))?;
    }

    // 曝光稳健性检查：自动锚定 + 手动曝光偏移，片基仍必须是纯黑
    // （--exposure 默认按"自动中调"模式跑；--manual-exposure 才关掉自动中调）
    let expo = std::env::args()
        .position(|a| a == "--exposure")
        .and_then(|p| std::env::args().nth(p + 1))
        .and_then(|s| s.parse::<f32>().ok());
    if let Some(e) = expo {
        let auto = !std::env::args().any(|a| a == "--manual-exposure");
        let body = serde_json::json!({
            "cmd": "render",
            "params": { "autoExposure": auto, "exposure": e }
        })
        .to_string();
        let (js, _) = s.handle(&body);
        let inner = js.trim_start_matches("window.__fm(").trim_end_matches(");");
        let v2: serde_json::Value = serde_json::from_str(inner)?;
        if let Some(url) = v2["jpeg"].as_str() {
            dump_data_url(url, std::path::Path::new("target/probe-exposure.jpg"))?;
            println!("exposure={e} → target/probe-exposure.jpg");
        } else {
            println!("exposure={e} 渲染失败: {}", v2["error"].as_str().unwrap_or("无 jpeg 且无 error"));
        }
    }

    // 对照：直接用 film-core 的 process()，验证界面与 CLI 是同一口径
    use film_core::image::{FloatImage, Linearize};
    let img = FloatImage::load(std::path::Path::new(path), Linearize::Srgb, 2.2)?;
    let po = film_core::pipeline::process(&img, &film_core::pipeline::ProcessParams::default());
    po.image
        .save_display8(std::path::Path::new("target/pipeline-ref.jpg"))?;
    println!(
        "process(): base={:?} exposure={:.3} median={:?}",
        po.base.base, po.exposure, po.median_dye
    );
    println!(
        "fit: conf={:.2} samples={} resid={:.4} gain={:?}",
        po.fit.confidence, po.fit.samples, po.fit.residual, po.model.gain.g
    );
    println!(
        "neutral: {:?}",
        film_core::pipeline::neutral_median_dye(&img, &po.model)
    );
    println!("d_ref: {:?}", po.print.d_ref);
    println!("anchor lo/med/hi: {:?} / {:?} / {:?}", po.print.anchor_lo, po.print.anchor_med, po.print.anchor_hi);
    println!("anchor_t: {:?}", po.print.anchor_t);
    let a = FloatImage::load(std::path::Path::new("target/probe-positive.jpg"), Linearize::Srgb, 2.2)?;
    let b = FloatImage::load(std::path::Path::new("target/pipeline-ref.jpg"), Linearize::Srgb, 2.2)?;
    if a.width == b.width && a.height == b.height {
        println!("PSNR(session vs pipeline) = {:.2} dB", film_core::metrics::psnr(&a, &b));
    }
    Ok(())
}

/// 只取**画面带**：上下都是片边/片基。参考正片把那片压成纯黑，本软件（`--rest`
/// 口径）把它渲染成纸白——把它们算进统计，逐像素差就会被撑到 ±0.2，完全是假信号。
const PICTURE_BAND: (f32, f32, f32, f32) = (0.12, 0.33, 0.76, 0.38);

/// 参考正片 vs 本软件输出的客观比对：两边都取「画面区域」内的低彩度像素，
/// 比较中性色偏（R-G / G-B）与亮度分布。用来判断"偏绿"到底是哪一段。
fn probe_diff(ref_path: &str, neg_path: &str, look: &str) -> anyhow::Result<()> {
    let mut s = session::Session::new();
    let open = serde_json::json!({ "cmd": "openPath", "path": neg_path }).to_string();
    s.handle(&open);
    let crop = serde_json::json!({
        "cmd": "crop", "x": 0.10, "y": 0.12, "w": 0.80, "h": 0.74
    })
    .to_string();
    s.handle(&crop);
    if !look.is_empty() {
        let p = serde_json::json!({ "cmd": "render", "params": { "look": look } }).to_string();
        s.handle(&p);
    }
    let v = s.export_jpeg_url()?;
    let Some(url) = v["data"].as_str() else {
        anyhow::bail!("导出失败");
    };
    dump_data_url(url, std::path::Path::new("target/diff-ours.jpg"))?;

    let ours = image::open("target/diff-ours.jpg")?.to_rgb8();
    let mut reference = image::open(ref_path)?.to_rgb8();
    // 参考正片是同一帧的手工版：翻拍后整体转了 180°，方向与本软件相反。
    // 转回来才能逐像素比（同一物体对同一物体），否则"同一亮度档"里根本不是同一批像素。
    reference = image::imageops::rotate180(&reference);
    if (reference.width(), reference.height()) != (ours.width(), ours.height()) {
        reference = image::imageops::resize(
            &reference,
            ours.width(),
            ours.height(),
            image::imageops::FilterType::Triangle,
        );
    }
    println!("REF  {}", tone_stats(&reference, PICTURE_BAND));
    println!("OURS {}", tone_stats(&ours, PICTURE_BAND));
    println!("PER-PIXEL ΔOURS-REF (R,G,B)");
    println!("{}", pixel_diff_bins(&reference, &ours, PICTURE_BAND));
    Ok(())
}

/// 按亮度分档统计逐像素差（ours - ref）：同像素同档才有意义。
fn pixel_diff_bins(
    reference: &image::RgbImage,
    ours: &image::RgbImage,
    rect: (f32, f32, f32, f32),
) -> String {
    const LO: [f32; 5] = [0.15, 0.30, 0.45, 0.60, 0.75];
    const HI: [f32; 5] = [0.30, 0.45, 0.60, 0.75, 1.00];
    let (w, h) = (reference.width() as f32, reference.height() as f32);
    let x0 = (rect.0 * w).round().max(0.0) as u32;
    let y0 = (rect.1 * h).round().max(0.0) as u32;
    let x1 = (((rect.0 + rect.2) * w).round() as u32).min(reference.width());
    let y1 = (((rect.1 + rect.3) * h).round() as u32).min(reference.height());
    let step = (((x1 - x0).max(y1 - y0) / 600).max(1)) as u32;
    let mut sum = [[0.0f32; 3]; 5];
    let mut cnt = [0usize; 5];
    for y in (y0..y1).step_by(step as usize) {
        for x in (x0..x1).step_by(step as usize) {
            let rp = reference.get_pixel(x, y).0;
            let op = ours.get_pixel(x, y).0;
            let lum = (rp[0] as f32 + rp[1] as f32 + rp[2] as f32) / 765.0;
            for b in 0..5 {
                if lum >= LO[b] && lum < HI[b] {
                    for j in 0..3 {
                        sum[b][j] += (op[j] as f32 - rp[j] as f32) / 255.0;
                    }
                    cnt[b] += 1;
                }
            }
        }
    }
    let mut s = String::new();
    for b in 0..5 {
        if cnt[b] > 40 {
            let m = [
                sum[b][0] / cnt[b] as f32,
                sum[b][1] / cnt[b] as f32,
                sum[b][2] / cnt[b] as f32,
            ];
            s.push_str(&format!(
                "  bin {:.2}-{:.2}: dR={:+.3} dG={:+.3} dB={:+.3} n={}\n",
                LO[b], HI[b], m[0], m[1], m[2], cnt[b]
            ));
        }
    }
    s
}

struct ToneStats {
    neutral: [f32; 3],
    n: usize,
    p10: f32,
    p50: f32,
    p90: f32,
}

impl std::fmt::Display for ToneStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "neutral=({:.3},{:.3},{:.3}) R-G={:+.3} G-B={:+.3} n={} lum p10/p50/p90={:.1}/{:.1}/{:.1}",
            self.neutral[0],
            self.neutral[1],
            self.neutral[2],
            self.neutral[0] - self.neutral[1],
            self.neutral[1] - self.neutral[2],
            self.n,
            self.p10 * 255.0,
            self.p50 * 255.0,
            self.p90 * 255.0
        )
    }
}

/// 区域内的低彩度（近中性）像素平均色 + 亮度分位。区域为归一化 (x,y,w,h)。
fn tone_stats(img: &image::RgbImage, rect: (f32, f32, f32, f32)) -> ToneStats {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x0 = (rect.0 * w).round().max(0.0) as u32;
    let y0 = (rect.1 * h).round().max(0.0) as u32;
    let x1 = (((rect.0 + rect.2) * w).round() as u32).min(img.width());
    let y1 = (((rect.1 + rect.3) * h).round() as u32).min(img.height());
    let step = (((x1 - x0).max(y1 - y0) / 400).max(1)) as u32;
    let mut sum = [0.0f32; 3];
    let mut n = 0usize;
    let mut lums: Vec<f32> = Vec::new();
    // 按亮度分档看色相：高光偏青与暗部偏绿是两种不同的病，不能混在一起报
    const BIN_LO: [f32; 5] = [0.15, 0.30, 0.45, 0.60, 0.75];
    const BIN_HI: [f32; 5] = [0.30, 0.45, 0.60, 0.75, 1.00];
    let mut bin_sum = [[0.0f32; 3]; 5];
    let mut bin_n = [0usize; 5];
    for y in (y0..y1).step_by(step as usize) {
        for x in (x0..x1).step_by(step as usize) {
            let p = img.get_pixel(x, y).0;
            let v = [
                p[0] as f32 / 255.0,
                p[1] as f32 / 255.0,
                p[2] as f32 / 255.0,
            ];
            let lum = (v[0] + v[1] + v[2]) / 3.0;
            lums.push(lum);
            for b in 0..5 {
                if lum >= BIN_LO[b] && lum < BIN_HI[b] {
                    for j in 0..3 {
                        bin_sum[b][j] += v[j];
                    }
                    bin_n[b] += 1;
                }
            }
            let chroma = v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
            if chroma < 0.06 && (0.30..0.85).contains(&lum) {
                for j in 0..3 {
                    sum[j] += v[j];
                }
                n += 1;
            }
        }
    }
    for b in 0..5 {
        if bin_n[b] > 40 {
            let m = [
                bin_sum[b][0] / bin_n[b] as f32,
                bin_sum[b][1] / bin_n[b] as f32,
                bin_sum[b][2] / bin_n[b] as f32,
            ];
            println!(
                "     bin {:.2}-{:.2}: ({:.3},{:.3},{:.3}) R-G={:+.3} G-B={:+.3} n={}",
                BIN_LO[b],
                BIN_HI[b],
                m[0],
                m[1],
                m[2],
                m[0] - m[1],
                m[1] - m[2],
                bin_n[b]
            );
        }
    }
    let neutral = if n > 0 {
        [sum[0] / n as f32, sum[1] / n as f32, sum[2] / n as f32]
    } else {
        [0.0; 3]
    };
    lums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |q: f32| {
        if lums.is_empty() {
            0.0
        } else {
            lums[(((lums.len() - 1) as f32) * q).round() as usize]
        }
    };
    ToneStats {
        neutral,
        n,
        p10: pick(0.10),
        p50: pick(0.50),
        p90: pick(0.90),
    }
}

fn dump_data_url(url: &str, out: &std::path::Path) -> anyhow::Result<()> {
    use base64::Engine as _;
    let (_, payload) = url
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("bad data url"))?;
    std::fs::write(out, base64::engine::general_purpose::STANDARD.decode(payload)?)?;
    println!("wrote {}", out.display());
    Ok(())
}
