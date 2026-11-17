package ai.film;

import android.app.Activity;
import android.content.ContentValues;
import android.content.Intent;
import android.database.Cursor;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.StrictMode;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.text.TextUtils;
import android.util.Base64;
import android.util.Log;
import android.view.View;
import android.webkit.JavascriptInterface;
import android.webkit.WebChromeClient;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.Toast;

import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * 壳：WebView 装 UI，Rust 干算法。
 *
 * 三条铁律：
 * 1. 所有 native 调用走**单线程** Executor——Rust 那边的 Session 是 thread_local，
 *    多线程调用会各自建一份会话，内存直接翻倍到 OOM。
 * 2. 选图必须在 Java 侧缩放后再交给 Rust。原图 24MP 解码成 f32 是 900MB 级别的
 *    峰值，手机扛不住；缩到长边 {@link #MAX_EDGE} 后峰值降到三四百 MB。
 * 3. UI 线程只做 evaluateJavascript，绝不在上面等 native 返回。
 */
public class MainActivity extends Activity {
    private static final String TAG = "FilmMask";
    private static final int REQ_PICK = 1001;
    private static final int REQ_SHOOT = 1002;

    /** 进算法前的长边上限（px）。再大手机容易 OOM，再小则导出的图不够用。 */
    private static final int MAX_EDGE = 2600;

    private WebView web;
    /** 相机的输出目标：API 29+ 是 MediaStore 的 pending 项，老系统是私有目录里的文件。 */
    private Uri shotUri;
    private final ExecutorService worker = Executors.newSingleThreadExecutor();
    private final Handler ui = new Handler(Looper.getMainLooper());

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        web = new WebView(this);
        WebSettings s = web.getSettings();
        s.setJavaScriptEnabled(true);
        s.setDomStorageEnabled(true);
        s.setAllowFileAccess(true);
        s.setMediaPlaybackRequiresUserGesture(false);
        s.setTextZoom(100);
        web.setBackgroundColor(0xFF0A0B0D);
        web.setWebViewClient(new WebViewClient());
        web.setWebChromeClient(new WebChromeClient() {
            @Override
            public boolean onConsoleMessage(android.webkit.ConsoleMessage m) {
                Log.d(TAG, m.message() + " @" + m.lineNumber());
                return true;
            }
        });
        web.addJavascriptInterface(new Bridge(), "fmnative");
        setContentView(web);
        hideSystemBars();
        web.loadUrl("file:///android_asset/ui.html");
    }

    private void hideSystemBars() {
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
            if (getWindow().getInsetsController() != null) {
                getWindow().getInsetsController().hide(
                        android.view.WindowInsets.Type.systemBars());
            }
        } else {
            getWindow().getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                            | View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
        }
    }

    /** JS 侧只认这两个方法，保持与桌面 `window.ipc.postMessage` 等价。 */
    public class Bridge {
        @JavascriptInterface
        public void send(final String json) {
            Log.i(TAG, "bridge.send " + json);
            worker.execute(() -> {
                long t0 = System.currentTimeMillis();
                String js;
                try {
                    js = Native.send(json);
                } catch (Throwable t) {
                    js = fail(t);
                }
                Log.i(TAG, "native.send 返回 " + (js == null ? "null" : js.length() + " 字符")
                        + " / " + (System.currentTimeMillis() - t0) + " ms");
                deliver(js);
            });
        }

        @JavascriptInterface
        public void pick() {
            ui.post(MainActivity.this::pickImage);
        }

        @JavascriptInterface
        public void shoot() {
            ui.post(MainActivity.this::shootImage);
        }
    }

    /**
     * 选图走系统相册的 {@link Intent#ACTION_PICK}，直接落在"图库"界面里；
     * 之前用的 SAF 会把人扔进文件管理器的文档树，手机上很难找。
     * 个别 ROM 没有 PICK 的宿主，退回 GET_CONTENT 兜底。
     */
    private void pickImage() {
        Intent i = new Intent(Intent.ACTION_PICK, MediaStore.Images.Media.EXTERNAL_CONTENT_URI);
        i.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        if (i.resolveActivity(getPackageManager()) == null) {
            i = new Intent(Intent.ACTION_GET_CONTENT);
            i.addCategory(Intent.CATEGORY_OPENABLE);
            i.setType("image/*");
        }
        try {
            startActivityForResult(i, REQ_PICK);
        } catch (Throwable t) {
            toast("没有可用的相册");
        }
    }

    /**
     * 直接拍一张当负片用。相机进程只能往 {@code EXTRA_OUTPUT} 给的 Uri 写，
     * 所以先建一个临时目标：API 29+ 用 MediaStore 的 pending 项，
     * 更老的系统没有这个机制，退回私有目录里的文件（读回后即删）。
     */
    private void shootImage() {
        Intent i = new Intent(MediaStore.ACTION_IMAGE_CAPTURE);
        if (i.resolveActivity(getPackageManager()) == null) {
            toast("这台机器上没有可用的相机");
            return;
        }
        try {
            if (Build.VERSION.SDK_INT >= 29) {
                ContentValues cv = new ContentValues();
                cv.put(MediaStore.MediaColumns.DISPLAY_NAME,
                        "shot_" + System.currentTimeMillis() + ".jpg");
                cv.put(MediaStore.MediaColumns.MIME_TYPE, "image/jpeg");
                cv.put(MediaStore.MediaColumns.RELATIVE_PATH, "Pictures/FilmMask");
                cv.put(MediaStore.MediaColumns.IS_PENDING, 1);
                shotUri = getContentResolver()
                        .insert(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, cv);
                if (shotUri == null) {
                    throw new Exception("建不出拍摄文件");
                }
            } else {
                java.io.File f = new java.io.File(getCacheDir(), "shot.jpg");
                // targetSdk 24+ 默认禁止 file:// 外泄，这里显式放开
                StrictMode.setVmPolicy(new StrictMode.VmPolicy.Builder().build());
                shotUri = Uri.fromFile(f);
            }
            i.putExtra(MediaStore.EXTRA_OUTPUT, shotUri);
            i.addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    | Intent.FLAG_GRANT_READ_URI_PERMISSION);
            startActivityForResult(i, REQ_SHOOT);
        } catch (Throwable t) {
            toast("拉不起相机：" + t.getMessage());
        }
    }

    private void discardShot(Uri uri) {
        if (uri == null) {
            return;
        }
        try {
            if ("content".equals(uri.getScheme())) {
                getContentResolver().delete(uri, null, null);
            } else {
                java.io.File f = new java.io.File(uri.getPath());
                if (f.exists()) {
                    f.delete();
                }
            }
        } catch (Throwable ignored) {
        }
    }

    private void loadAndSend(final Uri uri, final String name, final boolean discardAfter) {
        worker.execute(() -> {
            String js;
            try {
                js = Native.openBytes(shrink(uri), name);
            } catch (Throwable t) {
                js = fail(t);
            }
            if (discardAfter) {
                discardShot(uri);
            }
            deliver(js);
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == REQ_SHOOT) {
            final Uri shot = shotUri;
            shotUri = null;
            if (result != RESULT_OK || shot == null) {
                discardShot(shot);      // 用户按了返回，别在相册里留空文件
                return;
            }
            loadAndSend(shot, "拍摄负片", true);
            return;
        }
        if (request != REQ_PICK || result != RESULT_OK
                || data == null || data.getData() == null) {
            return;
        }
        final Uri uri = data.getData();
        loadAndSend(uri, displayName(uri), false);
    }

    /** 解码 → inSampleSize 粗降 → 精确缩到长边 MAX_EDGE → JPEG。 */
    private byte[] shrink(Uri uri) throws Exception {
        BitmapFactory.Options probe = new BitmapFactory.Options();
        probe.inJustDecodeBounds = true;
        try (InputStream in = getContentResolver().openInputStream(uri)) {
            BitmapFactory.decodeStream(in, null, probe);
        }
        if (probe.outWidth <= 0 || probe.outHeight <= 0) {
            throw new Exception("这个文件解不出图像（RAW 请先在电脑上转成 JPEG）");
        }
        int longEdge = Math.max(probe.outWidth, probe.outHeight);
        int sample = 1;
        while (longEdge / (sample * 2) >= MAX_EDGE) {
            sample *= 2;
        }

        BitmapFactory.Options opt = new BitmapFactory.Options();
        opt.inSampleSize = sample;
        opt.inPreferredConfig = Bitmap.Config.ARGB_8888;
        Bitmap bmp;
        try (InputStream in = getContentResolver().openInputStream(uri)) {
            bmp = BitmapFactory.decodeStream(in, null, opt);
        }
        if (bmp == null) {
            throw new Exception("解码失败，可能是不支持的格式");
        }

        int w = bmp.getWidth();
        int h = bmp.getHeight();
        int le = Math.max(w, h);
        Bitmap out = bmp;
        if (le > MAX_EDGE) {
            float k = (float) MAX_EDGE / le;
            out = Bitmap.createScaledBitmap(bmp,
                    Math.max(1, Math.round(w * k)),
                    Math.max(1, Math.round(h * k)), true);
            if (out != bmp) {
                bmp.recycle();
            }
        }
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        out.compress(Bitmap.CompressFormat.JPEG, 92, bos);
        out.recycle();
        return bos.toByteArray();
    }

    private String displayName(Uri uri) {
        String name = null;
        try (Cursor c = getContentResolver().query(uri, null, null, null, null)) {
            if (c != null && c.moveToFirst()) {
                int idx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (idx >= 0) {
                    name = c.getString(idx);
                }
            }
        } catch (Throwable ignored) {
        }
        return TextUtils.isEmpty(name) ? "image.jpg" : name;
    }

    /**
     * 把 Rust 回的 `window.__fm({...})` 送到 WebView。
     * 导出消息里带的是 JPEG data URL，先落进相册再换回一个人类能读的路径。
     */
    private void deliver(String js) {
        if (js == null) {
            return;
        }
        final String out = rewriteExport(js);
        ui.post(() -> {
            try {
                web.evaluateJavascript(out, null);
            } catch (Throwable t) {
                Log.e(TAG, "evaluateJavascript failed", t);
            }
        });
    }

    private String rewriteExport(String js) {
        String head = "window.__fm(";
        if (!js.startsWith(head) || !js.endsWith(");")) {
            return js;
        }
        try {
            String body = js.substring(head.length(), js.length() - 2);
            JSONObject o = new JSONObject(body);
            String data = o.optString("data", "");
            if (!"exported".equals(o.optString("type", ""))
                    || !data.startsWith("data:image/jpeg;base64,")) {
                return js;
            }
            int comma = data.indexOf(',');
            byte[] jpg = Base64.decode(data.substring(comma + 1), Base64.DEFAULT);
            String name = o.optString("path", "filmmask.jpg");
            String saved = saveToGallery(jpg, name);
            o.remove("data");
            o.put("path", saved);
            return head + o + ");";
        } catch (Throwable t) {
            Log.w(TAG, "rewrite export failed", t);
            return js;
        }
    }

    /** API 29+ 走 MediaStore（相册可见）；更老的直接落应用目录，只提示路径。 */
    private String saveToGallery(byte[] jpg, String name) {
        if (Build.VERSION.SDK_INT >= 29) {
            try {
                ContentValues cv = new ContentValues();
                cv.put(MediaStore.MediaColumns.DISPLAY_NAME, name);
                cv.put(MediaStore.MediaColumns.MIME_TYPE, "image/jpeg");
                cv.put(MediaStore.MediaColumns.RELATIVE_PATH, "Pictures/FilmMask");
                cv.put(MediaStore.MediaColumns.IS_PENDING, 1);
                Uri uri = getContentResolver()
                        .insert(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, cv);
                if (uri == null) {
                    throw new Exception("insert 返回空");
                }
                try (OutputStream os = getContentResolver().openOutputStream(uri)) {
                    os.write(jpg);
                }
                cv.clear();
                cv.put(MediaStore.MediaColumns.IS_PENDING, 0);
                getContentResolver().update(uri, cv, null, null);
                toast("已保存到相册 Pictures/FilmMask");
                return "Pictures/FilmMask/" + name;
            } catch (Throwable t) {
                Log.w(TAG, "MediaStore 写入失败", t);
            }
        }
        try {
            java.io.File dir = new java.io.File(getExternalFilesDir(null), "FilmMask");
            if (!dir.exists() && !dir.mkdirs()) {
                throw new Exception("建目录失败");
            }
            java.io.File f = new java.io.File(dir, name);
            try (java.io.FileOutputStream fos = new java.io.FileOutputStream(f)) {
                fos.write(jpg);
            }
            toast("已保存到 " + f.getAbsolutePath());
            return f.getAbsolutePath();
        } catch (Throwable t) {
            Log.w(TAG, "落盘失败", t);
            toast("保存失败: " + t.getMessage());
            return name;
        }
    }

    private void toast(final String s) {
        ui.post(() -> Toast.makeText(this, s, Toast.LENGTH_SHORT).show());
    }

    private String fail(Throwable t) {
        Log.e(TAG, "native 失败", t);
        try {
            return "window.__fm(" + new JSONObject()
                    .put("type", "error")
                    .put("error", String.valueOf(t.getMessage())) + ");";
        } catch (Throwable ignored) {
            return "window.__fm({\"type\":\"error\",\"error\":\"内部错误\"});";
        }
    }

    @Override
    protected void onDestroy() {
        worker.shutdownNow();
        if (web != null) {
            web.destroy();
        }
        super.onDestroy();
    }
}
