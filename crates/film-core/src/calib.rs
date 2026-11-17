//! 标定：片基估计、相纸锚点、**逐段中性增益的卷级估计**。
//!
//! 为什么只估增益、不估整套矩阵：片基归一后 `D = A·d`，中性色在测量密度空间
//! 是一条过原点的直线（方向 = 三通道相对增益）。故中性样本只能定出 2 个相对增益，
//! 串扰非对角项必须由档案先验或色卡给出——否则最小二乘会收敛到"三行压成同一行"
//! 的退化解（整幅画变灰）。数据负责修正增益（可见色偏的主因），先验负责串扰。

use rayon::prelude::*;

use crate::image::FloatImage;
use crate::model::{GainCurve, NegativeModel};
use crate::numeric::{solve_linear, Mat3, EPS};

/// 片基估计结果。
#[derive(Clone, Debug)]
pub struct BaseEstimate {
    pub base: [f32; 3],
    /// 0..1：平坦块数量与聚集度决定的置信度
    pub confidence: f32,
    pub source: BaseSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseSource {
    /// 片框外侧平坦块聚类（旧路径，仅手动/兼容用）
    Rebate,
    /// 逐层高分位兜底
    Percentile,
    /// 画面内容统计（裁剪后不依赖片基与片框）
    Screen,
}

/// C-41 色罩的典型通道比例（红层最透、蓝层最暗）。
///
/// 去色罩只要"比例"：`D = log10(base/t)` 里 base 的三通道比例决定了色罩矩阵的
/// 基准方向，整体尺度只是密度零点，后面会被 `d_ref` 与曝光吸收。
const C41_BASE_RATIO: [f32; 3] = [0.30, 0.18, 0.09];

/// 画面参考：**只测电平，不测颜色**，从画面自身统计，不再去画面外找橙色片基。
///
/// 为什么不能直接拿画面最亮点当三通道基准：画面里"最亮"的那撮像素往往带场景
/// 自身的颜色（天空偏蓝、黄布偏黄），三通道均值会把场景色当成色罩比例。
/// 实测合成负片（裁掉片基后）直接取最亮均值得到 [0.164,0.091,0.023]，
/// G/B 相对真值 [0.30,0.18,0.09] 塌了 4 倍，整幅偏色。
///
/// 所以：电平（红层有多透）从画面测，通道比例用 C-41 档案值。
/// 场景色偏交给中性增益与中调闭环，那两步只吃统计、不吃基准。
pub fn estimate_reference(img: &FloatImage) -> BaseEstimate {
    BaseEstimate {
        base: screen_reference(img),
        confidence: 0.8,
        source: BaseSource::Screen,
    }
}

/// 画面最透那一撮的**红层电平**，再按 C-41 比例推出 G/B。
fn screen_reference(img: &FloatImage) -> [f32; 3] {
    let n = img.len();
    if n == 0 {
        return C41_BASE_RATIO;
    }
    let stride = (n / 400_000).max(1);
    let mut samples: Vec<(f32, [f32; 3])> = (0..n)
        .step_by(stride)
        .map(|i| {
            let p = img.pixel(i);
            (p[0].max(p[1]).max(p[2]), p)
        })
        .collect();
    // 白纸 / 灯箱直射：没有胶片遮挡，三通道都贴近满量程，整像素剔除。
    samples.retain(|(_, p)| !(p[0] >= 0.90 && p[1] >= 0.90 && p[2] >= 0.90));
    if samples.is_empty() {
        return C41_BASE_RATIO;
    }
    let mut lums: Vec<f32> = samples.iter().map(|(l, _)| *l).collect();
    lums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = lums[lums.len() / 2];
    // 8 倍上限：仅防"白纸未到 0.9 但比画面亮一截"的极端情形，
    // 4 倍会连片基一起剔掉（片基通常比中间调亮 3~6 倍）。
    let cap = (med * 8.0).max(med + 0.08);
    samples.retain(|(l, _)| *l <= cap);
    if samples.is_empty() {
        return C41_BASE_RATIO;
    }
    samples.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top = (samples.len() / 200).max(1);
    let mut r = 0.0f32;
    for (_, p) in samples.iter().take(top) {
        r += p[0];
    }
    let level = (r / top as f32).max(EPS);
    [
        level,
        (level * C41_BASE_RATIO[1] / C41_BASE_RATIO[0]).max(EPS),
        (level * C41_BASE_RATIO[2] / C41_BASE_RATIO[0]).max(EPS),
    ]
}

/// 自动估计片基：片框外侧 15% 区域找平坦块，按 log 亮度聚类取最大簇。
///
/// 这是**会话/交互路径**用的版本：结果带 `confidence`，供上层决定要不要遮罩、
/// 要不要退回其它来源。内核直出（`neg` 管线）另有一套更简单的
/// `neg::detect_base`（16px 块、橙色区间 + 中位段），两者判据相近但不共享实现，
/// 改橙色色域时两边都要看。
pub fn estimate_base(img: &FloatImage) -> BaseEstimate {
    let w = img.width;
    let h = img.height;
    let block = 8usize;
    let mx = (w as f32 * 0.15) as usize;
    let my = (h as f32 * 0.15) as usize;

    let mut blocks: Vec<[f32; 3]> = Vec::new();
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
            if flat {
                blocks.push(mean);
            }
        }
    }

    // 片基必然带橙罩：C-41 未曝光区线性值满足 R > G > B（R-B 通常 > 0.08）。
    // 翻拍/扫描时四周的白纸背板近中性、还比片基更亮——按"最大簇"聚类会直接选中白纸，
    // 片基因此被抬到近白，整幅画再也压不黑，还会连带偏色。
    // 所以只要存在足够多的橙色平坦块，就只在它们里面找：片基 = 胶片上最透（最亮）的橙色区。
    // 判据用通道**比值**而不是差值：白纸的暖调差值也有 0.05~0.10，
    // 但 R/G、G/B 都只有 1.0~1.1；真正的橙罩片基 R/G ≥ 1.3、G/B ≥ 1.25。
    let orange: Vec<[f32; 3]> = blocks
        .iter()
        .copied()
        .filter(|b| b[0] > 0.02 && b[0] > b[1] * 1.30 && b[1] > b[2] * 1.25)
        .collect();
    if orange.len() >= 6 {
        let mut keyed: Vec<(i32, [f32; 3])> = orange
            .iter()
            .map(|b| ((b[0].max(EPS).log10() * 40.0).round() as i32, *b))
            .collect();
        keyed.sort_by_key(|k| k.0);
        // 从最亮端往下扫，取第一个够大的簇（避开单片高光，又不至于落到画面暗部）
        let mut chosen: Option<(usize, i32)> = None;
        let mut i = 0;
        while i < keyed.len() {
            let mut j = i;
            while j < keyed.len() && keyed[j].0 - keyed[i].0 <= 1 {
                j += 1;
            }
            if j - i >= 3 {
                chosen = Some((j - i, keyed[i].0));
            }
            i = j;
        }
        if let Some((cnt, key)) = chosen {
            let mut members: Vec<[f32; 3]> = keyed
                .iter()
                .filter(|(k, _)| (k - key).abs() <= 1)
                .map(|(_, b)| *b)
                .collect();
            members.sort_by(|a, b| {
                a[0].partial_cmp(&b[0])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let mid = members[members.len() / 2];
            return BaseEstimate {
                base: [mid[0].max(EPS), mid[1].max(EPS), mid[2].max(EPS)],
                confidence: ((cnt as f32) / 12.0).min(1.0).max(0.05),
                source: BaseSource::Rebate,
            };
        }
    }

    if blocks.len() >= 6 {
        let mut keyed: Vec<(i32, [f32; 3])> = blocks
            .iter()
            .map(|b| ((b[0].max(EPS).log10() * 25.0).round() as i32, *b))
            .collect();
        keyed.sort_by_key(|k| k.0);
        let mut best = (0usize, 0i32);
        let mut i = 0;
        while i < keyed.len() {
            let mut j = i;
            while j < keyed.len() && keyed[j].0 - keyed[i].0 <= 1 {
                j += 1;
            }
            if j - i > best.0 {
                best = (j - i, keyed[i].0);
            }
            i = j;
        }
        if best.0 >= 5 {
            let mut members: Vec<[f32; 3]> = keyed
                .iter()
                .filter(|(k, _)| (k - best.1).abs() <= 1)
                .map(|(_, b)| *b)
                .collect();
            members.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
            let mid = members[members.len() / 2];
            return BaseEstimate {
                base: [mid[0].max(EPS), mid[1].max(EPS), mid[2].max(EPS)],
                confidence: ((best.0 as f32) / 12.0).min(1.0).max(0.05),
                source: BaseSource::Rebate,
            };
        }
    }

    BaseEstimate {
        base: percentile_base(img, 0.995),
        confidence: 0.15,
        source: BaseSource::Percentile,
    }
}

/// 默认门槛倍率：比片基透 10% 以上算无胶片。
pub const FRAME_K: f32 = 1.10;

/// 片框之外（片夹白纸 / 灯箱背光 / 齿孔）的像素：三层的透过率都高于未曝光片基。
///
/// 这类像素的相对密度是负的（物理上不存在"比未曝光片基更透"的胶片），
/// 若照常走密度→印相，会被钳成片基色整块吞黑。渲染时应当直接按纸白处理。
/// 门槛倍率用 [`FRAME_K`]；需要用户手调时用 [`is_frame_outside_with`]。
pub fn is_frame_outside(p: [f32; 3], base: [f32; 3]) -> bool {
    is_frame_outside_with(p, base, FRAME_K)
}

/// 同上，但门槛倍率 `k` 由调用方给（界面上的"片边"滑杆）。
///
/// `k` 是"多透才算没有胶片"的唯一开关，而判决效果同时取决于两件事：`base`
/// 本身准不准，以及灯箱漏光有多强。当 `base` 被估高（例如片基被当成灯箱白），
/// 门槛会跟着漂到灯箱亮度上：同一条片边里，纯白齿孔过线、漏光带不过线，
/// 白块与片基色在带内交错——看上去就是"片框残缺"。
/// 固定值无法同时覆盖所有片基，所以把 `k` 暴露给用户按实际片边微调；
/// 调低则更多像素判纸白（片边更白、齿孔外扩），调高则相反（齿孔可能消失）。
pub fn is_frame_outside_with(p: [f32; 3], base: [f32; 3], k: f32) -> bool {
    // "比片基还透"必须**逐层**成立：三层同时高于未曝光片基，物理上只有
    // 没有胶片的地方做得到（齿孔、灯箱直射、片夹白纸）。画面里再亮的白墙，
    // 红层也一定被染料压在片基以下（p.R < base.R），所以不会误吃高光；
    // 也就不需要再靠"够亮 + 近中性"去兜底——那条色度门槛会把灯箱的暖色散射
    // 挡在外面，边缘只剩 1 像素的硬跳变，反相后就是一圈毛刺。
    //
    // 默认 10% 的实测依据（_DSC1566film）：齿孔中心 R 是片基的 1.126 倍、
    // G 1.92 倍、B 6.2 倍；而孔外那圈"青蓝漏光带"（孔的白光散射到片边上，
    // 约 20 像素宽）R 只高 6.9%、G 高 43%、B 高 157%。门槛若低到 5%，整条
    // 漏光带会被吃成"无胶片区"，孔洞凭空外扩一圈、边缘变成圆弧——也就是糊边。
    // 10% 让 R 这一层刚好把漏光带挡在外面（1.069 < 1.10），孔心仍然远在门槛上；
    // k 调低时即向这套边界靠近，调高则向片基色收回。
    p[0] > base[0] * k && p[1] > base[1] * k && p[2] > base[2] * k
}

fn percentile_base(img: &FloatImage, q: f32) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        let plane = img.plane(c);
        let stride = (plane.len() / 400_000).max(1);
        let mut vals: Vec<f32> = plane.iter().step_by(stride).copied().collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((vals.len().saturating_sub(1)) as f32 * q) as usize;
        out[c] = vals[idx].max(EPS);
    }
    out
}

