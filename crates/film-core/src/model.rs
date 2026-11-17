//! 负片测量模型：片基归一 → 密度 → 解耦（染料串扰 + 色罩耦合 + 扫描串扰）。

use crate::numeric::{inv3, mat3_vec3, Mat3, EPS};

/// 测量矩阵 A（行 = 扫描/观测通道 R,G,B；列 = 染料层 C,M,Y）。
///
/// 对角线 1.0，非对角项同时吸收：染料有害吸收、成色剂色罩随密度耦合（B 项）、
/// 扫描光源光谱与 CFA 串扰。整卷联合求解会直接优化这个矩阵（见 `calib::fit_unmix`）。
pub fn default_matrix() -> Mat3 {
    [
        [1.06, 0.070, 0.020],
        [0.085, 1.03, 0.060],
        [0.020, 0.075, 0.985],
    ]
}

/// 逐段相对增益（补偿档案与实际胶片的行和差异），按平均密度分段线性插值。
#[derive(Clone, Debug)]
pub struct GainCurve {
    /// 段中心（平均相对密度）：趾 / 中 / 肩
    pub centers: [f32; 3],
    /// 每段三通道增益
    pub g: [[f32; 3]; 3],
}

impl Default for GainCurve {
    fn default() -> Self {
        Self {
            centers: [0.55, 1.15, 1.90],
            g: [[1.0; 3]; 3],
        }
    }
}

impl GainCurve {
    pub fn at(&self, mean_density: f32) -> [f32; 3] {
        let c = self.centers;
        if mean_density <= c[0] {
            return self.g[0];
        }
        if mean_density >= c[2] {
            return self.g[2];
        }
        if mean_density <= c[1] {
            let t = (mean_density - c[0]) / (c[1] - c[0]).max(EPS);
            return lerp3(self.g[0], self.g[1], t);
        }
        let t = (mean_density - c[1]) / (c[2] - c[1]).max(EPS);
        lerp3(self.g[1], self.g[2], t)
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// 一个"可求解"的负片模型。
#[derive(Clone, Debug)]
pub struct NegativeModel {
    /// 片基线性读数（逐通道）——归一化基准，等价于隐式扣除常量色罩 b0
    pub base: [f32; 3],
    /// 解耦矩阵 B = A^-1（密度 → 染料）
    pub unmix: Mat3,
    /// 相纸满密度锚点（逐层染料密度，对应"纯黑"曝光处）
    pub d_ref: [f32; 3],
    /// 逐段中性增益（色偏主因修正）
    pub gain: GainCurve,
}

impl NegativeModel {
    pub fn from_matrix(base: [f32; 3], a: &Mat3, d_ref: [f32; 3]) -> Option<Self> {
        Some(Self {
            base,
            unmix: inv3(a)?,
            d_ref,
            gain: GainCurve::default(),
        })
    }

    /// 线性透射率 → 相对密度（以片基为 0）。
    ///
    /// `D_c = log10(base_c / t_c)`，物理上等价于 `D_meas - b0`：
    /// 片基处 d = 0，故归一化自动扣除未曝光成色剂的橙色罩。
    #[inline]
    pub fn rel_densities(&self, px: [f32; 3]) -> [f32; 3] {
        [
            (self.base[0] / px[0].max(1e-7)).max(EPS).log10(),
            (self.base[1] / px[1].max(1e-7)).max(EPS).log10(),
            (self.base[2] / px[2].max(1e-7)).max(EPS).log10(),
        ]
    }

    /// 相对密度 → 三层染料密度：先乘逐段中性增益，再串扰解耦。
    #[inline]
    pub fn dye_from_rel(&self, d_rel: [f32; 3]) -> [f32; 3] {
        let mean = (d_rel[0] + d_rel[1] + d_rel[2]) / 3.0;
        let g = self.gain.at(mean);
        let scaled = [d_rel[0] * g[0], d_rel[1] * g[1], d_rel[2] * g[2]];
        let d = mat3_vec3(&self.unmix, scaled);
        [d[0].max(0.0), d[1].max(0.0), d[2].max(0.0)]
    }

    #[inline]
    pub fn dye_at(&self, px: [f32; 3]) -> [f32; 3] {
        self.dye_from_rel(self.rel_densities(px))
    }
}

/// 胶卷档案：决定解耦矩阵的先验与相纸锚点。
#[derive(Clone, Debug)]
pub struct StockProfile {
    pub name: String,
    pub a_prior: Mat3,
    pub d_ref: [f32; 3],
    /// 相对密度的上限估计（用于 UI 显示与自动曝光锚定）
    pub d_max_nominal: f32,
}

impl StockProfile {
    /// C-41 通用起点（Portra/ColorPlus 等通用化学，非某个具体批次）。
    pub fn c41_generic() -> Self {
        Self {
            name: "C-41 generic".into(),
            a_prior: default_matrix(),
            d_ref: [2.10, 2.25, 2.15],
            d_max_nominal: 2.6,
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            _ => Self::c41_generic(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_normalization_yields_zero_at_base() {
        let m = NegativeModel::from_matrix([1.0, 0.45, 0.25], &default_matrix(), [2.1, 2.2, 2.1])
            .unwrap();
        let d = m.rel_densities([1.0, 0.45, 0.25]);
        for c in 0..3 {
            assert!(d[c].abs() < 1e-5, "base should read 0 density: {d:?}");
        }
        let dye = m.dye_at([1.0, 0.45, 0.25]);
        assert!(dye.iter().all(|v| *v < 1e-5));
    }

    #[test]
    fn unmix_recovers_forward_model() {
        // 用 A 前向生成 D_rel，再用 A^-1 应还原染料密度
        let a = default_matrix();
        let m = NegativeModel::from_matrix([1.0, 1.0, 1.0], &a, [2.1, 2.2, 2.1]).unwrap();
        for d in [[0.4f32, 0.25, 0.15], [1.2, 0.9, 0.7], [0.05, 0.02, 0.01]] {
            let d_rel = crate::numeric::mat3_vec3(&a, d);
            let back = m.dye_from_rel(d_rel);
            for c in 0..3 {
                assert!((back[c] - d[c]).abs() < 1e-4, "{back:?} vs {d:?}");
            }
        }
    }
}
