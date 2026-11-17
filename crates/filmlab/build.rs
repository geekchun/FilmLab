// 把 assets/icon.ico 与版本信息嵌进 Windows exe：资源管理器 / 任务栏显示胶卷图标。
//
// 只在 Windows 目标上跑；其它平台（含 Android）不产生资源。
// 改了 icon.ico 后 cargo 会因本文件的 rerun-if-changed 重新链接。
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("ProductName", "FilmLab")
        .set("FileDescription", "FilmLab — 彩色负片去色罩")
        .set("LegalCopyright", "MIT OR Apache-2.0");
    if let Err(e) = res.compile() {
        // 缺少 rc.exe 时不让整个构建失败：图标只是外观，缺了仍可用。
        println!("cargo:warning=嵌入 Windows 图标失败: {e}");
    }
}