/// 逐段中性增益估计结果。
#[derive(Clone, Debug)]
pub struct GainFit {
    pub gain: GainCurve,
    pub confidence: f32,
    pub samples: usize,
    /// 修正后中性色剩余色度（RMS，密度单位）
    pub residual: f32,
}

/// 估计逐段相对增益：在每个密度段取低彩度像素，求 `g` 使 `B·(g∘D)` 三通道相等。
///
/// 目标：`Σ [(d1-d2)^2 + (d1-d3)^2] + λ||g-1||^2`，再按均值归一到 1（尺度由曝光吸收）。
/// 中性修正后三通道残差（密度单位）。
fn gain_residual(b0: &Mat3, d: &[f32; 3], g: [f32; 3]) -> f32 {
    let s = [d[0] * g[0], d[1] * g[1], d[2] * g[2]];
    let dd = crate::numeric::mat3_vec3(b0, s);
    ((dd[0] - dd[1]).powi(2) + (dd[0] - dd[2]).powi(2)).sqrt()
}

/// 解 3×3 正规方程：求 `g` 使中性约束的加权残差最小，先验目标为 `g = 1`。
fn gain_solve(
    b0: &Mat3,
    samples: &[([f32; 3], f32)],
    w_extra: Option<&[f32]>,
    lambda: f32,
) -> [f32; 3] {
    let mut ata = [[0.0f32; 3]; 3];
    let mut atb = [0.0f32; 3];
    for (k, (d, wc)) in samples.iter().enumerate() {
        let w = wc * w_extra.map_or(1.0, |e| e[k]);
        let c1 = [
            (b0[0][0] - b0[1][0]) * d[0],
            (b0[0][1] - b0[1][1]) * d[1],
            (b0[0][2] - b0[1][2]) * d[2],
        ];
        let c2 = [
            (b0[0][0] - b0[2][0]) * d[0],
            (b0[0][1] - b0[2][1]) * d[1],
            (b0[0][2] - b0[2][2]) * d[2],
        ];
        for c in [c1, c2] {
            for i in 0..3 {
                for j in 0..3 {
                    ata[i][j] += w * c[i] * c[j];
                }
            }
        }
    }
    let lam = lambda.max(1e-6);
    for i in 0..3 {
        ata[i][i] += lam;
        atb[i] = lam;
    }
    let g = solve_linear(ata, atb).unwrap_or([1.0; 3]);
    let mean = (g[0] + g[1] + g[2]) / 3.0;
    if mean.abs() > 1e-3 {
        [g[0] / mean, g[1] / mean, g[2] / mean]
    } else {
        [1.0; 3]
    }
}

