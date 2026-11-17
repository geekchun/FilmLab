//! 虚拟暗房：把"三层染料密度"当作真实印相曝光的输入，走相纸 H&D 曲线，
//! 再用相纸染料光谱投影得到纸面反射率，最后做显示编码。
//!
//! ## 重构后的核心原则：所有调整都在"端点固定"的幂律域里做
//!
//! 每层先把相对密度压成 `t ∈ [0,1]`：`t = (d_ref - dye) / d_ref`，
//!   - 片基（dye = 0）  → t = 1 → 纸黑
//!   - 负片最密处       → t = 0 → 纸白
//!   - 比片基还透的（漏光边）→ dye < 0 → t > 1 → 钳到 1 → 也是纸黑
//!
//! 之后的中调对齐、曝光、反差全部写成 `t^p` 或保端点的 S 曲线：
//!   0^p = 0、1^p = 1，S(0) = 0、S(1) = 1。
//! 这就是为什么"调曝光 / 调反差 / 自动中调"都不会再把片基抬成灰——
//! 之前用加性 log-exposure（`e = ... + exposure + cmy`）时，滑杆一动黑点就跟着漂，
//! 三通道还各自漂得不一样，片基于是变成深棕/深蓝的杂色。

use crate::numeric::{clampf, mat3_vec3, softplus_lower, Mat3};

/// 出片风格。所有预设都只动**端点固定**的输出域参数：
/// 片基仍是纯黑、纸白仍是纯白，改的是中间调与彩色的关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// 自然：接近光学印相，尽量少加工（默认）
    Natural,
    /// Negative Lab Pro 风格：中调灰准、天空更纯、暗部收敛
    Nlp,
    /// 鲜明：反差与饱和更足
    Vivid,
    /// 柔和：低反差、低饱和，适合人像
    Soft,
    /// 黑白
    Mono,
}

impl Look {
    pub fn from_name(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "nlp" => Look::Nlp,
            "vivid" => Look::Vivid,
            "soft" => Look::Soft,
            "mono" | "bw" => Look::Mono,
            _ => Look::Natural,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Look::Natural => "natural",
            Look::Nlp => "nlp",
            Look::Vivid => "vivid",
            Look::Soft => "soft",
            Look::Mono => "mono",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Look::Natural => "自然",
            Look::Nlp => "NLP 风格",
            Look::Vivid => "鲜明",
            Look::Soft => "柔和",
            Look::Mono => "黑白",
        }
    }

    /// 预设参数。tint 行和必须为 1（否则中性灰会被整体偏色）。
    ///
    /// 所有数值只表达**风格意图的相对关系**，不表达任何"让某张图的统计指标
    /// 落到某个数"的绝对量：
    ///   * `mid_shift` 是相对自动锚点的档数偏移，0 就是"不干预"；
    ///   * `sky` 是"修缺陷"的强度，正常片不该超过 0.4，且必须按意图单调
    ///     （越艳越纯净 → 越大），不能为了贴某个参考值而破坏排序。
    pub fn params(self) -> LookParams {
        match self {
            // 标准：轻微去青 + 天空提纯。这些是"修缺陷"的量，不改变胶片本色
            Look::Natural => LookParams {
                grade: 1.00,
                mid_shift: 0.0,
                saturation: 0.02,
                sky: 0.30,
                shadow_clean: 1.0,
                tint: [
                    [1.02, -0.01, -0.01],
                    [-0.005, 1.00, 0.005],
                    [-0.01, -0.01, 1.02],
                ],
            },
            Look::Nlp => LookParams {
                // NLP 的意图：比标准更硬、中调略低、暗部收得更干净
                grade: 1.22,
                mid_shift: -0.15,
                saturation: 0.10,
                sky: 0.34,
                shadow_clean: 1.25,
                // 极轻的蓝红分离：让天空的 B 更纯、白墙更暖一点
                tint: [
                    [1.06, -0.03, -0.03],
                    [-0.01, 1.00, 0.01],
                    [-0.02, -0.02, 1.04],
                ],
            },
            Look::Vivid => LookParams {
                // 鲜艳：更饱和、天空最纯净，中调略抬
                grade: 1.18,
                mid_shift: 0.10,
                saturation: 0.28,
                sky: 0.38,
                shadow_clean: 1.0,
                tint: [
                    [1.04, -0.03, -0.01],
                    [-0.02, 1.01, 0.01],
                    [-0.03, -0.03, 1.06],
                ],
            },
            Look::Soft => LookParams {
                // 柔和：低反差、抬中调、轻微降饱和；不做去青
                grade: 0.85,
                mid_shift: 0.25,
                saturation: -0.12,
                sky: 0.0,
                shadow_clean: 0.8,
                tint: [
                    [1.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0],
                    [0.0, 0.0, 1.0],
                ],
            },
            Look::Mono => LookParams {
                grade: 1.10,
                mid_shift: 0.0,
                saturation: -1.0,
                sky: 0.0,
                shadow_clean: 1.0,
                tint: [
                    [1.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0],
                    [0.0, 0.0, 1.0],
                ],
            },
        }
    }
}

