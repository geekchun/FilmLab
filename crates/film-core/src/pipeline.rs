//! 端到端管线：片基 → 密度 → 解耦 → 虚拟印相 → 显示编码。

use rayon::prelude::*;

use crate::calib::{
    estimate_d_ref, estimate_reference, fit_gains, is_frame_outside, is_frame_outside_with,
    BaseEstimate, BaseSource, GainFit, FRAME_K,
};
use crate::image::FloatImage;
use crate::model::{GainCurve, NegativeModel, StockProfile};
use crate::print::{PrintParams, MID_TARGET};

/// 新内核（[`crate::neg`]）的端到端结果。
#[derive(Clone, Debug)]
pub struct NegOutcome {
    pub image: FloatImage,
    pub neg: crate::neg::Negative,
    pub samples: crate::neg::Samples,
    /// 片框外权重（255 = 无胶片区，已给纸白）。
    pub alpha: Vec<u8>,
}

/// 新内核的端到端：测片基 → 解一次解析式 → 印相 → 显示编码。
///
/// 只有 `base` 是从图上测的（负片上唯一的物理参考），其余全部是胶片属性或
/// 用户给定；没有任何"看完渲染结果再改参数"的闭环。
///
/// `frame_k` 是"多透算无胶片"的门槛倍率，只作用于片框遮罩（见
/// [`crate::calib::is_frame_outside_with`]），默认用 [`crate::calib::FRAME_K`]。
pub fn process_neg(
    img: &FloatImage,
    tuning: crate::neg::Tuning,
    base_override: Option<[f32; 3]>,
    frame_k: f32,
) -> NegOutcome {
    let mut samples = crate::neg::Samples::analyze(img);
    if let Some(b) = base_override {
        samples.base = b;
    }
    let neg = crate::neg::Negative::solve(&samples, tuning);
    // 片框外的灯箱透光比片基还透，照常印相会整块吞黑；这些像素没有画面信息，
    // 直接给纸白。判据是几何/照明的物理事实，不是给颜色打补丁。
    let mask: Vec<u8> = (0..img.len())
        .into_par_iter()
        .map(|i| {
            u8::from(is_frame_outside_with(
                img.pixel(i),
                samples.base,
                frame_k,
            ))
        })
        .collect();
    let alpha = frame_alpha(&mask, img.width, img.height);
    let image = neg.render_image(img, Some(&alpha));
    NegOutcome {
        image,
        neg,
        samples,
        alpha,
    }
}

#[derive(Clone, Debug)]
pub struct ProcessParams {
    pub stock: String,
    /// 手动指定片基（None = 自动检测）
    pub base: Option<[f32; 3]>,
    /// 是否做卷级解耦矩阵估计
    pub fit_unmix: bool,
    /// 先验强度（越大越贴近胶卷档案）
    pub lambda_prior: f32,
    pub print: PrintParams,
    /// 自动曝光锚定中调
    pub auto_exposure: bool,
    /// 中调目标（显示参考，0..1）
    pub exposure_target: f32,
}

