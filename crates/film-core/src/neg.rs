//! 负片反演内核：除片基色 → 量光密度 → 印相 → 显示编码。
//!
//! ## 校准到底是哪一步
//!
//! 负片上的颜色信息只有一件事：`D = log10(base / x)`，即以**该图自己的片基**
//! 为白点的光密度。中性灰在负片上的物理特性是"三层染料各自记录同一段 log 曝光"，
//! 所以除掉片基色之后，它在三个通道上的 `D` 是**相等**的。
//!
//! 因此色彩校准不是"让中性样本的输出接近灰"，而只是**把片基色除准**：
//! `base` 三个读数测准了，中性就自然中性；测偏了，整幅就偏。所有自由度全在
//! `base` 这一处，看得见、改得动、可复现。
//!
//! 反之，旧管线把"输出统计等于某个目标"当成校准本身（迭代改 `anchor_t`、
//! 迭代改 `alpha`，再叠 `w_deep / w_mid / w_purple / w_high / sky`），那是在
//! 假设"画面平均色应该是灰"。天空、草地、红衣都不是灰，所以闭环只能把参与
//! 拟合的那张图修好——留出检验的 −0.05 暗部偏蓝就是这么暴露的。
//!
//! ## 结构
//!
//! 1. `D = log10(base / x)`，片基处 0，越密越大。
//! 2. `E = bias - gain * D`，`gain` 三通道相同（除掉片基后三层等价），
//!    `bias = log10(1 + black)` 对三通道相同 → 片基严格纯黑，没有灰底也没有彩底。
//! 3. `P = exposure * (1 + black - 10^E)`、`P^gamma`、高光柔化、sRGB 编码。
//!    端点由 `black / gain` 钉死，与颜色无关，所以调反差、调曝光不会把黑点变灰。

use crate::image::{linear_to_srgb, FloatImage};
use rayon::prelude::*;

/// 2^-32（−32 EV），只用来防 log(0)。
const EPS: f32 = 2.328_306_4e-10;

/// 纸黑反射率：`10^E = 1 + black` 处输出 0。
pub const PAPER_BLACK: f32 = 0.0755;

/// 相纸号：把中间调落到正常亮度，并把纸黑附近的残余偏差压成纯黑。
pub const PAPER_GAMMA: f32 = 4.0;

#[inline]
pub fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// 最小通道：判断"三通道同时透"的白点／片基用。
#[inline]
fn min3(c: [f32; 3]) -> f32 {
    c[0].min(c[1]).min(c[2])
}

/// 一张负片只有两个物理参考量。
#[derive(Clone, Copy, Debug)]
pub struct Samples {
    /// 未曝光片基的透过率（逐通道）。负片上唯一绝对已知的参考，也是颜色校准的唯一自由度。
    pub base: [f32; 3],
    /// 画面最密处的透过率，决定该片实际记录的量程。
    pub densest: [f32; 3],
}

