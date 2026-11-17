//! UI 会话：把 `film-core` 的管线包成一组可被 WebView 调用的命令。
//!
//! 缓存策略：片基/中性增益/d_ref/中调密度只在"换图或换片基"时重算，
//! 曝光、反差、CMY、饱和度这些只影响最后一步逐像素印相 → 滑杆拖动只花一次渲染的时间。

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use film_core::calib::{
    estimate_base, estimate_d_ref, estimate_reference, fit_gains, BaseEstimate, BaseSource, GainFit,
};
use film_core::image::{clamp01, linear_to_srgb, FloatImage, Linearize};
use film_core::model::{GainCurve, NegativeModel, StockProfile};
use film_core::print::{AutoTone, PrintParams, MID_TARGET};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 预览长边的目标像素。屏幕按 CSS 像素 × devicePixelRatio 显示，
/// 1228 宽在高 DPI 屏上会被上采样，看着就是"糊"。
const PREVIEW_MAX: usize = 2200;
const PREVIEW_QUALITY: u8 = 88;
const NEG_QUALITY: u8 = 82;

/// 阶段进度回调：百分比 + 一句话说明。
/// 窗口线程用它把"现在卡在哪一步"推给前端，避免打开大图时界面一片死寂。
pub type Progress = Rc<dyn Fn(u8, &str)>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiParams {
    pub auto_base: bool,
    pub base: Option<[f32; 3]>,
    pub fit_gain: bool,
    pub auto_exposure: bool,
    pub exposure: f32,
    pub grade: f32,
    pub cmy: [f32; 3],
    pub wb: [f32; 3],
    pub sat: f32,
    pub gamma: f32,
    /// 出片风格：natural / nlp / vivid / soft / mono
    pub look: String,
    /// 自动色调（Photoshop Auto Tone 口径）：显示域逐通道自动色阶
    pub auto_tone: bool,
    /// 片边门槛：比片基透多少倍算"没有胶片"（白纸/齿孔/灯箱）。
    /// 1.10 是实测默认；调低 = 更多像素判纸白（片边更白、齿孔会外扩），
    /// 调高 = 更少（片边保留片基色，齿孔可能整块消失）。
    pub frame_k: f32,
}

impl Default for UiParams {
    fn default() -> Self {
        Self {
            auto_base: true,
            base: None,
            fit_gain: true,
            auto_exposure: true,
            exposure: 0.0,
            grade: 1.0,
            cmy: [0.0; 3],
            wb: [1.0; 3],
            sat: 0.0,
            gamma: 2.2,
            look: "natural".to_string(),
            auto_tone: true,
            frame_k: film_core::calib::FRAME_K,
        }
    }
}

struct Cached {
    key: String,
    model: NegativeModel,
    /// 判"无胶片区"（白纸/齿孔/灯箱）用的片基读数；找不到片基时退回 model.base
    frame_base: Option<[f32; 3]>,
    /// 中间调灰平衡：逐通道幂次（端点固定），修三层曲线形状造成的中间调偏青
    alpha: [f32; 3],
    base: BaseEstimate,
    fit: GainFit,
    d_ref: [f32; 3],
    median: [f32; 3],
    /// 近中性参考（低彩度像素的逐通道中位染料）
    neutral: [f32; 3],
    /// 每层染料分布的低 / 中 / 高分位（逐层归一的锚点）
    quant: ([f32; 3], [f32; 3], [f32; 3]),
    /// 闭环校准后的中调落点（只跟负片本身有关，由 `ensure` 算一次）
    anchor_t: [f32; 3],
    /// 自动色调参数（结构校准之后的显示域色阶，见 `film_core::print::AutoTone`）
    auto_tone: AutoTone,
}

