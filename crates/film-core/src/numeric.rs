//! 小型数值工具：3×3 密度矩阵求逆、小规模线性方程组、软限幅。

pub type Mat3 = [[f32; 3]; 3];

pub const EPS: f32 = 1e-6;

#[inline]
pub fn clampf(x: f32, lo: f32, hi: f32) -> f32 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
}

/// 平滑下界：值低于 lo 时被软压到 lo（C∞ 连续，导数连续）。
#[inline]
pub fn softplus_lower(x: f32, lo: f32, knee: f32) -> f32 {
    let a = knee.max(EPS);
    let v = (x - lo) / a;
    // log(exp(v)+1) 的稳定实现
    let sp = if v > 20.0 {
        v
    } else if v < -20.0 {
        v.exp()
    } else {
        v.exp().ln_1p()
    };
    lo + a * sp
}

#[inline]
pub fn det3(m: &Mat3) -> f32 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// 3×3 求逆（伴随矩阵法）。奇异时（|det| < 1e-9）返回 None。
pub fn inv3(m: &Mat3) -> Option<Mat3> {
    let d = det3(m);
    if d.abs() < 1e-9 || !d.is_finite() {
        return None;
    }
    let id = 1.0 / d;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * id,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * id,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * id,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * id,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * id,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * id,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * id,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * id,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * id,
        ],
    ])
}

#[inline]
pub fn mat3_vec3(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub fn mat3_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0f32; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    out
}

/// 行归一化：每行按行和缩放，保持"等密度输入 → 等染料密度"语义。
pub fn normalize_rows(m: &Mat3) -> Mat3 {
    let mut out = *m;
    for row in out.iter_mut() {
        let s: f32 = row.iter().sum();
        if s.abs() > EPS {
            for v in row.iter_mut() {
                *v /= s;
            }
        }
    }
    out
}

/// 高斯消元（部分主元）求解 n×n 线性方程组 a·x = b，n ≤ 8。
/// 返回 None 表示奇异。
pub fn solve_linear<const N: usize>(mut a: [[f32; N]; N], mut b: [f32; N]) -> Option<[f32; N]> {
    for col in 0..N {
        // 主元选择
        let mut piv = col;
        for r in (col + 1)..N {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);

        let p = a[col][col];
        for c in col..N {
            a[col][c] /= p;
        }
        b[col] /= p;

        for r in 0..N {
            if r == col {
                continue;
            }
            let f = a[r][col];
            if f.abs() < 1e-20 {
                continue;
            }
            for c in col..N {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv3_roundtrip() {
        let m: Mat3 = [[1.2, 0.08, 0.05], [0.09, 1.1, 0.14], [0.03, 0.11, 1.05]];
        let inv = inv3(&m).unwrap();
        let prod = mat3_mul(&m, &inv);
        for (i, row) in prod.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                let expect = if i == j { 1.0 } else { 0.0 };
                assert!((v - expect).abs() < 1e-5, "m*inv[{i}][{j}] = {v}");
            }
        }
    }

    #[test]
    fn solve_small() {
        let a = [[2.0f32, 1.0], [1.0, 3.0]];
        let x = solve_linear(a, [3.0, 5.0]).unwrap();
        assert!((x[0] - 0.8).abs() < 1e-5);
        assert!((x[1] - 1.4).abs() < 1e-5);
    }

    #[test]
    fn softplus_lower_behaves_as_soft_max() {
        let lo = 0.06;
        for k in [0.02f32, 0.05, 0.2] {
            // 严格不小于 lo
            for x in [-1.0f32, -0.2, 0.0, 0.06, 0.5, 2.0] {
                let v = softplus_lower(x, lo, k);
                assert!(v >= lo - 1e-6, "k={k} x={x} v={v}");
            }
            // 远离数学下界后应逼近 x
            let far = softplus_lower(3.0, lo, k);
            assert!((far - 3.0).abs() < 1e-3, "k={k} far={far}");
            // 单调
            let a = softplus_lower(0.5, lo, k);
            let b = softplus_lower(0.7, lo, k);
            assert!(b > a, "k={k} a={a} b={b}");
        }
    }
}