/// 稳健重加权估计：先最小二乘，再按残差迭代收紧（低饱和彩色内容被当离群点）。
/// 返回 `(增益, 内点质量)`；内点质量用于置信度与保守收缩。
fn irls_gain(b0: &Mat3, samples: &[([f32; 3], f32)], lambda: f32) -> ([f32; 3], f32) {
    if samples.len() < 50 {
        return ([1.0; 3], 0.0);
    }
    let mut g = gain_solve(b0, samples, None, lambda);
    let mut w = vec![1.0f32; samples.len()];
    let mut inlier = 0.0f32;
    for _ in 0..4 {
        let res: Vec<f32> = samples
            .iter()
            .map(|(d, _)| gain_residual(b0, d, g))
            .collect();
        let mut sorted = res.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let scale = (sorted[sorted.len() / 4] * 1.5).clamp(0.01, 0.10);
        inlier = 0.0;
        for (k, r) in res.iter().enumerate() {
            let t = (*r / scale).powi(2);
            w[k] = 1.0 / (1.0 + t * t);
            if *r <= 1.5 * scale {
                inlier += samples[k].1 * w[k];
            }
        }
        g = gain_solve(b0, samples, Some(&w), lambda);
    }
    (g, inlier)
}

pub fn fit_gains(img: &FloatImage, model: &NegativeModel, lambda_prior: f32) -> GainFit {
    let n_bands = 3;
    let centers = model.gain.centers;
    let stride = (img.len() / 300_000).max(1);

    let samples: Vec<([f32; 3], f32, usize)> = (0..img.len())
        .step_by(stride)
        .filter_map(|i| {
            let d = model.rel_densities(img.pixel(i));
            let mean = (d[0] + d[1] + d[2]) / 3.0;
            if mean < 0.02 || mean > 4.0 {
                return None;
            }
            let chroma = d[0].max(d[1]).max(d[2]) - d[0].min(d[1]).min(d[2]);
            // 最近的段
            let mut band = 0;
            for b in 1..n_bands {
                if (mean - centers[b]).abs() < (mean - centers[band]).abs() {
                    band = b;
                }
            }
            Some((d, chroma, band))
        })
        .collect();

    let b0 = &model.unmix;
    // 彩度权重（软 Shades-of-Gray）：只作粗筛，真正的"是否中性"交给 IRLS 残差
    let soft_w = |c: f32| (-((c / 0.30f32).powi(2))).exp();
    let all: Vec<([f32; 3], f32)> = samples
        .iter()
        .filter(|(_, c, _)| *c < 0.9)
        .map(|(d, c, _)| (*d, soft_w(*c)))
        .collect();

    // 卷级锚点：整幅共享的中性方向（色偏通常跨密度段一致）
    let (g_all, mass_all) = irls_gain(b0, &all, lambda_prior);
    let evidence = (mass_all / 5_000.0).clamp(0.0, 1.0);

    let mut out_gain = GainCurve {
        centers,
        g: [[1.0; 3]; 3],
    };
    for band in 0..n_bands {
        // 逐段只在锚点附近按证据量偏离，保留"色罩随密度漂移"的自由度
        let band_samples: Vec<([f32; 3], f32)> = samples
            .iter()
            .filter(|(_, c, b)| *b == band && *c < 0.9)
            .map(|(d, c, _)| (*d, soft_w(*c)))
            .collect();
        let (g_band, mass_band) = irls_gain(b0, &band_samples, lambda_prior);
        let wb = (mass_band / 5_000.0).clamp(0.0, 1.0) * 0.5;
        for c in 0..3 {
            let g = g_all[c] + wb * (g_band[c] - g_all[c]);
            out_gain.g[band][c] = 1.0 + (g - 1.0) * evidence;
        }
    }

    // 残差：修正后中性色剩余色度（按彩度权重）
    let mut total_resid = 0.0f64;
    let mut weight_used = 0.0f64;
    for (d, w) in all.iter() {
        let mean = (d[0] + d[1] + d[2]) / 3.0;
        let g = out_gain.at(mean);
        let r = gain_residual(b0, d, g) as f64;
        total_resid += *w as f64 * r * r;
        weight_used += *w as f64;
    }

    let rms = (total_resid / (2.0 * weight_used.max(1e-9))).sqrt() as f32;
    // 置信度只看"中性证据量"：残余色度是内容本身的颜色，不代表估计不可信
    let conf = evidence;
    GainFit {
        gain: out_gain,
        confidence: conf,
        samples: all.len(),
        residual: rms,
    }
}