pub struct Session {
    /// 当前工作图像（可能已被旋转）
    src: Option<FloatImage>,
    /// 刚解码时的原始图像，用于"重置几何"
    orig: Option<FloatImage>,
    /// 框定区域后冻结的校色参数：整幅都按这一套出片，区域内外色调一致。
    frozen: Option<Rc<Cached>>,
    /// 有效图像区域的统计图：校色只认这块区域，片基/白底不进统计。
    stat: Option<FloatImage>,
    /// 还没框定有效图像区域：这时只展示负片原图，不跑任何处理。
    need_crop: bool,
    /// 有效图像区域的归一化矩形。只作标记，**不裁图**：整幅照常出片，
    /// 区域外的片基跟着同一套参数反转，白底与齿孔由 pipeline 判成
    /// 无胶片区直接给纸白（见 film-core 的 `is_frame_outside`）。
    crop_rect: Option<[f32; 4]>,
    preview: Option<FloatImage>,
    factor: usize,
    name: String,
    path: Option<PathBuf>,
    input_lin: &'static str,
    cached: Option<Rc<Cached>>,
    params: UiParams,
    seq: u64,
    out_dir: PathBuf,
    progress: Option<Progress>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            src: None,
            orig: None,
            stat: None,
            need_crop: false,
            crop_rect: None,
            frozen: None,
            preview: None,
            factor: 1,
            name: String::from("未载入图像"),
            path: None,
            input_lin: "sRGB",
            cached: None,
            params: UiParams::default(),
            seq: 0,
            out_dir: PathBuf::from("export"),
            progress: None,
        }
    }

    /// 注册进度回调（窗口线程传入，内部只负责把它转发成一条 JS 消息）。
    pub fn set_progress(&mut self, f: Progress) {
        self.progress = Some(f);
    }

    /// 上报一个阶段。没有回调时是空操作，`--probe` 之类的无头路径不受影响。
    fn step(&self, pct: u8, msg: &str) {
        if let Some(f) = &self.progress {
            f(pct.min(100), msg);
        }
    }

    pub fn handle(&mut self, body: &str) -> (String, Option<String>) {
        let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        let result = match cmd {
            "demo" => self.open_demo(),
            "openPath" => self.open_path(req.get("path").and_then(|v| v.as_str()).unwrap_or("")),
            "openData" => self.open_data(
                req.get("data").and_then(|v| v.as_str()).unwrap_or(""),
                req.get("name").and_then(|v| v.as_str()).unwrap_or("dropped"),
            ),
            "render" => {
                self.read_params(&req);
                if self.need_crop {
                    // 还没框定有效图像区域：处理流程不启动，只回报状态
                    Ok(json!({ "type": "render", "needCrop": true }))
                } else {
                    self.render_preview()
                }
            }
            "rotate" => self.rotate(req.get("dir").and_then(|v| v.as_str()).unwrap_or("cw")),
            "crop" => {
                let f = |k: &str, d: f32| req.get(k).and_then(|v| v.as_f64()).unwrap_or(d as f64) as f32;
                self.crop(f("x", 0.0), f("y", 0.0), f("w", 1.0), f("h", 1.0))
            }
            "restoreCrop" => self.restore_crop(),
            "resetGeom" => self.reset_geom(),
            "export" => self.export(),
            "reset" => {
                self.params = UiParams::default();
                self.cached = None;
                self.frozen = None;
                if self.need_crop {
                    Ok(json!({ "type": "render", "needCrop": true }))
                } else {
                    self.render_preview().map(|mut v| {
                        v["params"] = self.params_json();
                        v
                    })
                }
            }
            _ => Err(anyhow!("未知命令: {cmd}")),
        };

        let mut v = match result {
            Ok(v) => v,
            Err(e) => json!({ "type": "error", "error": format!("{e:#}") }),
        };
        v["id"] = id;
        let title = v
            .get("title")
            .and_then(|t| t.as_str())
            .filter(|_| self.src.is_some())
            .map(|_| format!("FilmLab — {}", self.name));
        (format!("window.__fm({});", v), title)
    }

    // ---------- 载入 ----------

    fn open_demo(&mut self) -> Result<Value> {
        self.step(6, "生成演示负片…");
        let opts = film_core::synth::SynthOptions {
            width: 900,
            height: 600,
            rebate: 14,
            noise: 0.012,
            ..Default::default()
        };
        let (neg, _positive, _truth) = film_core::synth::synthetic_negative(&opts);
        self.assign(neg, "演示负片（合成 C-41）".into(), None, "线性");
        self.loaded_json()
    }

    fn open_path(&mut self, path: &str) -> Result<Value> {
        self.step(4, "读取文件…");
        let p = Path::new(path);
        let dynimg = image::open(p)?;
        self.step(16, "解码图像…");
        let (img, label) = decode_dynamic(&dynimg);
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        self.assign(img, name, Some(p.to_path_buf()), label);
        self.loaded_json()
    }

    fn open_data(&mut self, url: &str, name: &str) -> Result<Value> {
        self.step(4, "解析拖入的图像…");
        let bytes = decode_data_url(url)?;
        let dynimg = image::load_from_memory(&bytes)?;
        self.step(16, "解码图像…");
        let (img, label) = decode_dynamic(&dynimg);
        self.assign(img, name.to_string(), None, label);
        self.loaded_json()
    }

    fn assign(&mut self, img: FloatImage, name: String, path: Option<PathBuf>, lin: &'static str) {
        self.step(24, "生成预览缩略…");
        self.set_source(img.clone());
        self.orig = Some(img);
        self.stat = None;
        self.crop_rect = None;
        // 新图一律先停下等用户框定有效图像区域，绝不在打开时自动处理
        self.need_crop = true;
        self.frozen = None;
        self.name = name;
        self.path = path;
        self.input_lin = lin;
        self.params = UiParams::default();
    }

    /// 换一张"工作图像"（已含几何变换）：重算预览与降采样因子，缓存失效但**保留参数**，
    /// 这样旋转/框定后曝光、CMY 等调整不会被打回默认值。
    fn set_source(&mut self, img: FloatImage) {
        let long = img.width.max(img.height);
        // 取"结果不小于目标长边"的整数因子。原来用 ceil：4912 长边直接掉到
        // 1228（1/4），放大到屏幕就是一片软；改成 floor 得 2456（1/2），
        // 细节多一倍，内存只多 36 MB。
        let factor = (long / PREVIEW_MAX).max(1);
        self.preview = Some(if factor > 1 {
            img.downscale(factor)
        } else {
            img.clone()
        });
        self.factor = factor;
        self.src = Some(img);
        self.seq += 1;
        self.cached = None;
    }

    // ---------- 几何：旋转 / 框定有效区域 ----------

    fn rotate(&mut self, dir: &str) -> Result<Value> {
        self.step(50, "旋转图像…");
        let times = match dir {
            "cw" => 1u8,
            "ccw" => 3,
            "180" => 2,
            other => return Err(anyhow!("未知的旋转方向: {other}")),
        };
        let mut img = {
            let src = self.src.as_ref().ok_or_else(|| anyhow!("未载入图像"))?;
            src.clone()
        };
        for _ in 0..times {
            img = rot90_once(&img);
        }
        // 有效区域跟着一起转，否则旋转后区域统计会落在别处
        if let Some(r) = self.crop_rect {
            self.crop_rect = Some(rotate_rect(r, times));
        }
        self.set_source(img);
        self.geom_json()
    }

    /// 框定「有效图像区域」：只记下这块区域、只用它统计校色参数。
    ///
    /// 图像本身**不裁小**——整幅照常出片，区域外的片基跟着同一套参数反转，
    /// 白底与齿孔由 pipeline 判成无胶片区给纸白，最后输出的是完整一张图。
    fn crop(&mut self, x: f32, y: f32, w: f32, h: f32) -> Result<Value> {
        self.step(50, "框定有效区域…");
        let (rect, area) = {
            let src = self.src.as_ref().ok_or_else(|| anyhow!("未载入图像"))?;
            let iw = src.width as f32;
            let ih = src.height as f32;
            let x0 = ((x.clamp(0.0, 1.0) * iw).round() as usize).min(src.width.saturating_sub(16));
            let y0 = ((y.clamp(0.0, 1.0) * ih).round() as usize).min(src.height.saturating_sub(16));
            let cw = ((w.clamp(0.0, 1.0) * iw).round() as usize)
                .max(16)
                .min(src.width - x0);
            let ch = ((h.clamp(0.0, 1.0) * ih).round() as usize)
                .max(16)
                .min(src.height - y0);
            (
                [
                    x0 as f32 / iw,
                    y0 as f32 / ih,
                    cw as f32 / iw,
                    ch as f32 / ih,
                ],
                crop_rect(src, x0, y0, cw, ch),
            )
        };
        // 统计只取区域内的像素，并降到预览级：片基/白底不参与基准与增益拟合
        let s = (area.width.max(area.height) / PREVIEW_MAX).max(1);
        self.stat = Some(if s > 1 { area.downscale(s) } else { area });
        self.crop_rect = Some(rect);
        self.need_crop = false;
        self.frozen = None;
        self.cached = None;
        if let Ok(c) = self.ensure() {
            self.frozen = Some(c);
        }
        self.geom_json()
    }

    /// 清除区域标记：回到未处理的全幅，等用户重新框定。
    fn restore_crop(&mut self) -> Result<Value> {
        self.step(50, "清除区域标记…");
        self.stat = None;
        self.crop_rect = None;
        self.need_crop = true;
        self.frozen = None;
        self.cached = None;
        self.geom_json()
    }

    fn reset_geom(&mut self) -> Result<Value> {
        self.step(50, "恢复原始几何…");
        let orig = self
            .orig
            .clone()
            .ok_or_else(|| anyhow!("未载入图像，无法重置几何"))?;
        self.set_source(orig);
        self.crop_rect = None;
        // 几何回到最初 ＝ 重新开始，区域标记与统计一并清掉，等用户重新框定
        self.stat = None;
        self.need_crop = true;
        self.frozen = None;
        self.cached = None;
        self.geom_json()
    }

    /// 几何变更后的回包：与载入同一套字段（负片原图 + 渲染结果 + 统计），
    /// 前端只需刷新画面，不用关心是哪种变更。
    fn geom_json(&mut self) -> Result<Value> {
        let mut v = self.loaded_json()?;
        v["type"] = json!("geom");
        // 前端据此显示「重新框定」
        v["cropped"] = json!(self.crop_rect.is_some());
        Ok(v)
    }

    fn loaded_json(&mut self) -> Result<Value> {
        self.step(30, "生成负片预览…");
        let neg = {
            let src = self.src.as_ref().ok_or_else(|| anyhow!("未载入"))?;
            to_data_url(&display_of(src, true), NEG_QUALITY)
        };
        // 注意：w/h 必须是**预览图**的尺寸（前端用它算"适应窗口"的缩放），
        // 全幅尺寸走 fullW/fullH，否则大图会被按原像素数缩小成邮票大小。
        let (pw, ph, fw, fh) = {
            let pv = self.preview.as_ref().ok_or_else(|| anyhow!("未载入"))?;
            let src = self.src.as_ref().ok_or_else(|| anyhow!("未载入"))?;
            (pv.width, pv.height, src.width, src.height)
        };
        // 还没框定有效图像区域：只把负片原图交出去，处理流程等用户框完再跑。
        let mut v = if self.need_crop {
            json!({
                "w": pw,
                "h": ph,
                "fullW": fw,
                "fullH": fh,
                "factor": self.factor,
                "ms": 0,
                "needCrop": true,
                "params": self.params_json(),
            })
        } else {
            let mut v = self.render_preview()?;
            v["needCrop"] = json!(false);
            v
        };
        v["type"] = json!("loaded");
        v["neg"] = json!(neg);
        v["name"] = json!(self.name);
        v["inputLin"] = json!(self.input_lin);
        Ok(v)
    }

    // ---------- 参数 ----------

    fn read_params(&mut self, req: &Value) {
        let Some(p) = req.get("params") else { return };
        match serde_json::from_value::<UiParams>(p.clone()) {
            Ok(new) => {
                // 基准/增益口径变了就得重新统计，冻结的那份不再适用
                if new.auto_base != self.params.auto_base
                    || new.base != self.params.base
                    || new.fit_gain != self.params.fit_gain
                {
                    self.frozen = None;
                }
                self.params = new;
            }
            // 静默失败会让滑杆"看起来拖了但没生效"，必须留痕
            Err(e) => eprintln!("参数解析失败（沿用上一组）：{e}"),
        }
    }

    fn params_json(&self) -> Value {
        serde_json::to_value(&self.params).unwrap_or(Value::Null)
    }

    // ---------- 准备（带缓存）----------

    fn ensure(&mut self) -> Result<Rc<Cached>> {
        let pv = self.preview.as_ref().ok_or_else(|| anyhow!("未载入图像"))?;
        // 统计源是框定出来的「有效图像区域」：整幅都按这块区域的参数出片，
        // 片基/白底不进基准、不进增益拟合（校色只以画面为准）。
        let stats = self.stat.as_ref().unwrap_or(pv);
        // 片边门槛不参与片基/增益拟合，只改变遮罩范围：这里取出，给下面的探针渲染用。
        let frame_k = self.params.frame_k;
        // 自动颜色是在**显示域**量的，所以风格与显示伽马一变就得重算
        let key = format!(
            "{}|{}|{:?}|{}|{}|{}",
            self.seq,
            self.params.auto_base,
            self.params.base,
            self.params.fit_gain,
            self.params.look,
            self.params.gamma
        );
        // 冻结优先：框定区域时算好的那份参数，整幅沿用它。
        // 但 key 里含风格与显示伽马（自动色调依赖它们），换风格必须重算，
        // 否则黑白风格会沿用彩色风格算出的逐通道端点，灰又带上色偏。
        if let Some(f) = &self.frozen {
            if f.key == key {
                return Ok(f.clone());
            }
        }
        if let Some(c) = &self.cached {
            if c.key == key {
                return Ok(c.clone());
            }
        }

        self.step(36, "估计画面基准…");
        // 密度归一化用画面自身的高分位：框定区域里就是画面，不会被片框带偏
        let base = match (self.params.auto_base, self.params.base) {
            (false, Some(b)) => BaseEstimate {
                base: b,
                confidence: 1.0,
                source: BaseSource::Screen,
            },
            _ => estimate_reference(stats),
        };
        // 遮罩判据另用片基：只有"比未曝光片基更透"才真是白纸/齿孔/灯箱。
        // 图不裁切，片基还在框外，全幅里找得到；找不到（纯画面）就不遮罩。
        let frame_base = match (self.params.auto_base, self.params.base) {
            (false, Some(b)) => Some(b),
            _ => {
                let est = estimate_base(pv);
                (est.confidence >= 0.3).then_some(est.base)
            }
        };

        let stock = StockProfile::c41_generic();
        let model0 = NegativeModel::from_matrix(base.base, &stock.a_prior, stock.d_ref)
            .ok_or_else(|| anyhow!("档案矩阵不可逆"))?;

        self.step(52, "拟合中性增益…");
        let fit = if self.params.fit_gain {
            fit_gains(stats, &model0, 0.02)
        } else {
            GainFit {
                gain: GainCurve::default(),
                confidence: 0.0,
                samples: 0,
                residual: 0.0,
            }
        };
        let model = NegativeModel {
            gain: fit.gain.clone(),
            ..model0
        };

        // 相纸锚点必须以**这张负片的实际最大密度**为准：薄负片（密度范围只有 1 个 D
        // 左右）用档案 2.1 做锚点，印相反差会严重不足，整幅画必然发灰。
        // 逐通道取染料高分位并夹到合理范围。
        self.step(64, "测量最大密度…");
        let est = estimate_d_ref(stats, &model);
        let d_all = ((est[0] + est[1] + est[2]) / 3.0).clamp(0.55, 3.60);
        let d_ref = [d_all; 3];
        let _ = stock;

        self.step(74, "统计染料分布…");
        let median = median_dye(stats, &model);
        let neutral = film_core::pipeline::neutral_median_dye(stats, &model);
        let refs = film_core::pipeline::neutral_samples(stats, &model);
        let quant = film_core::pipeline::dye_quantiles(stats, &model);
        // 中调落点只由负片本身的近中性像素决定：用**默认**反差/白平衡/CMY 去校准，
        // 才不会被用户的滑杆反向抵消（自动是基线，手动是叠加在上面）。
        let probe = PrintParams {
            d_ref,
            anchor_lo: quant.0,
            anchor_med: neutral,
            anchor_hi: quant.2,
            ..Default::default()
        };
        self.step(84, "校准中调落点…");
        let anchor_t = film_core::pipeline::calibrate_anchor_t(&probe, &refs);
        // 第二自由度：三点锚点让"样本平均"中性，但三层曲线形状不同，
        // 占画面主体的中间调会残留青绿（实拍图"整幅偏绿"）。这里只用中调样本
        // 逐通道调 alpha，端点固定，片基与纸白都不动。
        let mut probe_alpha = probe.clone();
        probe_alpha.anchor_t = anchor_t;
        let alpha = film_core::pipeline::calibrate_alpha(&probe_alpha, &refs);
        // alpha 变了，中调落点再收一次，两个自由度对齐到同一组中性样本
        let mut probe_t = probe.clone();
        probe_t.alpha = alpha;
        let anchor_t = film_core::pipeline::calibrate_anchor_t(&probe_t, &refs);

        // 二级自动色调（Photoshop Auto Tone 口径）：结构校准之后，三层染料串扰
        // 在中密度处还留一点偏色。按显示域的实际分布逐通道重定端点（各自裁 0.5%
        // 后拉满），这一步天然带白平衡：哪层偏多，那层就被压回去。
        // 与 anchor 一样属于"自动基线"，不掺用户滑杆，滑杆只在它上面叠加。
        self.step(88, "自动色调…");
        let lp = film_core::Look::from_name(&self.params.look).params();
        // 风格的中调偏移要一并进采样探针：自动色调的端点按探针的实际亮度定，
        // 探针少了这个偏移就会按"另一个亮度"裁剪，端点跟着错。
        let mut probe_anchor_t = anchor_t;
        if lp.mid_shift.abs() > 1e-4 {
            let k = 2.0f32.powf(lp.mid_shift);
            for t in probe_anchor_t.iter_mut() {
                *t = (*t / k).clamp(0.02, 0.98);
            }
        }
        let pp0 = PrintParams {
            d_ref,
            grade: lp.grade,
            out_gamma: self.params.gamma,
            saturation: lp.saturation,
            look: lp.clone(),
            anchor_lo: quant.0,
            anchor_med: neutral,
            anchor_hi: quant.2,
            alpha,
            anchor_t: probe_anchor_t,
            exposure: 0.0,
            ..Default::default()
        };
        let shown = film_core::pipeline::render_negative_with_frame_k(
            stats,
            &model,
            &pp0,
            frame_base,
            frame_k,
        );
        let samples = display_samples(&shown, 0.03, 0.97);
        let mut auto_tone = film_core::print::auto_tone_params(&samples, 1.0);
        // 单色风格必须用**同一个**端点：逐通道自动色阶会把三层染料各自拉满，
        // 单色图因此重新带上色偏（实测 mono 之下灰是 0.651/0.654/0.621，偏黄）。
        if lp.saturation <= -0.9 {
            let lo = (auto_tone.lo[0] + auto_tone.lo[1] + auto_tone.lo[2]) / 3.0;
            let hi = (auto_tone.hi[0] + auto_tone.hi[1] + auto_tone.hi[2]) / 3.0;
            auto_tone.lo = [lo; 3];
            auto_tone.hi = [hi; 3];
        }
        if std::env::var("FM_DEBUG").is_ok() {
            eprintln!(
                "[auto_tone] look={} sat={} samples={} lo={:?} hi={:?}",
                self.params.look,
                lp.saturation,
                samples.len(),
                auto_tone.lo,
                auto_tone.hi
            );
        }

        let c = Rc::new(Cached {
            key,
            model,
            frame_base,
            alpha,
            base,
            fit,
            d_ref,
            median,
            neutral,
            quant,
            anchor_t,
            auto_tone,
        });
        self.cached = Some(c.clone());
        Ok(c)
    }

    fn print_params(&self, c: &Cached) -> PrintParams {
        // 风格给一组基线，用户的滑杆叠在上面（风格不是"覆盖"，是起点）
        let look = film_core::Look::from_name(&self.params.look);
        let lp = look.params();
        let mut pp = PrintParams {
            d_ref: c.d_ref,
            grade: self.params.grade * lp.grade,
            cmy: self.params.cmy,
            wb: self.params.wb,
            out_gamma: self.params.gamma,
            saturation: (self.params.sat + lp.saturation).clamp(-1.0, 2.0),
            look: lp,
            ..Default::default()
        };
        // 逐层三点锚定（片基→黑、中调→中调灰、最密处→白）：三层量程不同，
        // 各自用自己的中调落位，才不会出现整幅偏青/偏暖。端点固定，片基仍是纯黑。
        //
        // 关键：锚点是"测得的结构"（这张负片的染料分布），不是"自动的结论"。
        // 所以它总是在，自动开关只决定中调落点要不要逐层中性化；
        // 手动的曝光/反差/CMY 一律叠在它上面——自动给起点，手动给偏移，
        // 关掉自动时画面不会跳，滑杆也不会被清零。
        let (lo, _, hi) = c.quant;
        pp.anchor_lo = lo;
        pp.anchor_med = c.neutral;
        pp.anchor_hi = hi;
        pp.alpha = if self.params.auto_exposure {
            c.alpha
        } else {
            [1.0; 3]
        };
        let mut anchor_t = if self.params.auto_exposure {
            // 近中性像素闭环校准：三层各自的曲线形状不同，统一落点仍会偏色
            c.anchor_t
        } else {
            // 不中性化：三层用同一个落点，剩下多少色偏就交给 CMY 滑杆
            [MID_TARGET; 3]
        };
        // 风格只表达"相对自动落点偏几档"：`t` 越小越亮，所以正的 mid_shift
        // 要乘小于 1 的系数。绝对亮度一律由锚点按每张图自己算——
        // 之前把风格曝光写死成显示域绝对值，等于拿一张测试图的 p50 当全局常数。
        let lp = film_core::Look::from_name(&self.params.look).params();
        if lp.mid_shift.abs() > 1e-4 {
            let k = 2.0f32.powf(lp.mid_shift);
            for t in anchor_t.iter_mut() {
                *t = (*t / k).clamp(0.02, 0.98);
            }
        }
        pp.anchor_t = anchor_t;
        pp.exposure = self.params.exposure;
        // 自动色调：用户可关，关掉时是恒等（画面回到纯粹的"负片→相纸"结果）
        pp.auto_tone = if self.params.auto_tone {
            c.auto_tone.clone()
        } else {
            AutoTone::default()
        };
        // 诊断开关：整体曝光偏移，用来分辨"偏青"是亮度带来的还是真有色偏
        if let Ok(v) = std::env::var("FM_EXPO") {
            if let Ok(e) = v.trim().parse::<f32>() {
                pp.exposure += e;
            }
        }
        // 诊断开关：暂时关掉相纸串扰，用来判断"中调偏青"是否来自投影矩阵
        if std::env::var("FM_XTALK").ok().as_deref() == Some("off") {
            pp.paper_a = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        }
        pp
    }

    fn stats_json(&self, c: &Cached, pp: &PrintParams) -> Value {
        json!({
            "base": c.base.base,
            "baseConf": c.base.confidence,
            "baseSource": match c.base.source {
                BaseSource::Rebate => "片边",
                BaseSource::Percentile => "高分位",
                BaseSource::Screen => "画面统计",
            },
            "fitConf": c.fit.confidence,
            "samples": c.fit.samples,
            "gain": c.fit.gain.g,
            "exposure": pp.exposure,
            "dRef": c.d_ref,
            "median": c.median,
            "looksPositive": looks_like_positive(c.base.base),
        })
    }

    fn render_preview(&mut self) -> Result<Value> {
        let t0 = Instant::now();
        let c = self.ensure()?;
        let pp = self.print_params(&c);
        self.step(90, "渲染预览…");
        // 整幅出片：区域内的色调（参数由区域内统计得出）同步到区域外，
        // 白底与齿孔由 pipeline 遮罩成纸白，画面本身不裁切。
        let pv = self.preview.as_ref().ok_or_else(|| anyhow!("未载入图像"))?;
        let out = render_image(pv, &c.model, &pp, c.frame_base, self.params.frame_k);
        self.step(96, "编码 JPEG…");
        let jpeg = to_data_url(&out, PREVIEW_QUALITY);
        let (pw, ph) = (out.width, out.height);
        Ok(json!({
            "type": "render",
            "jpeg": jpeg,
            "stats": self.stats_json(&c, &pp),
            "params": self.params_json(),
            "w": pw,
            "h": ph,
            "fullW": self.src.as_ref().map(|s| s.width).unwrap_or(0),
            "fullH": self.src.as_ref().map(|s| s.height).unwrap_or(0),
            "factor": self.factor,
            "ms": t0.elapsed().as_millis() as u64,
        }))
    }

    // ---------- 导出 ----------

    fn export(&mut self) -> Result<Value> {
        if self.need_crop {
            return Err(anyhow!("请先框定有效图像区域"));
        }
        let t0 = Instant::now();
        let c = self.ensure()?;
        let pp = self.print_params(&c);
        self.step(90, "全幅渲染…");
        // 整幅出片（含区域外），不裁切
        let out = {
            let src = self.src.as_ref().ok_or_else(|| anyhow!("未载入图像"))?;
            render_image(src, &c.model, &pp, c.frame_base, self.params.frame_k)
        };

        std::fs::create_dir_all(&self.out_dir)?;
        let stem = Path::new(&self.name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "filmlab".into());
        let path = self.out_dir.join(format!("{stem}-mask.tif"));
        self.step(95, "写入 16-bit TIFF…");
        out.save_tiff16(&path)?;
        let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        Ok(json!({
            "type": "exported",
            "path": abs.to_string_lossy(),
            "w": out.width,
            "h": out.height,
            "ms": t0.elapsed().as_millis() as u64,
        }))
    }

    /// 全幅渲染成 JPEG data URL。
    ///
    /// 安卓上两条都走不通：相册不认 16-bit TIFF，WebView 也拿不到能自己写的路径。
    /// 所以把"渲染"和"落盘"拆开——这里只管出图和编码，存到相册交给 Java 侧。
    pub fn export_jpeg_url(&mut self) -> Result<Value> {
        if self.need_crop {
            return Err(anyhow!("请先框定有效图像区域"));
        }
        let t0 = Instant::now();
        let c = self.ensure()?;
        let pp = self.print_params(&c);
        self.step(90, "全幅渲染…");
        // 整幅出片（含区域外），不裁切
        let out = {
            let src = self.src.as_ref().ok_or_else(|| anyhow!("未载入图像"))?;
            render_image(src, &c.model, &pp, c.frame_base, self.params.frame_k)
        };
        self.step(95, "编码 JPEG…");
        let data = to_data_url(&out, 92);
        let stem = Path::new(&self.name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "filmlab".into());
        Ok(json!({
            "type": "exported",
            "path": format!("{stem}-mask.jpg"),
            "data": data,
            "w": out.width,
            "h": out.height,
            "ms": t0.elapsed().as_millis() as u64,
        }))
    }
}