impl Default for ProcessParams {
    fn default() -> Self {
        Self {
            stock: "C-41 generic".into(),
            base: None,
            fit_unmix: true,
            lambda_prior: 0.02,
            print: PrintParams::default(),
            auto_exposure: true,
            exposure_target: 0.45,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessOutcome {
    pub image: FloatImage,
    pub base: BaseEstimate,
    pub model: NegativeModel,
    pub fit: GainFit,
    pub exposure: f32,
    pub median_dye: [f32; 3],
    /// 实际生效的印相参数（含自动锚点），便于诊断"到底把哪一层钉到了哪个 t"
    pub print: PrintParams,
}

pub fn process(img: &FloatImage, params: &ProcessParams) -> ProcessOutcome {
    let stock = StockProfile::from_name(&params.stock);

    // 1) 归一化参考：画面自身的高分位，不再去找橙色片基——
    //    裁剪只保留画面区域时片基已经在框外，找片基反而会把整幅基准抬到白纸。
    let base = match params.base {
        Some(b) => BaseEstimate {
            base: b,
            confidence: 1.0,
            source: BaseSource::Screen,
        },
        None => estimate_reference(img),
    };

    // 2) 先验模型（串扰矩阵来自胶卷档案）
    let model0 = NegativeModel::from_matrix(base.base, &stock.a_prior, stock.d_ref)
        .expect("prior matrix must be invertible");

    // 3) 卷级中性增益估计（色偏主因；串扰保持档案先验）
    let fit = if params.fit_unmix {
        fit_gains(img, &model0, params.lambda_prior)
    } else {
        GainFit {
            gain: GainCurve::default(),
            confidence: 1.0,
            samples: 0,
            residual: 0.0,
        }
    };

    let model = NegativeModel {
        base: base.base,
        unmix: model0.unmix,
        d_ref: stock.d_ref,
        gain: fit.gain.clone(),
    };

    // 4) 相纸锚点：以这张负片的实际最大密度为准（薄负片用档案值会导致反差不足→发灰）
    let d_ref_est = estimate_d_ref(img, &model);
    let mut print = params.print.clone();
    // 三通道用同一个量程：逐通道量程归一等于给每层不同的斜率，饱和色场景
    // 会被推成明显偏色。层间灵敏度差交给 C/M/Y（printer lights）补，而不是量程。
    let d_all = ((d_ref_est[0] + d_ref_est[1] + d_ref_est[2]) / 3.0).clamp(0.55, 3.60);
    print.d_ref = [d_all; 3];

    // 5) 自动印相：只做"端点固定的中调对齐"（片基与最密处的映射不受影响）
    let median_dye = median_dye(img, &model);
    if params.auto_exposure {
        // 逐层三点锚定：片基→黑、中调→MID_TARGET、最密处→白。
        // 锚点取自"画面里彩度最低的那批像素"（路面/白墙/灰砖）：让它们三通道
        // 落到同一个 t 就自然中性，蓝天草地彩度高、不会被当成偏色反推。
        // 落点固定在中调灰：翻拍曝光偏亮的负片整体密度被压缩，只有把参考像素
        // 钉在中调，画面才会回到正常亮度。
        // 形状用全幅分位（逐层量程差、分布偏斜都被吸收），
        // 中调落点用"近中性像素"（低彩度那批）——这样白墙/灰路面才真的是灰。
        let (lo, _, hi) = dye_quantiles(img, &model);
        let refs = neutral_samples(img, &model);
        let ref_med = if refs.is_empty() {
            crate::pipeline::median_dye(img, &model)
        } else {
            let n = refs.len() as f32;
            let mut acc = [0.0f32; 3];
            for d in &refs {
                for j in 0..3 {
                    acc[j] += d[j];
                }
            }
            [acc[0] / n, acc[1] / n, acc[2] / n]
        };
        print.anchor_lo = lo;
        print.anchor_med = ref_med;
        print.anchor_hi = hi;
        print.anchor_t = calibrate_anchor_t(&print, &refs);
        print.alpha = [1.0; 3];
        print.exposure = params.print.exposure;
    }

    // 6) 逐像素渲染
    let image = render_negative(img, &model, &print);
    let exposure = print.exposure;

    ProcessOutcome {
        image,
        base,
        model,
        fit,
        exposure,
        median_dye,
        print,
    }
}

/// 逐像素虚拟印相。
///
/// 片框之外（片夹白纸 / 灯箱背光）的像素比片基还透，相对密度是负的，
/// 照常印相只会整块吞黑；这里直接给纸白，让翻拍/扫描的白边保持干净。
/// 3×3 邻域计数 → 0..255 的软权重（0 = 画面，255 = 无胶片区）。
///
/// 先水平、再垂直做两遍矩形和，统计窗口内 9 个像素有多少被判成"片框外"。
/// 过半数算孔洞（255），否则算画面（0）——**只表决，不插值**。
///
/// 齿孔是硬边物体：任何 0~255 的中间值都会在孔口铺出两三像素的灰白渐变，
/// 放大就成了"糊边"。窗口也不能再放大（5×5、9×9 会把小孔的圆角啃圆、
/// 让形状整体外扩），3×3 刚好压住逐像素判据噪声而不改变孔的形状。
fn frame_alpha(mask: &[u8], w: usize, h: usize) -> Vec<u8> {
    const R: usize = 1; // 3×3 中值表决
    const SPAN: usize = 2 * R + 1;

    let mut hs = vec![0u8; mask.len()];
    hs.par_chunks_mut(w)
        .zip(mask.par_chunks(w))
        .for_each(|(out, row)| {
            for (x, o) in out.iter_mut().enumerate() {
                let a = x.saturating_sub(R);
                let b = (x + R + 1).min(w);
                *o = row[a..b].iter().sum();
            }
        });

    let mut alpha = vec![0u8; mask.len()];
    alpha.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
        let y0 = y.saturating_sub(R);
        let y1 = (y + R + 1).min(h);
        for (x, o) in out.iter_mut().enumerate() {
            let mut s = 0u16;
            for yy in y0..y1 {
                s += hs[yy * w + x] as u16;
            }
            // 3×3 里过半数（≥5/9）判成无胶片区 → 255，否则 0。
            // 只做中值表决、不做插值：齿孔是硬边物体，任何 0~255 之间的
            // 中间值都会在孔口铺出一条灰白渐变，放大看就是"糊"。
            *o = if s * 2 >= SPAN as u16 * SPAN as u16 { 255 } else { 0 };
        }
    });
    alpha
}

pub fn render_negative(img: &FloatImage, model: &NegativeModel, pp: &PrintParams) -> FloatImage {
    render_negative_with_frame(img, model, pp, None)
}

/// 与 [`render_negative`] 相同，但可以单独指定"无胶片区"的判决基准。
///
/// 判据里那个"比它更透就是没有胶片"的参照，物理上只能是**未曝光片基**。
/// `model.base` 存的却是画面自身的高分位（有画面时更稳、不会被片框带偏），
/// 而灯箱/白纸常把画面高光推到比片基还亮——拿它当门槛，齿孔就会漏判，
/// 整条孔带被当成画面反转成黑块。所以遮罩用片基，密度归一化仍用 `model.base`。
pub fn render_negative_with_frame(
    img: &FloatImage,
    model: &NegativeModel,
    pp: &PrintParams,
    frame_base: Option<[f32; 3]>,
) -> FloatImage {
    render_negative_with_frame_k(img, model, pp, frame_base, FRAME_K)
}

/// 同 [`render_negative_with_frame`]，但可调"多透算无胶片"的门槛倍率。
pub fn render_negative_with_frame_k(
    img: &FloatImage,
    model: &NegativeModel,
    pp: &PrintParams,
    frame_base: Option<[f32; 3]>,
    frame_k: f32,
) -> FloatImage {
    let base = frame_base.unwrap_or(model.base);
    let n0 = img.len();

    // 1) 判"无胶片区"（片框外 / 齿孔 / 灯箱直射）。
    //    判决前先看 3×3 均值而不是单像素：孔外那圈"青蓝漏光带"的 R 只比片基高
    //    6.9%，离 10% 门槛太近，JPEG 噪声足以把零星像素推过门槛；硬二值之后
    //    就成了齿孔下沿一排 1~8px 的断续白点，看着像片基上的颗粒。先平均再判，
    //    单像素噪声被压掉、判决场稳定；平均不移动边界（孔口两侧过渡对称），
    //    齿孔的形状与位置都不变。
    let mut mask = vec![0u8; n0];
    mask.par_chunks_mut(img.width)
        .enumerate()
        .for_each(|(y, row)| {
            let y0 = y.saturating_sub(1);
            let y2 = (y + 1).min(img.height - 1);
            for (x, m) in row.iter_mut().enumerate() {
                let x0 = x.saturating_sub(1);
                let x2 = (x + 1).min(img.width - 1);
                let mut acc = [0f32; 3];
                for ny in y0..=y2 {
                    for nx in x0..=x2 {
                        let p = img.pixel(ny * img.width + nx);
                        acc[0] += p[0];
                        acc[1] += p[1];
                        acc[2] += p[2];
                    }
                }
                let cnt = ((y2 - y0 + 1) * (x2 - x0 + 1)) as f32;
                *m = u8::from(is_frame_outside_with(
                    [acc[0] / cnt, acc[1] / cnt, acc[2] / cnt],
                    base,
                    frame_k,
                ));
            }
        });
    // 2) 3×3 邻域计数化成软权重：判据在阈值附近难免抖一两个像素，硬二值会把
    //    轮廓打成锯齿；1 像素的过渡带足以让形状连续，又不会把齿孔边缘糊开。
    let alpha = frame_alpha(&mask, img.width, img.height);

    let v: Vec<[f32; 3]> = (0..n0)
        .into_par_iter()
        .map(|i| {
            let a = alpha[i];
            // 片框外没有画面信息，只能给纸白，而且必须是**显示域**的 1.0：
            // 1.0 才等于 8bit 255 / 16bit 65535。之前给 [0.985, 0.975, 0.955]
            // （想模拟"纸白微暖"），量化后是 251/249/244，白边一圈米灰。
            if a == 255 {
                return [1.0; 3];
            }
            let c = pp.apply(model.dye_at(img.pixel(i)));
            if a == 0 {
                return c;
            }
            let k = a as f32 / 255.0;
            [
                c[0] * (1.0 - k) + k,
                c[1] * (1.0 - k) + k,
                c[2] * (1.0 - k) + k,
            ]
        })
        .collect();
    // 平面存储：按平面写回，不能按交错 RGB 三元组切块
    let n = img.len();
    let mut out = FloatImage::new(img.width, img.height);
    let (a, rest) = out.data.split_at_mut(n);
    let (b, c) = rest.split_at_mut(n);
    for (i, px) in v.iter().enumerate() {
        a[i] = px[0];
        b[i] = px[1];
        c[i] = px[2];
    }
    out
}

/// 逐层中调落点：从该层原始线性 `t` 向 `MID_TARGET` 靠 `w` 比例。
///
/// `w = 1` 是纯灰世界（三层中调压到同一点，天空多的片子会反偏品红），
/// `w = 0` 完全不管（薄层 R 的中调偏暗，整幅偏青）。取 0.4~0.5 之间即可：
/// 通道间的量程差被吸收大半，场景自身的偏色仍然保留。
pub fn partial_neutral(median_dye: [f32; 3], d_ref: [f32; 3], w: f32) -> [f32; 3] {
    let w = w.clamp(0.0, 1.0);
    let mut at = [0.0f32; 3];
    for j in 0..3 {
        let dref = d_ref[j].max(1e-3);
        let t0 = ((dref - median_dye[j]) / dref).clamp(0.02, 0.98);
        at[j] = (t0 * (1.0 - w) + MID_TARGET * w).clamp(0.05, 0.95);
    }
    at
}

/// 用"近中性像素"的染料闭环校准中调落点。
///
/// 三点锚定只保证"该层中位落到 mt"，但三层曲线形状不同、`paper_a` 串扰也不对称，
/// 同一批中性物渲染出来仍会偏青或偏黄。这里把参考染料的平均输出直接算一遍，
/// 按三通道偏离灰的程度微调各层的 mt，迭代几次就收敛到"平均中性"。
///
pub fn calibrate_anchor_t(pp: &PrintParams, refs: &[[f32; 3]]) -> [f32; 3] {
    if refs.is_empty() {
        return [MID_TARGET; 3];
    }
    let n = refs.len().min(256);
    let mut at = [MID_TARGET; 3];
    let mut probe = pp.clone();
    for _ in 0..5 {
        probe.anchor_t = at;
        let mut sum = [0.0f32; 3];
        for s in refs.iter().take(n) {
            let c = probe.apply(*s);
            for j in 0..3 {
                sum[j] += c[j];
            }
        }
        let m = [sum[0] / n as f32, sum[1] / n as f32, sum[2] / n as f32];
        let avg = (m[0] + m[1] + m[2]) / 3.0;
        if avg <= 1e-4 {
            break;
        }
        let mut moved = false;
        for j in 0..3 {
            let r = m[j] / avg;
            if (r - 1.0).abs() > 0.003 {
                moved = true;
            }
            at[j] = (at[j] * r.powf(0.7)).clamp(0.08, 0.90);
        }
        if !moved {
            break;
        }
    }
    at
}

/// 中性样本在**中间调**的残余偏色 → 逐通道幂次补偿（自动灰平衡的第二自由度）。
///
/// `calibrate_anchor_t` 只把"样本平均"拉成中性。三层染料的曲线形状不同，
/// 平均中性 ≠ 中间调中性：实拍图里墙面、屋顶这些占画面主体的中间调会系统性
/// 偏青绿，而白墙（高光）与阴影把它拉回平均值——"整幅有点偏绿"就是这么来的。
///
/// 这里只取渲染后落在中间调的样本，逐通道微调 `alpha`。`alpha` 是端点固定的
/// 幂次（`t^(alpha*wb+cmy)`），所以片基仍是纯黑、纸白仍是纯白，改动只发生在
/// 中间调——正是需要修的那一段。
pub fn calibrate_alpha(pp: &PrintParams, refs: &[[f32; 3]]) -> [f32; 3] {
    if refs.is_empty() {
        return [1.0; 3];
    }
    let n = refs.len().min(256);
    let mut a = [1.0f32; 3];
    let mut probe = pp.clone();
    for _ in 0..8 {
        probe.alpha = a;
        let mut sum = [0.0f32; 3];
        let mut cnt = 0usize;
        for s in refs.iter().take(n) {
            let c = probe.apply(*s);
            let lum = (c[0] + c[1] + c[2]) / 3.0;
            if (0.30..0.72).contains(&lum) {
                for j in 0..3 {
                    sum[j] += c[j];
                }
                cnt += 1;
            }
        }
        if cnt < 16 {
            break;
        }
        let m = [
            sum[0] / cnt as f32,
            sum[1] / cnt as f32,
            sum[2] / cnt as f32,
        ];
        let avg = (m[0] + m[1] + m[2]) / 3.0;
        if avg <= 1e-4 {
            break;
        }
        let mut moved = false;
        for j in 0..3 {
            // alpha 越大越亮（t 越小越亮），所以偏暗的通道要**增大** alpha，
            // 与 anchor_t 的方向相反（那里 t 越大越暗）。
            let r = (m[j] / avg).clamp(0.85, 1.18);
            if (r - 1.0).abs() > 0.006 {
                moved = true;
            }
            a[j] = (a[j] * r.powf(-0.5)).clamp(0.55, 2.20);
        }
        if !moved {
            break;
        }
    }
    if std::env::var("FM_DEBUG").is_ok() {
        eprintln!("[calibrate_alpha] refs={} -> {:?}", refs.len(), a);
    }
    a
}

/// 无光区判定（灯箱照亮范围之外 / 片夹遮挡 / 胶片外沿的暗底）。
///
/// 翻拍负片时，画面之外还有两种像素：灯箱透光（比片基还亮，见 `is_frame_outside`）
/// 和灯箱以外的暗底。后者三层都压在扫描黑电平上，相对密度远超画面最密处，
/// 但它们不含任何画面信息。实测 `_DSC1566film.JPG`：
///   画面最密处（最亮天体）R/G/B ≈ 0.43 / 0.91 / 0.80，亮度为片基的 16% 以上；
///   暗底 R/G/B ≈ 2.06 / 1.92 / 1.29，亮度只有片基的 0.3% ~ 6%，却占全图 9%。
/// 一旦混进分位统计，白点被顶到 2.13，整幅画面只能占据 t 的下三分之一——
/// 观感就是欠曝一档多、饱和色被压平（对比参考正片逐点差 -50 ~ -70/255）。
/// 判据取"亮度低于片基 10%"：暗底 ≤6%，画面最密处 ≥16%，中间留了足够空隙。
#[inline]
fn is_no_light(p: [f32; 3], base: [f32; 3]) -> bool {
    let lum = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let base_lum = (0.2126 * base[0] + 0.7152 * base[1] + 0.0722 * base[2]).max(1e-6);
    lum < 0.10 * base_lum
}

/// 画面内每层染料的低 / 中 / 高分位。
///
/// 这是"去色罩"里真正起作用的逐层归一：负片三层的染料分布中心与宽度都不同
/// （实例 R 层薄得多），只对齐中位会在明暗两端残留偏色；把三层的低分位钉到
/// 纸黑侧、高分位钉到纸白侧、中位钉到中调，形状差才被吸收干净。
pub fn dye_quantiles(img: &FloatImage, model: &NegativeModel) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let stride = (img.len() / 80_000).max(1);
    // (染料, 是否在光照范围内)：先按"有光"筛，样本不够再退回全部，
    // 免得整幅都很薄的特殊负片把统计退化成空集。
    let mut samples: Vec<([f32; 3], bool)> = Vec::new();
    for i in (0..img.len()).step_by(stride) {
        let p = img.pixel(i);
        if is_frame_outside(p, model.base) {
            continue;
        }
        let d = model.dye_at(p);
        let mean = (d[0] + d[1] + d[2]) / 3.0;
        // 只做安全上限：暗端不能砍。实测画面有 26% 像素的中位密度 < 0.07，
        // 一旦把下限抬到 0.12，低分位就变成"剩下那些较亮像素"的低分位，
        // 黑点被抬高一档，暗部整片压死（参考正片同处是 50 左右，我们只有 9）。
        if !(0.01..=1.60).contains(&mean) {
            continue;
        }
        samples.push((d, !is_no_light(p, model.base)));
    }
    let lit = samples.iter().filter(|s| s.1).count();
    let use_all = lit < 500 || lit * 4 < samples.len();
    let mut v: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (d, is_lit) in &samples {
        if !use_all && !is_lit {
            continue;
        }
        for j in 0..3 {
            v[j].push(d[j]);
        }
    }
    let mut lo = [0.0f32; 3];
    let mut med = [0.0f32; 3];
    let mut hi = [0.0f32; 3];
    for j in 0..3 {
        if v[j].len() < 200 {
            return ([0.0; 3], [0.0; 3], [0.0; 3]);
        }
        v[j].sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = v[j].len();
        lo[j] = v[j][(n as f32 * 0.01) as usize];
        med[j] = v[j][n / 2];
        hi[j] = v[j][((n as f32 * 0.99) as usize).min(n - 1)];
    }
    (lo, med, hi)
}

