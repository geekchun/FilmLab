//! 合成负片：用前向物理模型生成"带片基边框 + 染料串扰 + 彩色偶联剂色罩"的测试图，
//! 同时给出对应的参考正片。用于可辨识性实验与回归测试（不依赖真实底片）。

use crate::image::FloatImage;
use crate::model::default_matrix;
use crate::numeric::{mat3_vec3, Mat3};
use crate::print::PrintParams;

/// 典型 C-41 片基线性透过率（红高、蓝低 = 橙色）
pub const BASE_TRANSMITTANCE: [f32; 3] = [0.30, 0.18, 0.09];

pub struct SynthOptions {
    pub width: usize,
    pub height: usize,
    /// 片基边框宽度（像素）
    pub rebate: usize,
    /// 测量矩阵（模拟染料串扰）
    pub a: Mat3,
    /// 读数噪声（相对满量程）
    pub noise: f32,
    pub seed: u64,
}

impl Default for SynthOptions {
    fn default() -> Self {
        Self {
            width: 900,
            height: 600,
            rebate: 24,
            a: default_matrix(),
            noise: 1.0 / 8192.0,
            seed: 0x5EED_1234,
        }
    }
}

struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32) / ((1u64 << 31) as f32)
    }
}

/// 生成 (负片线性图, 参考正片显示图) 与真实染料密度（供断言使用）。
pub fn synthetic_negative(opts: &SynthOptions) -> (FloatImage, FloatImage, Vec<[f32; 3]>) {
    let pp = PrintParams::default();
    let range = pp.paper_dmax - pp.paper_dmin;
    let d_ref = pp.d_ref;
    // 前向使用档案矩阵原始行和，验证"增益可由中性样本辨识"
    let a = opts.a;
    let b0 = [
        -(BASE_TRANSMITTANCE[0]).log10(),
        -(BASE_TRANSMITTANCE[1]).log10(),
        -(BASE_TRANSMITTANCE[2]).log10(),
    ];

    let w = opts.width;
    let h = opts.height;
    let mut neg = FloatImage::new(w, h);
    let mut pos = FloatImage::new(w, h);
    let mut truth: Vec<[f32; 3]> = Vec::with_capacity(w * h);
    let mut rng = Lcg(opts.seed);

    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let in_rebate = x < opts.rebate
                || y < opts.rebate
                || x + opts.rebate >= w
                || y + opts.rebate >= h;

            let (d, exposure_e): ([f32; 3], [f32; 3]) = if in_rebate {
                // 片基区：d = 0（无染料），只有色罩
                ([0.0f32; 3], [0.0f32; 3])
            } else {
                let (d, e) = scene_at(x as f32 / w as f32, y as f32 / h as f32, d_ref, range);
                (d, e)
            };

            // 前向：测量密度 = A·d + b0，加噪声，回到透过率
            let d_meas = mat3_vec3(&a, d);
            let mut r = [0.0f32; 3];
            for c in 0..3 {
                let dm = d_meas[c] + b0[c] + opts.noise * (rng.next_f32() - 0.5);
                r[c] = 10.0f32.powf(-dm).clamp(0.0, 1.0);
            }
            neg.set_pixel(i, r);

            // 参考正片（无噪声的理想印相）
            let out = if in_rebate { [0.0; 3] } else { pp.apply(d) };
            pos.set_pixel(i, out);
            truth.push(d);
            let _ = exposure_e;
        }
    }

    (neg, pos, truth)
}

/// 场景：中性灰阶带 + 彩色补丁 + 自然渐变场；返回染料密度与纸面曝光量。
fn scene_at(u: f32, v: f32, d_ref: [f32; 3], range: f32) -> ([f32; 3], [f32; 3]) {
    // 底部 18%：中性灰阶
    // 其余：4×3 彩色补丁（上方）+ 平滑渐变场
    let e: [f32; 3];

    if v > 0.82 {
        let t = (u * 1.02).clamp(0.0, 1.0);
        // 灰阶：三层曝光一致 → 中性
        e = [0.10 + 0.85 * t, 0.10 + 0.85 * t, 0.10 + 0.85 * t];
    } else {
        // 补丁区（v ∈ 0.30..0.78，u 分 6 列）
        let patch_v = (0.30..0.78).contains(&v);
        if patch_v {
            let col = ((u * 6.0) as usize).min(5);
            let row = (((v - 0.30) / 0.16) as usize).min(2);
            let hue = col as f32 / 6.0;
            let sat = match row {
                0 => 0.85,
                1 => 0.45,
                _ => 0.12,
            };
            e = hsv_to_exposure(hue, sat, 0.55 + 0.2 * (1.0 - v));
        } else {
            // 渐变场：模拟风景（上方天空偏冷，下方大地偏暖）
            let sky = (1.0 - v * 1.6).clamp(0.0, 1.0);
            let hue = 0.55 - 0.45 * (v * 1.2).clamp(0.0, 1.0) + 0.03 * (u * 6.28).sin();
            let sat = 0.25 + 0.2 * sky;
            let val = 0.25 + 0.55 * (1.0 - (v - 0.55).abs() * 1.4).clamp(0.0, 1.0);
            e = hsv_to_exposure(hue.rem_euclid(1.0), sat, val);
        }
    }

    // 场景亮度 → 负片染料密度：越亮 → 曝光越多 → 染料越密（负片是"负"的）
    let mut d = [0.0f32; 3];
    for j in 0..3 {
        d[j] = (e[j] * range * 0.95).clamp(0.02, d_ref[j]);
    }
    (d, e)
}

fn hsv_to_exposure(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = h * 6.0;
    let c = v * s;
    let x = c * (1.0 - ((h6 % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h6 as usize {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NegativeModel;

    #[test]
    fn forward_then_inverse_recovers_dye_densities() {
        let opts = SynthOptions {
            width: 240,
            height: 160,
            rebate: 12,
            noise: 0.0,
            ..Default::default()
        };
        let (neg, _pos, truth) = synthetic_negative(&opts);
        let base = BASE_TRANSMITTANCE;
        let model = NegativeModel::from_matrix(base, &opts.a, PrintParams::default().d_ref).unwrap();

        let mut max_err = 0.0f32;
        // 只看画面内部（排除边框），且避开极低密度
        for y in (opts.rebate + 2)..(opts.height - opts.rebate - 2) {
            for x in (opts.rebate + 2)..(opts.width - opts.rebate - 2) {
                let i = y * opts.width + x;
                let d = model.dye_at(neg.pixel(i));
                for c in 0..3 {
                    max_err = max_err.max((d[c] - truth[i][c]).abs());
                }
            }
        }
        assert!(max_err < 2e-3, "max dye error {max_err}");
    }
}