// ---------- 工具 ----------

/// 很粗的正片判别：C-41 的片基是橙色，线性域里必然 R > G > B 且 R 明显高于 B。
/// 反过来（B 压过 R，或三通道接近中性）基本可以断定输入已经是冲洗好的正片，
/// 此时再做反相只会得到负像 —— 这种情况直接原样显示更合理。
fn looks_like_positive(base: [f32; 3]) -> bool {
    let orange = base[0] > base[1] && base[1] > base[2];
    let spread = base[0] - base[2];
    !(orange && spread > 0.12)
}

/// 位深决定如何解释数据：16-bit / 32-bit 视作线性扫描，8-bit 视作显示编码。
fn decode_dynamic(img: &image::DynamicImage) -> (FloatImage, &'static str) {
    match img.color() {
        image::ColorType::Rgb16
        | image::ColorType::Rgba16
        | image::ColorType::L16
        | image::ColorType::Rgb32F
        | image::ColorType::Rgba32F => (
            FloatImage::from_dynamic(img, Linearize::None, 2.2),
            "线性 16-bit",
        ),
        _ => (
            FloatImage::from_dynamic(img, Linearize::Srgb, 2.2),
            "sRGB 8-bit",
        ),
    }
}

/// `FloatImage` 是**平面存储**（R 平面、G 平面、B 平面依次排布），
/// 所以逐像素结果必须先并行算好，再按平面写回——不能按交错的 RGB 三元组切块。
fn write_planes(w: usize, h: usize, v: &[[f32; 3]]) -> FloatImage {
    let n = w * h;
    let mut out = FloatImage::new(w, h);
    let (a, rest) = out.data.split_at_mut(n);
    let (b, c) = rest.split_at_mut(n);
    for (i, px) in v.iter().enumerate() {
        a[i] = px[0];
        b[i] = px[1];
        c[i] = px[2];
    }
    out
}

