package ai.film;

/** 与 Rust cdylib `libfilm_android.so` 的绑定。两个入口都是同步的，调用方负责线程。 */
public final class Native {
    static {
        System.loadLibrary("film_android");
    }

    private Native() {}

    /** 一条 UI 命令，返回 `window.__fm(...)` 形式的 JS。 */
    public static native String send(String json);

    /** 已缩好的 JPEG 字节直接进会话（绕开 WebView 的 data URL 搬运）。 */
    public static native String openBytes(byte[] jpeg, String name);
}