/// 相纸锚点：逐层取高分位染料密度，夹在物理区间。
pub fn estimate_d_ref(img: &FloatImage, model: &NegativeModel) -> [f32; 3] {
    let stride = (img.len() / 200_000).max(1);
    let mut vals: Vec<[f32; 3]> = (0..img.len())
        .step_by(stride)
        .map(|i| model.dye_at(img.pixel(i)))
        .collect();
    if vals.is_empty() {
        vals.push([1.0; 3]);
    }
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        let mut v: Vec<f32> = vals.iter().map(|d| d[c]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((v.len() - 1) as f32 * 0.995) as usize;
        out[c] = v[idx].clamp(1.2, 3.6);
    }
    out
}

/// 并行统计：逐段平均色度（诊断"中性轴漂移"用）。
pub fn chroma_by_band(img: &FloatImage, model: &NegativeModel) -> Vec<(f32, f32)> {
    let samples: Vec<(f32, f32)> = (0..img.len())
        .into_par_iter()
        .step_by((img.len() / 50_000).max(1))
        .map(|i| {
            let d = model.rel_densities(img.pixel(i));
            let mean = (d[0] + d[1] + d[2]) / 3.0;
            let chroma = d[0].max(d[1]).max(d[2]) - d[0].min(d[1]).min(d[2]);
            (mean, chroma)
        })
        .collect();
    let mut bins: Vec<(f32, f32, f32)> = vec![(0.0, 0.0, 0.0); 8];
    for (m, c) in samples {
        let b = ((m / 0.4) as usize).min(7);
        bins[b].0 += m;
        bins[b].1 += c;
        bins[b].2 += 1.0;
    }
    bins.iter()
        .filter(|b| b.2 > 0.0)
        .map(|b| (b.0 / b.2, b.1 / b.2))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::default_matrix;

    #[test]
    fn base_detection_finds_rebate() {
        // 边框平坦、画面有细节：只有边框块能通过"平坦"筛选
        let mut img = FloatImage::new(96, 96);
        for y in 0..96 {
            for x in 0..96 {
                let on_border = x < 10 || y < 10 || x >= 86 || y >= 86;
                let px = if on_border {
                    [0.32, 0.20, 0.12]
                } else {
                    let k = 0.30 * (((x / 2 + y / 2) % 2) as f32);
                    [0.20 + k, 0.25 - k * 0.5, 0.10 + k]
                };
                img.set_pixel(y * 96 + x, px);
            }
        }
        let est = estimate_base(&img);
        assert_eq!(est.source, BaseSource::Rebate);
        assert!((est.base[0] - 0.32).abs() < 0.02, "{:?}", est.base);
        assert!((est.base[1] - 0.20).abs() < 0.02, "{:?}", est.base);
        assert!((est.base[2] - 0.12).abs() < 0.02, "{:?}", est.base);
    }

    #[test]
    fn gains_correct_prior_matrix_mismatch() {
        // 真值矩阵与档案先验不一致（行增益 1.0/1.15/0.9）：拟合应把中性灰阶拉回中性
        use crate::synth::{synthetic_negative, SynthOptions, BASE_TRANSMITTANCE};
        let base_a = default_matrix();
        let a_true = [
            [base_a[0][0], base_a[0][1], base_a[0][2]],
            [base_a[1][0] * 1.15, base_a[1][1] * 1.15, base_a[1][2] * 1.15],
            [base_a[2][0] * 0.90, base_a[2][1] * 0.90, base_a[2][2] * 0.90],
        ];
        let opts = SynthOptions {
            width: 480,
            height: 320,
            rebate: 12,
            a: a_true,
            noise: 0.0,
            ..Default::default()
        };
        let (neg, _pos, _truth) = synthetic_negative(&opts);

        let model0 = NegativeModel::from_matrix(BASE_TRANSMITTANCE, &base_a, [2.1, 2.2, 2.1])
            .expect("invertible");
        let fit = fit_gains(&neg, &model0, 0.05);

        // 灰阶带（v > 0.85 的底部条带）修正前后彩度
        let chroma = |model: &NegativeModel| {
            let mut sum = 0.0f64;
            let mut cnt = 0f64;
            for y in (opts.height as f32 * 0.88) as usize..opts.height - opts.rebate - 1 {
                for x in (opts.rebate + 1)..(opts.width - opts.rebate - 1) {
                    let d = model.dye_at(neg.pixel(y * opts.width + x));
                    let c = d[0].max(d[1]).max(d[2]) - d[0].min(d[1]).min(d[2]);
                    sum += c as f64;
                    cnt += 1.0;
                }
            }
            (sum / cnt.max(1.0)) as f32
        };
        let before = chroma(&model0);
        let corrected = NegativeModel {
            gain: fit.gain.clone(),
            ..model0.clone()
        };
        let after = chroma(&corrected);
        assert!(
            after < before * 0.5,
            "增益未起作用: before={before:.4} after={after:.4} g={:?}",
            fit.gain.g
        );
    }

    #[test]
    fn gains_stay_near_unity_for_already_neutral_image() {
        // 构造一个"已中性"的画面：三层密度相等 ⇒ 相对密度相等
        let base = [0.30f32, 0.18, 0.09];
        let model = NegativeModel::from_matrix(base, &default_matrix(), [2.1, 2.2, 2.1]).unwrap();
        let mut img = FloatImage::new(128, 128);
        for y in 0..128 {
            for x in 0..128 {
                let t = x as f32 / 127.0;
                // 中性：三层同密度 ⇒ 透过率按同一因子下降
                let dens = 0.2 + 2.0 * t;
                let mut px = [0.0f32; 3];
                for c in 0..3 {
                    px[c] = base[c] * 10.0f32.powf(-dens);
                }
                img.set_pixel(y * 128 + x, px);
            }
        }
        let fit = fit_gains(&img, &model, 0.5);
        for band in 0..3 {
            for c in 0..3 {
                // 中性场景下增益应保持接近 1（串扰导致的偏差由先验承担）
                assert!(
                    (fit.gain.g[band][c] - 1.0).abs() < 0.35,
                    "band {band} c{c}: {:?}",
                    fit.gain.g[band]
                );
            }
        }
        assert!(fit.residual.is_finite());
    }
}