/// 风格在输出域上的具体参数（在 sRGB 显示值上生效）。
#[derive(Clone, Debug)]
pub struct LookParams {
    /// 相纸反差（S 曲线指数，1 = 恒等）
    pub grade: f32,
    /// 饱和度增益（围绕 luma，-1 = 黑白）
    pub saturation: f32,
    /// 蓝青系纯度：天空、水面这类"蓝主导"像素提纯（0 = 关，0.5 ≈ NLP 的观感）
    pub sky: f32,
    /// 暗部去蓝强度（1 = 现状）：阴影发蓝是"整幅发青"的一大来源
    pub shadow_clean: f32,
    /// 风格的中调偏移（档，正 = 更亮）。
    ///
    /// 只作用在 `anchor_t` 上，**不**参与显示域幂次：绝对亮度永远由自动锚点
    /// 按每张图自己算（`MID_TARGET` + 闭环校准），风格只表达"比自动落点偏亮
    /// 或偏暗多少档"。之前这里是显示域绝对值，结果是靠对着一张测试图的
    /// p50 反解出来的，换任何一张正常曝光的图都会整体压暗半档以上。
    pub mid_shift: f32,
    /// 输出域色矩阵（行和 = 1，保持纸白/纸黑）
    pub tint: Mat3,
}

impl Default for LookParams {
    fn default() -> Self {
        Self {
            grade: 1.0,
            saturation: 0.0,
            sky: 0.0,
            shadow_clean: 1.0,
            mid_shift: 0.0,
            tint: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }
}

/// Photoshop「自动色调」的等价实现：在**显示域**逐通道做一次自动色阶。
///
/// Photoshop 的 Auto Tone 就是三次独立的 Auto Levels：R、G、B 各自把
/// 直方图两端各裁掉 0.5%，剩下的拉满 0..1。它"大部分情况下就很好"的原因：
///   * 端点由**实际直方图**决定，不受噪点/极端像素影响（0.5% 分位足够稳）；
///   * 逐通道独立拉伸天然带白平衡——哪层染料偏多，它那层就被压回去；
///   * 只动端点，没有曲线造型，所以不会把画面塑成"塑料感"。
///
/// 与"自动颜色"的区别：Auto Color 还会把中间调拉到 128 灰（snap neutral
/// midtones），在天空/草地占大头的照片上会被"场景本色"骗过去，把蓝天洗成灰暖。
/// Auto Tone 不做这一步，所以不需要猜测"什么才算中性"。
///
/// 端点固定：`<= lo` 压成 0、`>= hi` 压成 1，中间是线性拉伸。
#[derive(Clone, Debug, PartialEq)]
pub struct AutoTone {
    /// 各通道黑点（显示域 0..1），低于它的像素压成 0
    pub lo: [f32; 3],
    /// 各通道白点，高于它的压成 1
    pub hi: [f32; 3],
    /// 0 = 恒等（自动色调关闭），1 = 全量
    pub strength: f32,
}

impl Default for AutoTone {
    fn default() -> Self {
        Self {
            lo: [0.0; 3],
            hi: [1.0; 3],
            strength: 0.0,
        }
    }
}

impl AutoTone {
    #[inline]
    pub fn is_identity(&self) -> bool {
        self.strength <= 1e-4
    }