/// 顺时针旋转 90°。平面存储，逐像素搬运（新图宽 = 原高、新高 = 原宽）。
fn rot90_once(src: &FloatImage) -> FloatImage {
    let (w, h) = (src.width, src.height);
    let mut out = FloatImage::new(h, w);
    let nsrc = src.len();
    let ndst = out.len();
    for y in 0..h {
        for x in 0..w {
            // 顺时针映射：(x, y) → (dx, dy) = (h - 1 - y, x)
            let di = x * h + (h - 1 - y);
            let si = y * w + x;
            out.data[di] = src.data[si];
            out.data[ndst + di] = src.data[nsrc + si];
            out.data[2 * ndst + di] = src.data[2 * nsrc + si];
        }
    }
    out
}

/// 取像素矩形子图（x0/y0/cw/ch 已由调用方夹取到边界内）。
fn crop_rect(src: &FloatImage, x0: usize, y0: usize, cw: usize, ch: usize) -> FloatImage {
    let mut out = FloatImage::new(cw, ch);
    let nsrc = src.len();
    let ndst = out.len();
    for y in 0..ch {
        let srow = (y0 + y) * src.width + x0;
        let drow = y * cw;
        for x in 0..cw {
            out.data[drow + x] = src.data[srow + x];
            out.data[ndst + drow + x] = src.data[nsrc + srow + x];
            out.data[2 * ndst + drow + x] = src.data[2 * nsrc + srow + x];
        }
    }
    out
}