impl Samples {
    /// 固定规则取样：片基取边缘带橙色块的中位段（见 [`detect_base`]），
    /// 最密取画面区 1% 分位。判据不随图像内容改，所以不存在"这张图好看那张图崩"。
    pub fn analyze(img: &FloatImage) -> Samples {
        let n = img.len();
        if n == 0 {
            return Samples {
                base: [0.5; 3],
                densest: [0.05; 3],
            };
        }
        let stride = (n / 160_000).max(1);
        let mut px: Vec<[f32; 3]> = (0..n).step_by(stride).map(|i| img.pixel(i)).collect();
        // 灯箱直射/白纸：三通道都亮 = 没有胶片遮挡（片基的蓝通道远低于红绿）。
        px.retain(|p| p[0].min(p[1]).min(p[2]) < 0.70);
        if px.is_empty() {
            px = (0..n).step_by(stride).map(|i| img.pixel(i)).collect();
        }
        // 亮序按亮度（找最密处用），白序按**最小通道**（找白点用）。
        //
        // 负片上"亮度最高"的像素往往是天空：蓝色物体在负片里呈黄，蓝通道染料少
        // 所以 B 很高、R 却很低，亮度能顶到最高，但它不是白点。按亮度平均出来的
        // base 会偏青，反演后整幅偏绿。三通道同时透才接近未曝光片基／中性白。
        let mut by_lum = px.clone();
        by_lum.sort_by(|a, b| {
            luma(*a)
                .partial_cmp(&luma(*b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut by_white = px;
        by_white.sort_by(|a, b| {
            min3(*a)
                .partial_cmp(&min3(*b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let len = by_lum.len();
        let mut base = detect_base(img);
        // 片基是"未曝光"＝该片里**最透**的橙色区，只可能出现在画面外沿。
        // 画面被裁到只剩内容时，边缘带里的橙色块其实只是画面中的暖色区，
        // 照它当 Dmin 会把白点压到画面中间，整幅输出随之偏暗（边缘带片基
        // 明显暗于画面最亮处即是这种情形）。这时物理上唯一可用的白点就是
        // 画面最亮处（白墙/高光），用它当 Dmin。
        if len >= 200 {
            let peak = mean_of(&by_white[(len * 995 / 1000).min(len - 1)..]);
            if luma(base) < 0.85 * luma(peak) {
                base = peak;
            }
        }
        let base_lum = luma(base).max(EPS);
        // 画面区：比片基暗，又没掉进无光区（片夹遮挡、灯箱外暗底）。
        let start = by_lum
            .iter()
            .position(|p| luma(*p) > 0.03 * base_lum)
            .unwrap_or(0);
        let end = by_lum
            .iter()
            .rposition(|p| luma(*p) < 0.85 * base_lum)
            .unwrap_or(len);
        let scene = if end > start + 32 {
            &by_lum[start..end]
        } else {
            &by_lum[..]
        };
        let densest = mean_of(&scene[..(scene.len() / 100).max(1)]);
        Samples { base, densest }
    }
}

/// 片基检测：片框边缘带的平坦块里，取橙色组的**中位段**均值。
///
/// 两条判据都是物理事实，不是限制：
///   * 未曝光片基只出现在画面外沿（翻拍/扫描时胶片的边缘），不会在画面中间；
///   * C-41 彩色负片的片基必定偏橙（未曝光处青层染料不显影），
///     `G/R` 与 `B/R` 落在区间内；灯箱直射三通道都高，被这条挡掉。
///
/// 边缘带找不到橙色块时（黑白片、E-6 反转片、或片框被裁掉）退回全部平坦块，
/// 同样取中位段。这与 darktable 让用户框选未曝光片基是同一件事，只是判据自动化了。
///
/// 会话路径另有一套 [`crate::calib::estimate_base`]（带 confidence 聚类版本），
/// 两者判据相近但不共享实现：改橙色色域时两边都要看。
fn detect_base(img: &FloatImage) -> [f32; 3] {
    const BLK: usize = 16;
    let w = img.width;
    let h = img.height;
    if w < BLK * 2 || h < BLK * 2 {
        return img.pixel(img.len() / 2);
    }
    let mw = ((w as f32 * 0.12) as usize).max(BLK);
    let mh = ((h as f32 * 0.12) as usize).max(BLK);
    let mut flat: Vec<([f32; 3], f32)> = Vec::new();
    let mut y = 0usize;
    while y + BLK <= h {
        let mut x = 0usize;
        while x + BLK <= w {
            let edge = x < mw || x + BLK > w - mw || y < mh || y + BLK > h - mh;
            if edge {
                let mut m = [0.0f32; 3];
                let mut q = [0.0f32; 3];
                for dy in 0..BLK {
                    for dx in 0..BLK {
                        let p = img.pixel((y + dy) * w + x + dx);
                        for c in 0..3 {
                            m[c] += p[c];
                            q[c] += p[c] * p[c];
                        }
                    }
                }
                let cnt = (BLK * BLK) as f32;
                let mut ok = true;
                for c in 0..3 {
                    m[c] /= cnt;
                    q[c] /= cnt;
                    let sd = (q[c] - m[c] * m[c]).max(0.0).sqrt();
                    // 平坦、非黑、非纯白
                    if m[c] < 0.02 || sd > 0.04 * m[c] + 0.004 {
                        ok = false;
                        break;
                    }
                }
                // 灯箱透光 = 三通道**同时**接近满透过。判据看最暗那一层：
                // 胶片任何一层有染料都会把该层压到片基以下，只有无胶片区
                // （齿孔、灯箱直射、片夹白纸）才能三层同时 ≥0.85。
                // 原判据要求三层都 ≥0.90，(0.99, 0.99, 0.898) 这类白刚好差
                // 一点漏网，混进候选后就成了"最亮平坦块"。
                let blown = m[0].min(m[1]).min(m[2]) >= 0.85;
                if ok && !blown {
                    flat.push((m, luma(m)));
                }
            }
            x += BLK;
        }
        y += BLK;
    }
    if flat.is_empty() {
        return [0.4; 3];
    }
    let orange: Vec<([f32; 3], f32)> = flat
        .iter()
        .copied()
        // 片基色域用区间，不用单点阈值：深橙片基（柯达/富士）约
        // `G/R 0.37、B/R 0.08`，浅橙片基（HARMAN PHOENIX 等）约
        // `G/R 0.78、B/R 0.38`。单点阈值只能覆盖其中一类，另一类会整组落选，
        // 候选归零后退回"最亮块"，`base` 随之落到灯箱白。
        .filter(|(m, _)| {
            let gr = m[1] / m[0];
            let br = m[2] / m[0];
            m[0] > 0.08 && (0.35..=0.85).contains(&gr) && (0.05..=0.65).contains(&br)
        })
        .collect();
    let mut pick = if orange.is_empty() { flat } else { orange };
    // 取中位段，不取最亮段：片边上的**漏光**（灯箱白光散射到片基上）比片基
    // 更透，挑最亮那一批会把它当 Dmin，白点被抬高、整幅随之压暗。
    // 片基在边缘带里占多数，中位段才是"未曝光片基"的稳健估计。
    pick.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let n = pick.len();
    let lo = n / 3;
    let hi = ((n * 2) / 3).max(lo + 1).min(n);
    mean_of(&pick[lo..hi].iter().map(|(m, _)| *m).collect::<Vec<_>>())
}

fn mean_of(v: &[[f32; 3]]) -> [f32; 3] {
    if v.is_empty() {
        return [0.5; 3];
    }
    let mut s = [0.0f32; 3];
    for p in v {
        s[0] += p[0];
        s[1] += p[1];
        s[2] += p[2];
    }
    let n = v.len() as f32;
    [s[0] / n, s[1] / n, s[2] / n]
}

/// 创作参数。只表达风格，不参与校准。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// 胶片的物理密度量程（log10 密度跨度）。
    ///
    /// 这是**胶片属性**，不是画面属性：C-41 彩色负片的片基到最大密度约 1.5，
    /// 与这一张照片里恰好有没有深色调毫无关系。若按"画面最暗点"定量程，
    /// 每张图都会被拉满——没有暗调的片子整幅泛白，有大片阴影的片子整幅压黑，
    /// 那就是旧管线的 `auto_tone`。
    pub film_range: f32,
    /// 相纸号（反差）
    pub gamma: f32,
    /// 印相曝光偏移（EV）
    pub exposure_ev: f32,
    /// 高光柔化（0..1）
    pub gloss: f32,
    /// 饱和度（围绕亮度）
    pub saturation: f32,
    /// 黑白
    pub mono: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            film_range: 0.0,
            gamma: PAPER_GAMMA,
            exposure_ev: 0.0,
            gloss: 0.75,
            saturation: 0.0,
            mono: false,
        }
    }
}

/// 反演内核。`base/gain/bias` 一次解析求出，之后只读。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Negative {
    pub base: [f32; 3],
    /// 三通道相同的密度增益：把该片的量程铺满印相密度区间。
    pub gain: f32,
    /// 三通道相同的密度偏移：片基处 `E = bias`，`10^bias = 1 + black` → 纯黑。
    pub bias: f32,
    pub black: f32,
    pub tuning: Tuning,
}

impl Negative {
    /// 一次解析求解。没有迭代，不看渲染结果。
    pub fn solve(s: &Samples, tuning: Tuning) -> Negative {
        let mut base = s.base;
        for b in base.iter_mut() {
            *b = b.max(EPS);
        }
        // 量程 = 这张片子**实际记录**的密度跨度（片基 → 画面最密处）。
        //
        // 这是物理量程，不是审美目标：它把"片基 = 纸白、最密处 = 纸黑"两个端点
        // 同时钉死，画面在这之间按密度线性落位。高调片记录得薄，输出就是高调；
        // 欠曝片整幅偏薄，输出就偏暗——这正是负片的事实，不能靠看渲染结果把它拉回来。
        // `film_range > 0` 时才用人工给定的量程覆盖。
        let dmax = if tuning.film_range > 0.0 {
            tuning.film_range.clamp(0.8, 3.0)
        } else {
            let mut d = 0.0f32;
            for c in 0..3 {
                d += (base[c] / s.densest[c].max(EPS)).log10();
            }
            (d / 3.0).clamp(0.25, 3.0)
        };
        let span = ((1.0 + PAPER_BLACK) / PAPER_BLACK).log10();
        Negative {
            base,
            gain: span / dmax,
            bias: (1.0 + PAPER_BLACK).log10(),
            black: PAPER_BLACK,
            tuning,
        }
    }

    /// 单像素：负片线性读数 → 显示域（已 sRGB 编码）。
    #[inline]
    pub fn render(&self, x: [f32; 3]) -> [f32; 3] {
        let t = &self.tuning;
        let amp = 2f32.powf(t.exposure_ev.clamp(-6.0, 6.0));
        let s = t.gloss.clamp(1e-4, 1.0);
        let g = t.gamma.clamp(1.0, 8.0);
        let mut lin = [0.0f32; 3];
        for c in 0..3 {
            let d = (self.base[c] / x[c].max(EPS)).log10().max(0.0);
            let e = (-self.gain * d + self.bias).min(2.0);
            let p = (amp * (1.0 + self.black - 10f32.powf(e))).max(0.0);
            let pg = p.powf(g);
            // 相纸光泽：P^gamma 超过 gloss 后指数收敛到 1，连续且一阶可导。
            lin[c] = if pg > s {
                s + (1.0 - s) * (1.0 - (-(pg - s) / (1.0 - s)).exp())
            } else {
                pg
            };
        }
        if t.mono {
            let y = luma(lin);
            lin = [y; 3];
        } else if t.saturation.abs() > 1e-4 {
            let y = luma(lin);
            for v in lin.iter_mut() {
                *v = y + (*v - y) * (1.0 + t.saturation);
            }
        }
        [
            linear_to_srgb(lin[0]),
            linear_to_srgb(lin[1]),
            linear_to_srgb(lin[2]),
        ]
    }

    /// 整图渲染。`alpha` 给出片框外权重（255 = 无胶片区，直接给纸白）。
    pub fn render_image(&self, img: &FloatImage, alpha: Option<&[u8]>) -> FloatImage {
        let n = img.len();
        let v: Vec<[f32; 3]> = (0..n)
            .into_par_iter()
            .map(|i| {
                let a = alpha.map_or(0u8, |a| a[i]);
                if a == 255 {
                    return [1.0; 3];
                }
                let c = self.render(img.pixel(i));
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
        let mut out = FloatImage::new(img.width, img.height);
        let (a0, rest) = out.data.split_at_mut(n);
        let (b0, c0) = rest.split_at_mut(n);
        for (i, px) in v.iter().enumerate() {
            a0[i] = px[0];
            b0[i] = px[1];
            c0[i] = px[2];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 物理自洽的样本：中性灰在负片上三通道密度相等。
    fn silver_at(d: f32) -> [f32; 3] {
        let base = [1.0f32, 0.45, 0.25];
        [base[0] * 10f32.powf(-d), base[1] * 10f32.powf(-d), base[2] * 10f32.powf(-d)]
    }

    fn samples() -> Samples {
        Samples {
            base: [1.0, 0.45, 0.25],
            densest: silver_at(1.2),
        }
    }

    #[test]
    fn base_renders_pure_black() {
        let s = samples();
        for &gamma in &[1.0f32, 4.0, 8.0] {
            for &ev in &[-3.0f32, 0.0, 3.0] {
                let t = Tuning {
                    gamma,
                    exposure_ev: ev,
                    ..Default::default()
                };
                let ng = Negative::solve(&s, t);
                assert_eq!(ng.render(s.base), [0.0, 0.0, 0.0], "gamma={gamma} ev={ev}");
            }
        }
    }

    #[test]
    fn neutral_stays_neutral_at_every_density() {
        let ng = Negative::solve(&samples(), Tuning::default());
        for d in [0.15f32, 0.35, 0.55, 0.80, 1.10] {
            let out = ng.render(silver_at(d));
            let mx = out[0].max(out[1]).max(out[2]);
            let mn = out[0].min(out[1]).min(out[2]);
            assert!(mx - mn < 0.02, "D={d} 应中性: {out:?}");
        }
    }

    #[test]
    fn densest_is_paper_white() {
        let s = samples();
        let ng = Negative::solve(&s, Tuning::default());
        let out = ng.render(s.densest);
        assert!(luma(out) > 0.8, "最密处应接近纸白: {out:?}");
    }

    #[test]
    fn monotone_in_density() {
        let ng = Negative::solve(&samples(), Tuning::default());
        let mut prev = -1.0;
        for i in 0..=60 {
            let d = i as f32 / 60.0 * 1.3;
            let l = luma(ng.render(silver_at(d)));
            assert!(l >= prev - 1e-4, "密度越大应越亮: d={d} {l} < {prev}");
            prev = l;
        }
    }

    #[test]
    fn grade_keeps_white_endpoint() {
        let s = samples();
        // 相纸号只改中段斜率：`pg = 0/1` 两个端点在 `P^gamma` 下不动。
        for &gamma in &[1.5f32, 4.0, 8.0] {
            let t = Tuning {
                gamma,
                ..Default::default()
            };
            let ng = Negative::solve(&s, t);
            assert_eq!(ng.render(s.base), [0.0, 0.0, 0.0]);
            assert!(luma(ng.render(s.densest)) > 0.7, "gamma={gamma}");
        }
    }

    #[test]
    fn print_exposure_is_monotone() {
        let s = samples();
        let base = Negative::solve(&s, Tuning::default());
        let up = Negative::solve(
            &s,
            Tuning {
                exposure_ev: 1.0,
                ..Default::default()
            },
        );
        let mid = luma(base.render(silver_at(0.6)));
        // 印相曝光是整体平移：只改亮度，不改颜色。
        assert!(luma(up.render(silver_at(0.6))) > mid);
        let a = up.render(silver_at(0.6));
        let mx = a[0].max(a[1]).max(a[2]);
        let mn = a[0].min(a[1]).min(a[2]);
        assert!(mx - mn < 0.02, "曝光不该引入偏色: {a:?}");
    }
}