    /// 单像素映射。端点固定，中间线性。
    #[inline]
    pub fn apply(&self, v: f32, c: usize) -> f32 {
        if self.is_identity() {
            return v;
        }
        let span = (self.hi[c] - self.lo[c]).max(1e-3);
        clampf((v - self.lo[c]) / span, 0.0, 1.0)
    }
}

/// 从一批**显示域**样本（0..1 的 sRGB 值）推出 [`AutoTone`] 参数。
///
/// `samples` 应当只含画面像素（不含片基/白纸），否则端点会被片边的纯黑/纯白钉死。
///
/// 两处保守处理，避免"自动"过头：
///   * 每侧最多收 30%（lo ≤ 0.30、hi ≥ 0.70）。负片印相本身已经把片基压到纯黑，
///     再让自动色阶收到 0.1 会把暗部整片切掉。
///   * 通道动态范围不足 0.30（薄层、大面积单色）就不拉伸该通道，否则等于放大噪声。
pub fn auto_tone_params(samples: &[[f32; 3]], strength: f32) -> AutoTone {
    if samples.len() < 256 || strength <= 1e-4 {
        return AutoTone::default();
    }
    let n = samples.len();
    let mut lo = [0.0f32; 3];
    let mut hi = [1.0f32; 3];
    for c in 0..3 {
        let mut v: Vec<f32> = samples.iter().map(|s| s[c]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // 两端各收 0.5%：避开扫描噪点、JPEG 振铃与边缘的极端像素
        let l = v[((n as f32 * 0.005) as usize).min(n - 1)];
        let h = v[((n as f32 * 0.995) as usize).min(n - 1)];
        if h - l < 0.30 {
            // 该通道几乎没有可拉伸的层次：不动它
            continue;
        }
        lo[c] = l.min(0.30);
        hi[c] = h.max(0.70);
    }
    AutoTone {
        lo: [lo[0] * strength, lo[1] * strength, lo[2] * strength],
        hi: [
            1.0 - strength * (1.0 - hi[0]),
            1.0 - strength * (1.0 - hi[1]),
            1.0 - strength * (1.0 - hi[2]),
        ],
        strength,
    }
}

/// 自动中调对齐的目标位置（t 域，1 = 纸黑）。
///
/// 取值由参考正片 `_DSC1566.JPG` 反推：它的近中性中调落在 8bit ≈ 138，
/// 对应 t ≈ 0.17。曾经用 0.35 / 0.32 / 0.25，画面都比参考系统性欠曝
/// （逐点差 -50 / -45 / -20 每 255）。这条曲线是"肩部陡、趾部缓"的印相曲线：
/// 中调锚点落在密度量程约 40% 处、却只占纸密度量程的 17%，正好对上参考的影调形状。
pub const MID_TARGET: f32 = 0.17;

/// 把投影矩阵每行缩放到和为 1：中性染料密度的三通道观察值必须相等，
/// 否则中性灰会系统性地偏品红/偏青（行和 1.11 / 1.19 / 1.14 就是这个来源）。
fn normalized_rows(m: Mat3) -> Mat3 {
    let mut out = m;
    for r in 0..3 {
        let s = m[r][0] + m[r][1] + m[r][2];
        let s = if s > 1e-6 { s } else { 1.0 };
        for c in 0..3 {
            out[r][c] = m[r][c] / s;
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct PrintParams {
    /// 相纸满密度锚点：每层"负片最密处"的相对密度（t = 0 的位置）
    pub d_ref: [f32; 3],
    /// 每层中调对齐幂次（端点固定的 `t^alpha`），由 `auto_balance` 给出
    pub alpha: [f32; 3],
    /// 相纸染料层在观察通道上的投影（含纸张染料串扰）
    pub paper_a: Mat3,
    /// 纸白/纸黑（相纸染料密度，非反射率）
    pub paper_dmin: f32,
    pub paper_dmax: f32,
    /// 趾部/肩部软度（归一化曝光单位）
    pub knee_lo: f32,
    pub knee_hi: f32,
    /// 曝光（端点固定：`t^(2^exposure)`，正 = 更亮）
    pub exposure: f32,
    /// 逐层 printer lights 微调（端点固定：叠在 alpha 上）
    pub cmy: [f32; 3],
    /// 逐层白平衡增益（端点固定：叠在 alpha 上）
    pub wb: [f32; 3],
    /// 逐层中调锚点（该层染料密度的中位数）。<= 0 表示不做三点锚定。
    /// 三层量程差很大（实例是 R 0.27 / G 0.54 / B 0.59），一个共同斜率
    /// 要么把薄层的中调压暗（偏青），要么把厚层的中调打亮（偏暖）。
    pub anchor_med: [f32; 3],
    /// 该层中调要落到的 t 值（0..1）。由调用方按"部分中性化"算好：
    /// 纯灰世界中点是 1.0，但天空/绿地占大头的场景会被推到反面偏色，
    /// 所以只对齐一部分，剩下的偏色留给场景本身。
    pub anchor_t: [f32; 3],
    /// 该层染料分布的低分位（片基一侧，t → 1）。<=0 表示不做三点锚定。
    pub anchor_lo: [f32; 3],
    /// 该层染料分布的高分位（最密一侧，t → 0）。
    pub anchor_hi: [f32; 3],
    /// 相纸反差（端点固定的 S 曲线指数，1 = 恒等）
    pub grade: f32,
    /// 显示编码伽马（sRGB ≈ 2.2）
    pub out_gamma: f32,
    /// 饱和度增益（0 = 原始）
    pub saturation: f32,
    /// 出片风格（端点固定，见 [`Look`]）
    pub look: LookParams,
    /// 自动色调（显示域逐通道自动色阶，见 [`AutoTone`]）
    pub auto_tone: AutoTone,
}

impl Default for PrintParams {
    fn default() -> Self {
        Self {
            d_ref: [2.10, 2.25, 2.15],
            alpha: [1.0; 3],
            // 行归一：原来三行的和是 1.11 / 1.19 / 1.14，同一块中性染料
            // 投到观察通道后 G 最暗，中性灰就会带品红（云、白墙都发粉）。
            // 每行缩放到和为 1，中性进中性出，串扰的"夹角"保留。
            paper_a: normalized_rows([
                [1.0, 0.080, 0.030],
                [0.090, 1.0, 0.100],
                [0.030, 0.110, 1.0],
            ]),
            paper_dmin: 0.06,
            paper_dmax: 2.30,
            knee_lo: 0.045,
            knee_hi: 0.030,
            exposure: 0.0,
            cmy: [0.0; 3],
            wb: [1.0; 3],
            anchor_med: [0.0; 3],
            anchor_t: [0.0; 3],
            anchor_lo: [0.0; 3],
            anchor_hi: [0.0; 3],
            grade: 1.0,
            out_gamma: 2.2,
            saturation: 0.0,
            look: LookParams::default(),
            auto_tone: AutoTone::default(),
        }
    }
}

impl PrintParams {
    /// 相纸 H&D 响应：归一化曝光 → 归一化染料密度（趾部 + 直线 + 肩部）
    ///
    /// 必须**严格单调**且 `e >= 1` 时精确返回 1；`e <= 0` 时返回 0。
    #[inline]
    pub fn hd_curve(&self, e: f32) -> f32 {
        // 连续的"软 max(e, 0)"：e=0 精确为 0，e<0 平滑贴地，且严格单调
        let k_lo = self.knee_lo.max(1e-4);
        let lo = clampf(e - softplus_lower(-e, 0.0, k_lo), 0.0, 1.0).max(0.0);
        let k = self.knee_hi.max(1e-4);
        if lo >= 1.0 {
            return 1.0;
        }
        if lo <= 1.0 - k {
            return clampf(lo, 0.0, 1.0);
        }
        let t = (lo - (1.0 - k)) / k;
        clampf((1.0 - k) + k * (t * t * (3.0 - 2.0 * t)), 0.0, 1.0)
    }

    /// 端点与 pivot 都固定的反差曲线（对数几率域绕 pivot 缩放），k = 1 时精确恒等。
    ///
    /// 原来绕 s = 0.5 打反差。三点锚定后画面的中调落在 s ≈ anchor_t（0.17 左右），
    /// 于是"提反差"实际是在把中调往上顶、把暗部压死——滑杆一动整体就过曝，很难用。
    /// pivot 取该层的中调锚点，中性点才不会被反差滑杆推着走。
    #[inline]
    fn s_curve(&self, s: f32, k: f32, pivot: f32) -> f32 {
        if s <= 0.0 {
            return 0.0;
        }
        if s >= 1.0 {
            return 1.0;
        }
        if (k - 1.0).abs() < 1e-4 {
            return s;
        }
        let p = pivot.clamp(0.02, 0.98);
        let lp = (p / (1.0 - p)).ln();
        let l = (s / (1.0 - s)).ln();
        let l2 = (lp + k * (l - lp)).clamp(-30.0, 30.0);
        let e = l2.exp();
        e / (1.0 + e)
    }

    /// 单层：相对密度 → 纸面归一密度（0 = 纸白，1 = 纸黑）
    #[inline]
    fn channel(&self, j: usize, dye: f32) -> f32 {
        let dref = self.d_ref[j].max(1e-3);
        // t = 1 在片基（纸黑），t = 0 在负片最密处（纸白）
        // 若给了中调锚点：dye=0→1、dye=med→MID_TARGET、dye=dref→0。
        // 两端仍然固定，所以片基依旧纯黑；中间的斜率由该层自己的量程决定，
        // 三层薄厚不同也不会把中调推成青/暖。
        let m = self.anchor_med[j];
        let mt = self.anchor_t[j];
        let lo = self.anchor_lo[j];
        let hi = if self.anchor_hi[j] > 0.0 { self.anchor_hi[j] } else { dref };
        let mut t = if hi > lo + 0.05 && m > lo + 0.02 && m < hi && mt > 0.02 && mt < 0.99 {
            // 该层按自己的分布三点（片基分位 / 中位 / 最密分位）拉直：
            // 三层分布形状不同也能被各自吸收，这才是"去色罩"里真正起作用的
            // 逐层归一；共同的中位落点仍由调用方钉在中调灰。
            if dye <= m {
                1.0 - (1.0 - mt) * ((dye - lo) / (m - lo)).clamp(0.0, 1.0)
            } else {
                mt * (1.0 - ((dye - m) / (hi - m)).clamp(0.0, 1.0))
            }
        } else {
            clampf((hi - dye) / (hi - lo).max(1e-3), 0.0, 1.0)
        };
        // 黑场软化：片基附近的密度带（lo .. lo+0.05）整段平滑收到纯黑。
        // 三点锚定把 dye 0→med 拉满 t 1→mt（斜率约 4×），扫描噪声 ±0.005
        // 会被放大成黑底上一粒粒灰斑——齿孔周围、片边外侧最扎眼。
        // 这段密度里本来就没有画面信息（胶片最透处也在片基之上），
        // 压平不会丢层次，却能把"坑坑洼洼"的噪点一并消掉。
        {
            const D_FLOOR: f32 = 0.05;
            let u = ((dye - lo) / D_FLOOR).clamp(0.0, 1.0);
            if u < 1.0 {
                let s = u * u * (3.0 - 2.0 * u);
                let lift = 1.0 - s; // dye≈lo → 1（纯黑），dye=lo+0.05 → 0
                t = t * (1.0 - lift) + lift;
            }
        }
        // 近片基 / 近最密处再钳一段：片基的扫描噪声、base 估计偏差都落在这里，
        // 钳成 0/1 才能保证"一点杂色都没有的黑"与干净的纸白（端点依旧固定）。
        if t >= 0.93 {
            t = 1.0;
        } else if t <= 0.02 {
            t = 0.0;
        }
        // 每层中调对齐（幂律），端点 0/1 严格不动
        let a = (self.alpha[j] * self.wb[j] + self.cmy[j]).clamp(0.15, 4.0);
        let mut s = t.powf(a);
        // 反差绕该层的中调锚点打（pivot 就是锚点被幂律挪过之后的位置），
        // 否则反差滑杆会顺带把中调往上顶，跟曝光滑杆互相打架。
        let pivot_t = if mt > 0.02 && mt < 0.99 { mt } else { 0.5 };
        let pivot = pivot_t.powf(a).clamp(0.02, 0.98);
        s = self.s_curve(s, self.grade.clamp(0.2, 4.0), pivot);
        self.hd_curve(s)
    }

    /// 单像素：染料相对密度 → 显示参考 RGB（0..1）
    #[inline]
    pub fn apply(&self, dye: [f32; 3]) -> [f32; 3] {
        let range = (self.paper_dmax - self.paper_dmin).max(0.2);
        let mut d_paper = [0.0f32; 3];
        for j in 0..3 {
            d_paper[j] = self.paper_dmin + range * self.channel(j, dye[j]);
        }

        // 纸面染料光谱投影 → 反射率
        let d_obs = mat3_vec3(&self.paper_a, d_paper);

        // 纸白/纸黑归一化：相纸自身纸白→1、纸黑→0 铺满显示范围，
        // 纸黑三通道严格同为 0（否则会得到 sRGB≈0.085 的灰罩 + 棕蓝杂色）。
        let row_sum = [
            self.paper_a[0][0] + self.paper_a[0][1] + self.paper_a[0][2],
            self.paper_a[1][0] + self.paper_a[1][1] + self.paper_a[1][2],
            self.paper_a[2][0] + self.paper_a[2][1] + self.paper_a[2][2],
        ];
        let mut rgb = [0.0f32; 3];
        for c in 0..3 {
            let r_white = 10.0f32.powf(-row_sum[c] * self.paper_dmin);
            let r_black = 10.0f32.powf(-row_sum[c] * self.paper_dmax);
            let r = 10.0f32.powf(-d_obs[c]);
            rgb[c] = clampf((r - r_black) / (r_white - r_black).max(1e-6), 0.0, 1.0);
        }

        // 显示编码
        let inv_g = 1.0 / self.out_gamma.max(0.5);
        let mut out = [rgb[0].powf(inv_g), rgb[1].powf(inv_g), rgb[2].powf(inv_g)];

        // 暗部净色：纸黑附近三层趾部斜率不可能完全一致，片边、阴影、黑玻璃
        // 会漏出肉眼可辨的青/红（这正是"整幅偏青"的第一观感来源）。
        // 亮度越低越向中性灰收，l=0 处完全中性，所以片基仍是纯黑。
        let lum = (out[0] + out[1] + out[2]) / 3.0;
        // 极暗处净色：纸黑附近三层趾部斜率不可能一致，片边、黑玻璃会漏青/红，
        // 越暗越向中性灰收，l=0 完全中性，所以片基仍是纯黑。
        let w_deep = ((0.25 - lum) / 0.25).max(0.0).powf(1.3);
        if w_deep > 0.0 {
            for v in out.iter_mut() {
                *v += (lum - *v) * w_deep;
            }
        }
        // 暗部去紫：负片三层趾部斜率不同，极暗处会残留 R 塌陷 / B 溢出的形状，
        // 整体收灰同时抬高 R 与 G，蓝仍压在 G 之上，观感就是蓝紫。
        // 只在很暗的一段生效（0.15 以下），且强度取半——再往上就会开始
        // 洗掉夜色、深蓝水面这些真实存在的冷色。
        let w_purple = ((0.15 - lum) / 0.15).clamp(0.0, 1.0);
        if w_purple > 0.0 && out[2] > out[1] {
            out[2] += (out[1] - out[2]) * w_purple * 0.5;
        }
        // 中间偏暗这一段（阴影里的墙、屋檐、树干）：参考正片是中性灰，
        // 实拍负片却透出明显的蓝（逐像素比对 dB 高出 0.2）。这里只压蓝，
        // 不把整段往灰收（否则会洗掉暗部里真实的彩色）。
        let w_mid = ((0.70 - lum) / 0.70).max(0.0).powf(1.3) * self.look.shadow_clean;
        if w_mid > 0.0 {
            for j in 1..3 {
                if out[j] > lum {
                    out[j] += (lum - out[j]) * w_mid * 0.70;
                }
            }
        }

        // 高光净色：相纸接近纸白时三种染料都到上限，亮部（云、白墙、
        // 窗框高光）不该带品红/青。只在亮而且本来就接近灰的区域生效，
        // 深蓝天空彩度高，不会被一起洗灰。
        let chroma = out[0].max(out[1]).max(out[2]) - out[0].min(out[1]).min(out[2]);
        let w_high = ((lum - 0.60) / 0.40).clamp(0.0, 1.0)
            * (1.0 - (chroma / 0.18).clamp(0.0, 1.0))
            * 0.9;
        if w_high > 0.0 {
            for v in out.iter_mut() {
                *v += (lum - *v) * w_high;
            }
        }

        // 风格：蓝青系去青 + 提纯（天空、水面）。三色染料的天然串扰会把中密度
        // 处的蓝天推成"灰青"——实测参考正片天空 (0.49,0.61,0.85)，
        // 本软件是 (0.41,0.59,0.77)：R 低 0.08、B 低 0.08，G 差不多，
        // 于是 B-G 只剩一半，观感就是发青、发闷。
        // 修法是"去青"：把 R 拉向 B（补回橙红的分离度），同时把 B 推向饱和。
        if self.look.sky > 1e-4 {
            let s = self.look.sky;
            // 只认"蓝主导且够亮"：阴影与深蓝天顶里的蓝已经过量，
            // 再提纯只会推成荧光青。门限 0.42 是因为天空实测 lum ≈ 0.59。
            let lead_blue = ((out[2] - out[0]) / 0.15).clamp(0.0, 1.0);
            let wl = ((lum - 0.44) / 0.12).clamp(0.0, 1.0);
            let lead = lead_blue * wl;
            if lead > 0.0 {
                // `s` 直接就是"R 向 B 靠拢的比例"：s = 1 时蓝像素的 R 被提到与 B 同高。
                // B 的提升与 R 同强度（之前乘 1.3 是反解一张参考图得到的，属过拟合）：
                // 那会让已经很亮的深蓝天空被推向 1 之后溢出，变成荧光蓝。
                out[0] += (out[2] - out[0]) * lead * s;
                out[2] += (1.0 - out[2]) * lead * s;
            }
        }

        // 饱和度（围绕 luma）
        if self.saturation.abs() > 1e-4 {
            let luma = 0.2126 * out[0] + 0.7152 * out[1] + 0.0722 * out[2];
            for v in out.iter_mut() {
                *v = luma + (*v - luma) * (1.0 + self.saturation);
            }
        }

        // 曝光：显示域上的幂次偏移，0 与 1 固定（片基还是纯黑、纸白还是纸白）。
        //
        // 以前是把曝光按 `t^(2^e)` 叠在中调映射里：锚定后中调落在 t ≈ 0.17，
        // 2^e 一动 t 就被压掉一大截，|e| > 0.3 基本整片过曝/死黑，滑杆半个量程不可用。
        // 这里改成 ±2 档对应约 ±2 档、中间调渐进变化，端点不动。
        // 这里只留用户滑杆。风格的中调偏移已经在 `anchor_t` 上生效，
        // 再在这里叠一次绝对值等于把同一件事做两遍，且必然过冲。
        let e = self.exposure.clamp(-3.0, 3.0);
        if e.abs() > 1e-4 {
            // 0.8 让滑杆 ±2 大致等于 ±2 档（显示值 0.4 处约 1.9 倍），
            // 再大暗部会一片死黑、再小半个量程看不出变化。
            let g = 2.0f32.powf(-0.8 * e);
            for v in out.iter_mut() {
                *v = v.powf(g);
            }
        }

        // 风格色矩阵（行和 = 1：纸白/纸黑仍是端点，中性灰不变灰罩）
        let t = &self.look.tint;
        let identity = t[0][0] == 1.0
            && t[0][1] == 0.0
            && t[0][2] == 0.0
            && t[1][0] == 0.0
            && t[1][1] == 1.0
            && t[1][2] == 0.0
            && t[2][0] == 0.0
            && t[2][1] == 0.0
            && t[2][2] == 1.0;
        if !identity {
            out = mat3_vec3(t, out);
        }

        // 自动色调（Photoshop Auto Tone 口径）：放在最末端，因为它就是"看最终
        // 显示值"来定逐通道端点的；端点之外压成纯黑/纯白，所以片基与纸白不动。
        if !self.auto_tone.is_identity() {
            for c in 0..3 {
                out[c] = self.auto_tone.apply(out[c], c);
            }
        }

        for v in out.iter_mut() {
            *v = clampf(*v, 0.0, 1.0);
        }
        out
    }

    /// 自动中调对齐：让每层的中调都落到同一个 `target`（0..1，t 域）。
    ///
    /// 端点固定：alpha 是幂律指数，片基(t=1) 与最密处(t=0) 不受影响，
    /// 所以"自动色平衡"永远不会把片基弄灰。
    pub fn auto_balance(&self, median_dye: [f32; 3], target: f32) -> [f32; 3] {
        // 只用**同一个**幂次落位，不做逐通道 alpha 差异：
        // 场景平均色不是中性灰，逐通道对齐会把彩片（尤其大幅蓝天/草地）推成
        // 一片粉紫。色偏交给卷级增益 fit_gains / 用户 CMY，中调位置只用整体幂次。
        let tgt = target.clamp(0.05, 0.95).ln();
        let mut t_sum = 0.0f32;
        for j in 0..3 {
            let dref = self.d_ref[j].max(1e-3);
            t_sum += ((dref - median_dye[j]) / dref).clamp(0.02, 0.98).ln();
        }
        let a = (tgt / (t_sum / 3.0).max(-3.0)).clamp(0.5, 3.0);
        [a; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luma(c: [f32; 3]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    #[test]
    fn hd_curve_is_monotone_and_bounded() {
        let p = PrintParams::default();
        let mut prev = -1.0;
        for i in -300..=300 {
            let e = i as f32 / 100.0;
            let v = p.hd_curve(e);
            assert!((0.0..=1.0).contains(&v));
            assert!(v >= prev - 1e-4, "monotone at e={e}: {v} < {prev}");
            prev = v;
        }
        assert!(p.hd_curve(-5.0) < p.hd_curve(0.5));
        assert!(p.hd_curve(5.0) > p.hd_curve(0.5));
        assert_eq!(p.hd_curve(0.0), 0.0);
        assert_eq!(p.hd_curve(1.0), 1.0);
    }

    #[test]
    fn base_is_pure_black_under_any_exposure_and_grade() {
        // 片基 = 相对密度 0（以及被钳到 0 的漏光边）。
        // 无论曝光、反差、CMY 怎么调，都必须是一点杂色都没有的 (0,0,0)。
        for &exposure in &[-3.0f32, -1.5, -0.5, 0.0, 0.5, 1.5, 3.0] {
            for &grade in &[0.5f32, 1.0, 1.6, 2.4, 4.0] {
                for &cmy in &[-0.6f32, 0.0, 0.6] {
                    let mut p = PrintParams::default();
                    p.exposure = exposure;
                    p.grade = grade;
                    p.cmy = [cmy; 3];
                    p.wb = [0.7, 1.0, 1.25];
                    let out = p.apply([0.0, 0.0, 0.0]);
                    assert_eq!(out, [0.0, 0.0, 0.0], "exp={exposure} grade={grade} cmy={cmy}");
                    let out_neg = p.apply([-0.2, -0.2, -0.2]);
                    assert_eq!(out_neg, [0.0, 0.0, 0.0]);
                }
            }
        }
    }

    #[test]
    fn midtone_stays_mid_under_auto_balance() {
        let mut p = PrintParams::default();
        p.d_ref = [1.2, 1.2, 1.26];
        let med = [0.267, 0.543, 0.592];
        p.alpha = p.auto_balance(med, 0.35);
        let out = p.apply(med);
        let l = luma(out);
        assert!((0.25..0.75).contains(&l), "mid luma {l} {out:?}");
        // 片基仍然是纯黑
        assert_eq!(p.apply([0.0; 3]), [0.0, 0.0, 0.0]);
        // 最密处接近纸白
        let hi = p.apply([1.2, 1.2, 1.26]);
        assert!(luma(hi) > luma(out), "{:?}", out);
    }

    /// 三点锚定的典型参数（取自 `_DSC1566film.JPG` 的实测分布）
    fn anchored() -> PrintParams {
        let mut p = PrintParams::default();
        p.d_ref = [0.55, 0.855, 0.736];
        p.anchor_lo = [0.0, 0.0, 0.0];
        p.anchor_med = [0.207, 0.406, 0.292];
        p.anchor_hi = [0.55, 0.855, 0.736];
        p.anchor_t = [0.169, 0.185, 0.157];
        p
    }

    #[test]
    fn grade_pivots_at_mid_anchor() {
        // 反差滑杆只能改明暗两端的斜率，不能把中调位置推走：
        // pivot 原来在 t=0.5，锚定后中调在 0.17，一提反差中调就被顶到过曝。
        let mut p = anchored();
        let med = p.anchor_med;
        let r0 = p.apply(med);
        for &grade in &[0.4f32, 0.7, 1.0, 1.6, 2.2, 3.0] {
            p.grade = grade;
            let r = p.apply(med);
            for j in 0..3 {
                assert!(
                    (r[j] - r0[j]).abs() < 2e-4,
                    "grade={grade} 挪动了中调: {:?} vs {:?}",
                    r,
                    r0
                );
            }
            // 片基仍是纯黑、最密处仍是纸白
            assert_eq!(p.apply([0.0; 3]), [0.0, 0.0, 0.0]);
            assert!(luma(p.apply(p.anchor_hi)) > 0.95);
        }
    }

    #[test]
    fn exposure_offsets_on_top_of_anchors() {
        // 自动锚定生效时，曝光滑杆仍然是有效的偏移量（之前被强制清零，拖了没反应）
        let mut p = anchored();
        let med = p.anchor_med;
        let mut prev = -1.0;
        for &e in &[-1.5f32, -0.75, 0.0, 0.75, 1.5] {
            p.exposure = e;
            let l = luma(p.apply(med));
            assert!(l > prev, "曝光 {e} 没让中调变亮: {l} <= {prev}");
            prev = l;
            assert_eq!(p.apply([0.0; 3]), [0.0, 0.0, 0.0], "曝光 {e} 把片基弄灰了");
        }
        // 3 档以内的偏移必须落在可用的亮度区间，不是一片死白或死黑
        p.exposure = 1.5;
        assert!(luma(p.apply(med)) < 0.995);
        p.exposure = -1.5;
        assert!(luma(p.apply(med)) > 0.005);
    }

    #[test]
    fn denser_dye_is_brighter() {
        let p = PrintParams::default();
        let dark = luma(p.apply([0.2, 0.2, 0.2]));
        let mid = luma(p.apply([1.0, 1.0, 1.0]));
        let bright = luma(p.apply([2.0, 2.0, 2.0]));
        assert!(dark < mid && mid < bright, "{dark} {mid} {bright}");
    }
}