/// 归一化矩形跟着 90° 旋转一起转（times 次顺时针）。
fn rotate_rect(mut r: [f32; 4], times: u8) -> [f32; 4] {
    for _ in 0..(times % 4) {
        // 顺时针：(x, y, w, h) → (1-(y+h), x, h, w)
        r = [1.0 - (r[1] + r[3]), r[0], r[3], r[2]];
    }
    r
}

fn render_image(
    src: &FloatImage,
    model: &NegativeModel,
    pp: &PrintParams,
    frame_base: Option<[f32; 3]>,
    frame_k: f32,
) -> FloatImage {
    film_core::pipeline::render_negative_with_frame_k(src, model, pp, frame_base, frame_k)
}

/// 负片原样预览：线性值 → sRGB 显示编码。
fn display_of(img: &FloatImage, from_linear: bool) -> FloatImage {
    let v: Vec<[f32; 3]> = (0..img.len())
        .into_par_iter()
        .map(|i| {
            let p = img.pixel(i);
            if from_linear {
                [
                    clamp01(linear_to_srgb(p[0])),
                    clamp01(linear_to_srgb(p[1])),
                    clamp01(linear_to_srgb(p[2])),
                ]
            } else {
                [clamp01(p[0]), clamp01(p[1]), clamp01(p[2])]
            }
        })
        .collect();
    write_planes(img.width, img.height, &v)
}