/// 近中性参考：画面里彩度最低的一批像素（路面、白墙、灰砖）的逐通道中位染料。
///
/// 整幅逐通道中位在"天空/草地占大头"的片子上不成立（那会把蓝天当偏色，
/// 反推成整体偏品红）；彩度最低的子集才更像真正的中性体。
pub fn neutral_samples(img: &FloatImage, model: &NegativeModel) -> Vec<[f32; 3]> {
    let stride = (img.len() / 60_000).max(1);
    let mut samples: Vec<(f32, [f32; 3])> = (0..img.len())
        .step_by(stride)
        .filter_map(|i| {
            let p = img.pixel(i);
            if is_frame_outside(p, model.base) || is_no_light(p, model.base) {
                return None;
            }
            let d = model.dye_at(p);
            let mean = (d[0] + d[1] + d[2]) / 3.0;
            if !(0.25..=2.20).contains(&mean) {
                return None;
            }
            let chroma = d[0].max(d[1]).max(d[2]) - d[0].min(d[1]).min(d[2]);
            Some((chroma, d))
        })
        .collect();
    if samples.len() < 200 {
        return Vec::new();
    }
    samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    // 最低彩度 5%：太少容易被噪点带偏，太多会把天空也算进来
    let take = (samples.len() / 20).max(64).min(samples.len());
    let mut out: Vec<[f32; 3]> = samples[..take].iter().map(|(_, d)| *d).collect();
    // 均匀抽稀到 ~200 个，闭环校准只需求平均，不需要全部
    if out.len() > 200 {
        let step = out.len() / 200;
        out = out.into_iter().step_by(step.max(1)).collect();
    }
    out
}