/// 从渲染结果的**显示域**取样，供自动颜色统计。
///
/// 只留中间亮度：纸白（1.0）和片基（0.0）是端点固定的，把它们算进分位
/// 会让黑色/白色被拉回中间，中调中性化就白做了。
fn display_samples(img: &FloatImage, lo: f32, hi: f32) -> Vec<[f32; 3]> {
    let stride = (img.len() / 120_000).max(1);
    (0..img.len())
        .into_par_iter()
        .step_by(stride)
        .map(|i| img.pixel(i))
        .filter(|p| {
            let lum = (p[0] + p[1] + p[2]) / 3.0;
            lum > lo && lum < hi
        })
        .collect()
}

fn median_dye(img: &FloatImage, model: &NegativeModel) -> [f32; 3] {
    let stride = (img.len() / 80_000).max(1);
    let samples: Vec<[f32; 3]> = (0..img.len())
        .into_par_iter()
        .step_by(stride)
        .filter_map(|i| {
            let p = img.pixel(i);
            // 片框外的白纸没有染料信息，混入中位数会让自动曝光整体偏亮
            if film_core::calib::is_frame_outside(p, model.base) {
                return None;
            }
            Some(model.dye_at(p))
        })
        .collect();
    let n = samples.len().max(1);
    let mut out = [0.0f32; 3];
    for j in 0..3 {
        let mut v: Vec<f32> = samples.iter().map(|s| s[j]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        out[j] = v[n / 2];
    }
    out
}

fn to_data_url(img: &FloatImage, quality: u8) -> String {
    let mut raw = Vec::with_capacity(img.len() * 3);
    for i in 0..img.len() {
        let p = img.pixel(i);
        raw.push((clamp01(p[0]) * 255.0).round() as u8);
        raw.push((clamp01(p[1]) * 255.0).round() as u8);
        raw.push((clamp01(p[2]) * 255.0).round() as u8);
    }
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    if enc
        .encode(&raw, img.width as u32, img.height as u32, image::ExtendedColorType::Rgb8)
        .is_err()
    {
        return String::new();
    }
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(buf)
    )
}

fn decode_data_url(url: &str) -> Result<Vec<u8>> {
    let (meta, payload) = url
        .split_once(',')
        .ok_or_else(|| anyhow!("非法的 data URL"))?;
    if !meta.contains(";base64") {
        return Err(anyhow!("仅支持 base64 data URL"));
    }
    Ok(base64::engine::general_purpose::STANDARD.decode(payload)?)
}