pub fn neutral_median_dye(img: &FloatImage, model: &NegativeModel) -> [f32; 3] {
    let s = neutral_samples(img, model);
    if s.is_empty() {
        return median_dye(img, model);
    }
    let mut out = [0.0f32; 3];
    let n = s.len() as f32;
    for d in &s {
        for j in 0..3 {
            out[j] += d[j];
        }
    }
    [out[0] / n, out[1] / n, out[2] / n]
}

pub fn median_dye(img: &FloatImage, model: &NegativeModel) -> [f32; 3] {
    let stride = (img.len() / 100_000).max(1);
    let mut samples: Vec<[f32; 3]> = (0..img.len())
        .step_by(stride)
        .filter_map(|i| {
            let p = img.pixel(i);
            // 背景（片框外/无光区）没有染料信息，混进中位数会把自动曝光整体推偏
            if is_frame_outside(p, model.base) || is_no_light(p, model.base) {
                return None;
            }
            Some(model.dye_at(p))
        })
        .collect();
    if samples.is_empty() {
        samples.push([1.0, 1.0, 1.0]);
    }
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        let mut v: Vec<f32> = samples.iter().map(|d| d[c]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        out[c] = v[v.len() / 2];
    }
    out
}

/// 只输出相对密度图（诊断用）。
pub fn density_map(img: &FloatImage, model: &NegativeModel) -> FloatImage {
    let mut out = FloatImage::new(img.width, img.height);
    for i in 0..img.len() {
        let d = model.rel_densities(img.pixel(i));
        out.set_pixel(i, [d[0] / 3.0, d[1] / 3.0, d[2] / 3.0]);
    }
    out
}
